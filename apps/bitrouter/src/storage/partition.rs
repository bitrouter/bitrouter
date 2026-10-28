//! Secret-free physical key derivation. A key is not a namespace or execution grant.
use anyhow::Result;
use bitrouter_ai::tenancy::identity::{InstallationId, TenantId};
use sha2::{Digest, Sha256};

#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) fn learner_schedule(
    installation: &InstallationId,
    tenant: &TenantId,
    id: &str,
) -> Result<String> {
    tenant_key(installation, tenant, "owned.learner.schedule.v1", &[id])
}

pub(super) const LEARNING_PENDING: &str = "learning.pending";
pub(super) const NATIVE_EXPERIMENT_PENDING: &str = "native.experiment.pending";

/// Pending native experiment links remain a readiness fence in reduced builds.
pub(crate) fn native_experiment_pending(
    installation: &InstallationId,
    tenant: &TenantId,
) -> Result<String> {
    tenant_key(installation, tenant, NATIVE_EXPERIMENT_PENDING, &[])
}

/// Common readiness may inspect this partition even without Core or Harness.
pub(crate) fn learning_pending(installation: &InstallationId, tenant: &TenantId) -> Result<String> {
    tenant_key(installation, tenant, LEARNING_PENDING, &[])
}

// Same v1 framing as Scope::physical_key; Legacy bypasses this helper entirely.
pub(super) fn tenant_key(
    installation: &InstallationId,
    tenant: &TenantId,
    domain: &str,
    logical_parts: &[&str],
) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(b"bitrouter.store.key\0\x01");
    for part in [domain, installation.as_str(), tenant.as_str()] {
        frame(&mut digest, part)?;
    }
    digest.update(u32::try_from(logical_parts.len())?.to_be_bytes());
    for part in logical_parts {
        frame(&mut digest, part)?;
    }
    Ok(hex::encode(digest.finalize()))
}

fn frame(digest: &mut Sha256, part: &str) -> Result<()> {
    digest.update(u32::try_from(part.len())?.to_be_bytes());
    digest.update(part.as_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_pending_partition_preserves_v1_literal_and_identity_framing() -> Result<()> {
        let installation: InstallationId = "installation".try_into()?;
        let tenant: TenantId = "tenant".try_into()?;
        assert_eq!(
            learning_pending(&installation, &tenant)?,
            "44cfc65d204e61ebc2d3d3af627011ed6ebad4b7a43b81d1d976cabbf28299f4"
        );
        assert_eq!(
            tenant_key(&installation, &tenant, "eval.subject", &["logical"])?,
            "628cef0903080bc46e65b72bdc091f6d5cc51e17468c4bce830d73db3c6ab0e9"
        );
        assert_ne!(
            learning_pending(&"a".try_into()?, &"bc".try_into()?)?,
            learning_pending(&"ab".try_into()?, &"c".try_into()?)?
        );
        assert_ne!(
            learning_pending(&installation, &tenant)?,
            learning_pending(&installation, &"Tenant".try_into()?)?
        );
        Ok(())
    }

    #[cfg(all(feature = "core", feature = "harness"))]
    #[test]
    fn full_scope_pending_key_matches_common_readiness_partition() -> Result<()> {
        use crate::storage::scope::{KeyDomain, Scope};
        let installation: InstallationId = "installation".try_into()?;
        let tenant: TenantId = "tenant".try_into()?;
        let (scope, _) =
            Scope::for_test(installation.clone(), tenant.clone(), "creator".try_into()?);
        assert_eq!(
            scope.physical_key(KeyDomain::LearningPending, "unused Legacy spelling", &[])?,
            learning_pending(&installation, &tenant)?
        );
        assert_eq!(
            scope.physical_key(KeyDomain::EvalSubject, "unused", &["logical"])?,
            "628cef0903080bc46e65b72bdc091f6d5cc51e17468c4bce830d73db3c6ab0e9"
        );
        Ok(())
    }
}
