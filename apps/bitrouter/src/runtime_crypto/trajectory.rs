//! Narrow correlation-key derivation, separate from continuation capabilities.
use super::{DeploymentKey, Hmac, KeyInit, Mac, Sha256};
use anyhow::{Context, Result};
use bitrouter_ai::tenancy::{
    configuration::ResourceRef,
    identity::{InstallationId, RevisionId, TenantId},
};

pub(crate) struct TrajectoryCrypto {
    secret: [u8; 32],
}
impl DeploymentKey {
    pub(crate) fn trajectory(&self) -> Result<TrajectoryCrypto> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret)
            .map_err(|_| anyhow::anyhow!("trajectory key unavailable"))?;
        // HMAC domain is distinct from wrapping fingerprints and continuation tags.
        // https://www.rfc-editor.org/rfc/rfc2104
        mac.update(b"bitrouter.tenancy.trajectory.root.v1\0");
        Ok(TrajectoryCrypto {
            secret: mac.finalize().into_bytes().into(),
        })
    }
}
impl TrajectoryCrypto {
    pub(crate) fn correlation_key(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        revision: &RevisionId,
        route: &ResourceRef,
        policy: &ResourceRef,
    ) -> Result<crate::trajectory::canonical::CorrelationKey> {
        let route = serde_json::to_vec(route)?;
        let policy = serde_json::to_vec(policy)?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret)
            .map_err(|_| anyhow::anyhow!("trajectory key unavailable"))?;
        mac.update(b"bitrouter.tenancy.trajectory.partition.v1\0");
        for part in [
            installation.as_str().as_bytes(),
            tenant.as_str().as_bytes(),
            revision.as_str().as_bytes(),
            &route,
            &policy,
        ] {
            mac.update(
                &u64::try_from(part.len())
                    .context("trajectory identity length overflow")?
                    .to_be_bytes(),
            );
            mac.update(part);
        }
        crate::trajectory::canonical::CorrelationKey::from_bytes(mac.finalize().into_bytes().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_ai::tenancy::authority::{OwnerKind, ResourceKind};
    #[test]
    fn correlation_domains_bind_exact_namespace_and_profile_but_no_creator() -> Result<()> {
        let installation: InstallationId = "installation".try_into()?;
        let tenant: TenantId = "tenant".try_into()?;
        let revision: RevisionId = "configuration".try_into()?;
        let route = ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: "tenant".try_into()?,
            resource_kind: ResourceKind::Route,
            resource_id: "route".try_into()?,
            revision_id: "v1".try_into()?,
        };
        let policy = ResourceRef {
            resource_kind: ResourceKind::Policy,
            resource_id: "policy".try_into()?,
            ..route.clone()
        };
        let root = DeploymentKey::from_bytes([23; 32])?;
        let crypto = root.trajectory()?;
        let key = crypto.correlation_key(&installation, &tenant, &revision, &route, &policy)?;
        let restarted = DeploymentKey::from_bytes([23; 32])?
            .trajectory()?
            .correlation_key(&installation, &tenant, &revision, &route, &policy)?;
        assert_eq!(key.key_id(), restarted.key_id());
        assert_ne!(key.key_id(), root.id());
        for other in [
            crypto.correlation_key(
                &"Installation".try_into()?,
                &tenant,
                &revision,
                &route,
                &policy,
            )?,
            crypto.correlation_key(
                &installation,
                &"Tenant".try_into()?,
                &revision,
                &route,
                &policy,
            )?,
            crypto.correlation_key(
                &installation,
                &tenant,
                &"other-config".try_into()?,
                &route,
                &policy,
            )?,
            crypto.correlation_key(
                &installation,
                &tenant,
                &revision,
                &ResourceRef {
                    revision_id: "v2".try_into()?,
                    ..route.clone()
                },
                &policy,
            )?,
            crypto.correlation_key(
                &installation,
                &tenant,
                &revision,
                &route,
                &ResourceRef {
                    owner_id: "other-owner".try_into()?,
                    ..policy.clone()
                },
            )?,
            DeploymentKey::from_bytes([24; 32])?
                .trajectory()?
                .correlation_key(&installation, &tenant, &revision, &route, &policy)?,
        ] {
            assert_ne!(key.key_id(), other.key_id());
        }
        // The factory deliberately has no principal/token input; same profile
        // reconstructs the same key independently of who later executes it.
        assert_eq!(
            key.request_identity("scope", "execution")?,
            restarted.request_identity("scope", "execution")?
        );
        Ok(())
    }
}
