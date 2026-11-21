//! Secret-free physical key derivation. A key is not a namespace or execution grant.
use anyhow::Result;
use bitrouter_ai::tenancy::identity::{InstallationId, TenantId};
use sha2::{Digest, Sha256};

// The same framing is used by Scope::physical_key for authenticated tenant keys.
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
    fn tenant_key_preserves_domain_and_identity_framing() -> Result<()> {
        let installation: InstallationId = "installation".try_into()?;
        let tenant: TenantId = "tenant".try_into()?;
        assert_eq!(
            tenant_key(&installation, &tenant, "eval.subject", &["logical"])?,
            "628cef0903080bc46e65b72bdc091f6d5cc51e17468c4bce830d73db3c6ab0e9"
        );
        assert_ne!(
            tenant_key(&"a".try_into()?, &"bc".try_into()?, "eval.subject", &[])?,
            tenant_key(&"ab".try_into()?, &"c".try_into()?, "eval.subject", &[])?
        );
        assert_ne!(
            tenant_key(&installation, &tenant, "eval.subject", &[])?,
            tenant_key(&installation, &"Tenant".try_into()?, "eval.subject", &[])?
        );
        Ok(())
    }
}
