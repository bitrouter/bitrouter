//! Full retained subjects are checked independently of results and projections.
use super::*;
use bitrouter_ai::{
    eval::EvalSubjectIdentity,
    tenancy::identity::{InstallationId, TenantId},
};

/// Read original rows under the caller's stable transaction, without mutation.
/// One encoded row and its canonical decode share the supplied working allowance.
pub(crate) async fn validate_retained_subjects_in(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    bytes: usize,
    check: &(dyn Fn() -> Result<()> + Sync),
) -> Result<()> {
    let mut after = None::<String>;
    loop {
        check()?;
        let mut query = subject_entity::Entity::find().order_by_asc(subject_entity::Column::EvalId);
        if let Some(after) = &after {
            query = query.filter(subject_entity::Column::EvalId.gt(after.as_str()));
        }
        // SQL counts every cell before transfer. No page accumulates bodies,
        // and the remaining allowance covers JSON/canonical decoder scratch.
        let row = io::one(db, query, &mut Budget::retained(bytes / 8)?).await?;
        check()?;
        let Some(row) = row else { return Ok(()) };
        let scope = match (
            &row.installation_id,
            &row.tenant_id,
            &row.creator_principal_id,
        ) {
            // Current gateway records share this DB and retain unscoped keys.
            // The canonical decoder still requires the current subject schema.
            (None, None, None) => Scope::legacy(),
            (Some(actual), Some(tenant), Some(_)) => {
                ensure!(
                    actual.as_str() == installation.as_str(),
                    "retained Eval subject installation mismatch"
                );
                let tenant: TenantId = tenant.as_str().try_into()?;
                Scope::for_retained_eval(installation.clone(), tenant)
            }
            _ => anyhow::bail!("mixed retained Eval subject ownership"),
        };
        let subject = codec::Decoder { scope: &scope }.decode_subject(&row)?;
        match scope.snapshot_namespace() {
            None => ensure!(
                matches!(subject.identity, EvalSubjectIdentity::GatewayGeneric {}),
                "global retained Eval subject requires gateway_generic profile"
            ),
            Some((installation, tenant)) => ensure!(
                subject.identity.tenant_namespace().is_some_and(
                    |(actual_installation, actual_tenant)| actual_installation.as_str()
                        == installation
                        && actual_tenant.as_str() == tenant
                ),
                "retained Eval subject profile or namespace mismatch"
            ),
        }
        after = Some(row.eval_id);
    }
}
