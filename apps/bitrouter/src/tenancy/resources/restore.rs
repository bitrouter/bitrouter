//! Bounded authentication of stored credentials and configuration envelopes.

use super::{manifest::bounded_bytes, *};
use crate::{offline::memory, tenancy::configuration_store};
use anyhow::Context;

#[cfg(test)]
mod tests;

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
        self.inspect_all_configurations_in(db, bytes, check).await?;
        Ok(())
    }

    /// Integrity-only authentication of every retained compiler envelope. Capability
    /// qualification belongs to activation or the explicit current BRO owner.
    pub(in crate::tenancy) async fn inspect_all_configurations_in(
        &self,
        db: &impl ConnectionTrait,
        bytes: usize,
        check: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<()> {
        let mut after = None;
        loop {
            check()?;
            let page = configuration_store::retained_configuration_page(db, after.as_ref()).await?;
            if page.is_empty() {
                break;
            }
            for (tenant, revision) in page {
                check()?;
                memory::preflight(
                    db,
                    "tenancy_configurations",
                    &[
                        ("tenant_id", tenant.as_str()),
                        ("revision_id", revision.as_str()),
                    ],
                    &["document_json", "compiled_json", "dependencies_json"],
                    bytes / 4,
                )
                .await?;
                let stored = configuration_store::configuration(db, &tenant, &revision)
                    .await?
                    .context("retained configuration disappeared during authentication")?;
                let checked = || check().map_err(|_| ErrorCode::AuthorityUnavailable);
                let budget = crate::storage::operation_budget::OperationBudget::with_limits(
                    &checked, 65536, bytes,
                );
                validation::compiled_with_operation(self, &stored, Some(&budget))?;
                after = Some((tenant, revision));
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
            crate::offline::large::CapacityExceeded
        );
        Ok(())
    }

    /// Read-only key-epoch check; never creates an epoch or infers a key.
    pub(in crate::tenancy) async fn inspect_key_epoch_in(
        &self,
        db: &impl ConnectionTrait,
    ) -> Result<()> {
        let key = self.key.as_ref().context("restore key missing")?;
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
            "restore key epoch mismatch"
        );
        Ok(())
    }
}
