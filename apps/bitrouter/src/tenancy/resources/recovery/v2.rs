//! Per-object authenticated checks for streamed offline recovery.

use super::super::{
    manifest::{Definition, Import, bounded_bytes},
    *,
};
use crate::offline::recovery::v2::memory;
use crate::tenancy::configuration_store::{self, StoredConfiguration};
use anyhow::Context;

#[cfg(test)]
mod tests;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Tag {
    Resource,
}

/// A strict independent approval record. Definitions remain private to the
/// original resource decoder rather than becoming a new public import DTO.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApprovedResource {
    kind: Tag,
    pub(crate) reference: ResourceRef,
    definition: Definition,
}

impl Resources {
    /// Authenticate one retained definition under its original key and metadata.
    /// The caller reserves driver, decrypted and decoded copies before loading it.
    pub(in crate::tenancy) async fn inspect_retained_resource_in(
        &self,
        db: &impl ConnectionTrait,
        reference: &ResourceRef,
        bytes: usize,
    ) -> Result<()> {
        self.resource_preflight(db, reference, bytes).await?;
        let stored = configuration_store::resource(db, reference)
            .await?
            .context("retained resource missing")?;
        self.plaintext_preflight(&stored, bytes)?;
        storage::authenticate(self, stored)?;
        Ok(())
    }

