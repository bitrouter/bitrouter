//! Borrowed inspection only. No import, compilation write, epoch pin or authority.
use super::{
    manifest::{Definition, Manifest, bounded_bytes},
    *,
};
use crate::{offline::recovery::plan::Provisioning, storage::operation_budget::OperationBudget};
use anyhow::Context;

impl Resources {
    pub(in crate::tenancy) async fn inspect_fresh_static(
        &self,
        db: &impl ConnectionTrait,
        approved: &[u8],
        observed: &Provisioning,
        budget: &OperationBudget<'_>,
        check_creator: &(
             dyn Fn(&super::super::configuration_store::StoredConfiguration) -> Result<()> + Sync
         ),
    ) -> Result<()> {
        budget.charge(0, approved.len())?;
        ensure!(
            approved.len() <= MAX_MANIFEST_BYTES,
            "recovery manifest bound"
        );
        let manifest: Manifest = serde_json::from_slice(approved)?;
        ensure!(
            manifest.format == 1
                && manifest.installation_id == self.installation
                && manifest.resources.len() <= MAX_RESOURCES,
            "approved static manifest identity"
        );
        let references = storage::references(db).await?;
        let actual: HashSet<_> = references.iter().collect();
        let supplied: HashSet<_> = manifest.resources.iter().map(|r| &r.reference).collect();
        let expected: HashSet<_> = observed.resources.iter().collect();
        ensure!(
            actual.len() == references.len()
                && supplied.len() == manifest.resources.len()
                && actual == supplied
                && actual == expected,
            "approved resource coverage mismatch"
        );
        for resource in &manifest.resources {
            budget.check()?;
            resource.validate()?;
            match &resource.definition {
                Definition::Route {
                    core_choice: None,
                    block: None,
                    assessment: None,
                    exploration,
                    ..
                } => ensure!(
                    exploration.is_empty(),
                    "recovery first profile is fixed static routing"
                ),
                Definition::Policy { .. } => {}
                Definition::Credential {
                    credential:
                        crate::gateway::providers::auth::credentials::Credential::ApiKey { .. },
                    oauth: None,
                    oauth_generation: 0,
                } => {}
                _ => anyhow::bail!("unsupported recovery resource generation/profile"),
            }
            // Bound the storage projection and original authenticated decoder's
            // temporary copies BEFORE loading ciphertext. Same operation budget.
            budget.charge(1, 3 * MAX_ARTIFACT_BYTES)?;
            let stored = super::super::configuration_store::resource(db, &resource.reference)
                .await?
                .context("approved retained resource missing")?;
            let artifact = storage::authenticate(self, stored)?;
            ensure!(
                bounded_bytes(&artifact.definition, MAX_ARTIFACT_BYTES)?
                    == bounded_bytes(&resource.definition, MAX_ARTIFACT_BYTES)?,
                "approved static definition changed"
            );
            ensure!(
                artifact
                    .definition
                    .dependencies()
                    .iter()
                    .all(|r| actual.contains(r)),
                "approved dependency missing"
            );
        }
        let key = self.key.as_ref().context("target key missing")?;
        let epoch = db
            .query_all(sea_orm::Statement::from_string(
                db.get_database_backend(),
                "SELECT singleton,installation_id,key_id FROM tenancy_continuation_epoch LIMIT 2",
            ))
            .await?;
        ensure!(
            epoch.len() == 1
                && epoch[0].try_get::<i32>("", "singleton")? == 1
                && epoch[0].try_get::<String>("", "installation_id")? == self.installation.as_str()
                && epoch[0].try_get::<String>("", "key_id")? == key.id(),
            "target key epoch mismatch"
        );
        let mut reached = HashSet::new();
        for expected in &observed.configurations {
            budget.check()?;
            // The original configuration reader caps the stored envelope. Its
            // full reserved decode cost is charged before any payload transfer.
            budget.charge(
                1,
                super::super::configuration_store::MAX_ENVELOPE_BYTES + 2 * MAX_ARTIFACT_BYTES,
            )?;
            let stored = super::super::configuration_store::configuration(
                db,
                &expected.tenant_id,
                &expected.revision_id,
            )
            .await?
            .context("approved configuration missing")?;
            check_creator(&stored)?;
            ensure!(
                serde_json::to_value(&stored.revision)? == serde_json::to_value(expected)?,
                "configuration readback changed"
            );
            ensure!(
                matches!(
                    stored.revision.document.learning.mode,
                    bitrouter_ai::tenancy::configuration::LearningMode::Off
                ) && stored.revision.document.learning.max_evaluations == 0,
                "recovery first profile excludes automatic learning"
            );
            let validated = validation::compiled_with_operation(self, &stored, Some(budget))?;
            validated
                .require_capabilities()
                .map_err(|_| anyhow::anyhow!("target profile unavailable in this binary"))?;
            for dependency in stored.dependencies {
                ensure!(
                    actual.contains(&dependency),
                    "configuration dependency not retained"
                );
                reached.insert(dependency);
            }
        }
        ensure!(
            reached.len() == actual.len() && reached.iter().all(|r| actual.contains(r)),
            "unreferenced static resource outside approved closure"
        );
        budget.check()?;
        Ok(())
    }
}
