//! Installation-local opaque references for authenticated upstream accounts.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use bitrouter_sdk::language_model::auth::ContinuationAuthority;

const DOMAIN: &[u8] = b"bitrouter.upstream-account-ref.v1";

#[derive(Clone)]
pub struct AccountRefKey([u8; 32]);

impl AccountRefKey {
    pub fn load(home: &std::path::Path) -> anyhow::Result<Self> {
        crate::paths::get_or_create_account_ref_key(home).map(Self)
    }

    pub fn ephemeral() -> Self {
        Self(rand::random())
    }

    pub fn derive(&self, provider_id: &str, authority: &ContinuationAuthority) -> Option<String> {
        let mut mac = <Hmac<Sha256>>::new_from_slice(&self.0).ok()?;
        mac.update(DOMAIN);
        mac.update(&[0]);
        mac.update(provider_id.as_bytes());
        mac.update(&[0]);
        mac.update(authority.credential().proof_bytes());
        Some(format!(
            "account-v1-{}",
            hex::encode(mac.finalize().into_bytes())
        ))
    }
}

impl std::fmt::Debug for AccountRefKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AccountRefKey(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_sdk::language_model::auth::CredentialAuthority;
    use bitrouter_sdk::language_model::types::AuthScheme;

    fn authority(value: &str) -> ContinuationAuthority {
        ContinuationAuthority::new(
            CredentialAuthority::derive("test", value),
            AuthScheme::Bearer,
        )
    }

    #[test]
    fn refs_are_installation_and_provider_scoped() {
        let first = AccountRefKey::ephemeral();
        let second = AccountRefKey::ephemeral();
        let authority = authority("principal");
        let stable = first.derive("provider-a", &authority);
        assert_eq!(stable, first.derive("provider-a", &authority));
        assert_ne!(stable, first.derive("provider-b", &authority));
        assert_ne!(stable, second.derive("provider-a", &authority));
        assert!(
            !stable
                .as_deref()
                .is_some_and(|value| value.contains("principal"))
        );
    }
}