    pub(in crate::tenancy) async fn inspect_all_retained_in(
        &self,
        db: &impl ConnectionTrait,
        bytes: usize,
        check: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<()> {
        let mut after = None;
        loop {
            check()?;
            let page = storage::reference_page(db, after.as_ref()).await?;
            if page.is_empty() {
                break;
            }
            for reference in page {
                check()?;
                self.inspect_retained_resource_in(db, &reference, bytes)
                    .await?;
                after = Some(reference);
            }
        }
        Ok(())
    }

    async fn resource_preflight(
        &self,
        db: &impl ConnectionTrait,
        reference: &ResourceRef,
        bytes: usize,
    ) -> Result<()> {
        let owner = serde_json::to_value(reference.owner_kind)?;
        let kind = serde_json::to_value(reference.resource_kind)?;
        memory::preflight(
            db,
            "tenancy_resource_revisions",
            &[
                ("owner_kind", owner.as_str().context("owner kind")?),
                ("owner_id", reference.owner_id.as_str()),
                ("resource_kind", kind.as_str().context("resource kind")?),
                ("resource_id", reference.resource_id.as_str()),
                ("revision_id", reference.revision_id.as_str()),
            ],
            &["payload_json", "dependency_json"],
            bytes,
        )
        .await
    }

    fn plaintext_preflight(
        &self,
        record: &configuration_store::StoredResource,
        bytes: usize,
    ) -> Result<()> {
        let dependencies: Vec<ResourceRef> = serde_json::from_value(record.dependencies.clone())?;
        let binding = (
            &self.installation,
            &record.reference,
            record.compiler_version.as_str(),
            &dependencies,
        );
        let sealed: crate::runtime_crypto::SealedSecret =
            serde_json::from_value(record.payload.clone())?;
        let purpose = if record.compiler_version == "tenancy-oauth-resource-v1" {
            "owned-oauth-seed-v1"
        } else {
            "resource"
        };
        let plaintext = self
            .key
            .as_ref()
            .context("resource key missing")?
            .open_bound(purpose, &binding, &sealed)?;
        let stored_bytes = memory::json_required(&bounded_bytes(
            &record.payload,
            configuration_store::MAX_ENVELOPE_BYTES,
        )?)?
        .checked_add(memory::json_required(&bounded_bytes(
            &record.dependencies,
            configuration_store::MAX_ENVELOPE_BYTES,
        )?)?)
        .context("resource memory overflow")?;
        ensure!(
            stored_bytes
                .checked_add(memory::json_required(&plaintext)?)
                .is_some_and(|needed| needed <= bytes),
            crate::offline::recovery::v2::streams::WorkingCapacity
        );
        Ok(())
    }

    /// Read-only key-epoch check; never creates an epoch or infers a key.
    pub(in crate::tenancy) async fn inspect_key_epoch_in(
        &self,
        db: &impl ConnectionTrait,
    ) -> Result<()> {
        let key = self.key.as_ref().context("target key missing")?;
        let rows = db
            .query_all(sea_orm::Statement::from_string(
                db.get_database_backend(),
                "SELECT CASE WHEN typeof(singleton)='integer' AND singleton=1 THEN 1 ELSE NULL END AS singleton, CASE WHEN typeof(installation_id)='text' AND length(CAST(installation_id AS BLOB)) BETWEEN 1 AND 128 THEN installation_id ELSE NULL END AS installation_id, CASE WHEN typeof(key_id)='text' AND length(CAST(key_id AS BLOB))=64 THEN key_id ELSE NULL END AS key_id FROM tenancy_continuation_epoch LIMIT 2",
            ))
            .await?;
        ensure!(
            rows.len() == 1
                && rows[0].try_get::<i32>("", "singleton")? == 1
                && rows[0].try_get::<String>("", "installation_id")? == self.installation.as_str()
                && rows[0].try_get::<String>("", "key_id")? == key.id(),
            "target key epoch mismatch"
        );
        Ok(())
    }

    /// Fresh-target static restrictions are deliberately separate from restore.
    pub(in crate::tenancy) async fn inspect_approved_resource_in(
        &self,
        db: &impl ConnectionTrait,
        approved: ApprovedResource,
        bytes: usize,
        check: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<()> {
        check()?;
        let import = Import {
            reference: approved.reference,
            definition: approved.definition,
        };
        import.validate()?;
        match &import.definition {
            Definition::Route {
                core_choice: None,
                block: None,
                assessment: None,
                exploration,
                ..
            } => ensure!(
                exploration.is_empty(),
                "fresh recovery requires fixed static routing"
            ),
            Definition::Policy { .. } => {}
            Definition::Credential {
                credential: crate::gateway::providers::auth::credentials::Credential::ApiKey { .. },
                oauth: None,
                oauth_generation: 0,
            } => {}
            _ => anyhow::bail!("unsupported fresh recovery generation/profile"),
        }
        self.resource_preflight(db, &import.reference, bytes)
            .await?;
        let stored = configuration_store::resource(db, &import.reference)
            .await?
            .context("approved resource missing")?;
        self.plaintext_preflight(&stored, bytes)?;
        let artifact = storage::authenticate(self, stored)?;
        ensure!(
            bounded_bytes(&artifact.definition, MAX_ARTIFACT_BYTES)?
                == bounded_bytes(&import.definition, MAX_ARTIFACT_BYTES)?,
            "approved resource definition changed"
        );
        // Full catalog/approval coverage is proved by the caller. Each edge is
        // an exact original-store lookup, not an in-memory catalog membership set.
        for dependency in artifact.definition.dependencies() {
            check()?;
            ensure!(
                resource_exists(db, &dependency).await?,
                "approved dependency missing"
            );
        }
        check()
    }

    /// Validate one fully loaded compiler envelope; retain no cross-record graph.
    pub(in crate::tenancy) fn inspect_static_configuration(
        &self,
        stored: &StoredConfiguration,
        bytes: usize,
    ) -> Result<()> {
        ensure!(
            matches!(
                stored.revision.document.learning.mode,
                bitrouter_ai::tenancy::configuration::LearningMode::Off
            ) && stored.revision.document.learning.max_evaluations == 0,
            "fresh recovery excludes automatic learning"
        );
        let mut required =
            memory::json_required(&bounded_bytes(&stored.compiled, 2 * MAX_ARTIFACT_BYTES)?)?
                .checked_add(memory::json_required(&bounded_bytes(
                    &stored.revision.document,
                    MAX_ARTIFACT_BYTES,
                )?)?)
                .context("configuration working size overflow")?;
        if let Some(sealed) = stored.compiled.get("sealed") {
            let sealed: crate::runtime_crypto::SealedSecret =
                serde_json::from_value(sealed.clone())?;
            let binding = (
                &self.installation,
                &stored.revision.tenant_id,
                &stored.revision.revision_id,
                COMPILER_VERSION,
                &stored.revision.document,
                &stored.dependencies,
            );
            let plaintext = self
                .key
                .as_ref()
                .context("configuration key missing")?
                .open_bound("configuration", &binding, &sealed)?;
            required = required
                .checked_add(memory::json_required(&plaintext)?)
                .context("configuration plaintext size overflow")?;
        }
        ensure!(
            required <= bytes,
            crate::offline::recovery::v2::streams::WorkingCapacity
        );
        validation::compiled(self, stored)?
            .require_capabilities()
            .map_err(|_| anyhow::anyhow!("target profile unavailable in this binary"))?;
        Ok(())
    }
}

async fn resource_exists(db: &impl ConnectionTrait, reference: &ResourceRef) -> Result<bool> {
    let owner = serde_json::to_value(reference.owner_kind)?;
    let kind = serde_json::to_value(reference.resource_kind)?;
    Ok(db.query_one(sea_orm::Statement::from_sql_and_values(db.get_database_backend(), "SELECT 1 FROM tenancy_resource_revisions WHERE owner_kind=? AND owner_id=? AND resource_kind=? AND resource_id=? AND revision_id=? LIMIT 1", [owner.as_str().context("owner kind")?.into(), reference.owner_id.as_str().into(), kind.as_str().context("resource kind")?.into(), reference.resource_id.as_str().into(), reference.revision_id.as_str().into()])).await?.is_some())
}
