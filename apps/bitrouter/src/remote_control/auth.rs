//! Validated host-control authority. Plaintext credentials never survive setup.

use std::collections::BTreeSet;

use crate::config::ControlConfig;
use crate::config::ControlCredentialConfig;
use anyhow::{Result, ensure};
use bitrouter_ai::control::{ControlScope, MIN_TOKEN_BYTES};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

#[derive(Debug, Clone)]
pub struct ControlCaller {
    id: String,
    scopes: Vec<ControlScope>,
}

impl ControlCaller {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn scopes(&self) -> &[ControlScope] {
        &self.scopes
    }
    pub fn permits(&self, scope: ControlScope) -> bool {
        self.scopes.contains(&scope)
    }
}

#[derive(Clone)]
struct Credential {
    caller: ControlCaller,
    digest: Vec<u8>,
}

#[derive(Clone)]
pub struct ControlAuth {
    credentials: Vec<Credential>,
    comparison_key: [u8; 32],
}

impl ControlAuth {
    pub fn from_config(config: &ControlConfig) -> Result<Self> {
        Self::from_lookup(config, |name| std::env::var(name).ok())
    }

    pub fn from_lookup(
        config: &ControlConfig,
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self> {
        let fallback = vec![ControlCredentialConfig {
            id: "legacy-operator".into(),
            token_env: super::CONTROL_TOKEN_ENV.into(),
            scopes: vec![ControlScope::Read],
        }];
        let entries = if config.credentials.is_empty() {
            &fallback
        } else {
            &config.credentials
        };
        let mut ids = BTreeSet::new();
        let mut digests = BTreeSet::new();
        let mut credentials = Vec::with_capacity(entries.len());
        // Private to this immutable auth snapshot. Rebuilding from configuration
        // preserves token semantics without a shared, fixed HMAC comparison key.
        let comparison_key = rand::random();
        for entry in entries {
            ensure!(valid_name(&entry.id, true), "invalid control credential ID");
            if entry.scopes.contains(&ControlScope::Tenancy) {
                bitrouter_ai::tenancy::identity::PrincipalId::for_control_credential(&entry.id)?;
            }
            ensure!(
                ids.insert(entry.id.clone()),
                "duplicate control credential ID"
            );
            ensure!(
                valid_name(&entry.token_env, false),
                "invalid control credential token_env"
            );
            ensure!(
                !entry.scopes.is_empty()
                    && (!entry.scopes.contains(&ControlScope::Reload)
                        || entry.scopes.contains(&ControlScope::Read)),
                "control credentials require a scope; control:reload also requires control:read"
            );
            let unique_scopes: BTreeSet<_> = entry.scopes.iter().collect();
            ensure!(
                unique_scopes.len() == entry.scopes.len(),
                "duplicate control scope"
            );
            let token = lookup(&entry.token_env).ok_or_else(|| {
                anyhow::anyhow!("control credential environment variable is missing")
            })?;
            ensure!(
                token.len() >= MIN_TOKEN_BYTES,
                "control token must contain at least 32 bytes"
            );
            let digest = token_digest(&comparison_key, token.as_bytes())?;
            ensure!(
                digests.insert(digest.clone()),
                "control credentials must have distinct tokens"
            );
            credentials.push(Credential {
                caller: ControlCaller {
                    id: entry.id.clone(),
                    scopes: entry.scopes.clone(),
                },
                digest,
            });
        }
        Ok(Self {
            credentials,
            comparison_key,
        })
    }

    /// Visit every configured credential, including after a successful match.
    pub fn authenticate(&self, token: &[u8]) -> Option<ControlCaller> {
        let mut selected = None;
        for credential in &self.credentials {
            let verified =
                Hmac::<Sha256>::new_from_slice(&self.comparison_key).is_ok_and(|mut verifier| {
                    verifier.update(token);
                    verifier.verify_slice(&credential.digest).is_ok()
                });
            if verified {
                selected = Some(credential.caller.clone());
            }
        }
        selected
    }
}

fn token_digest(key: &[u8; 32], token: &[u8]) -> Result<Vec<u8>> {
    let mut digest = Hmac::<Sha256>::new_from_slice(key)
        .map_err(|_| anyhow::anyhow!("initialize control credential verification"))?;
    digest.update(token);
    Ok(digest.finalize().into_bytes().to_vec())
}

fn valid_name(value: &str, credential_id: bool) -> bool {
    if value.is_empty() || value.len() > 256 {
        return false;
    }
    value.bytes().enumerate().all(|(index, byte)| {
        byte.is_ascii_alphabetic()
            || byte == b'_'
            || (index > 0 && byte.is_ascii_digit())
            || (credential_id && index > 0 && matches!(byte, b'-' | b'.'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ControlConfig {
        ControlConfig {
            credentials: vec![ControlCredentialConfig {
                id: "admin".into(),
                token_env: "ADMIN_TOKEN".into(),
                scopes: vec![ControlScope::Read, ControlScope::Reload],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn legacy_token_never_gains_write_authority() -> Result<()> {
        let auth = ControlAuth::from_lookup(&ControlConfig::default(), |_| Some("r".repeat(32)))?;
        let caller = auth
            .authenticate("r".repeat(32).as_bytes())
            .ok_or_else(|| anyhow::anyhow!("reader rejected"))?;
        assert!(caller.permits(ControlScope::Read));
        assert!(!caller.permits(ControlScope::Reload));
        assert!(auth.authenticate(b"wrong").is_none());
        Ok(())
    }

    #[test]
    fn explicit_credentials_ignore_legacy_env() -> Result<()> {
        let auth = ControlAuth::from_lookup(&config(), |name| {
            Some(if name == "ADMIN_TOKEN" { "a" } else { "r" }.repeat(32))
        })?;
        assert!(auth.authenticate("r".repeat(32).as_bytes()).is_none());
        let admin = auth
            .authenticate("a".repeat(32).as_bytes())
            .ok_or_else(|| anyhow::anyhow!("admin rejected"))?;
        assert!(admin.permits(ControlScope::Reload));
        Ok(())
    }

    #[test]
    fn private_comparison_keys_preserve_clone_and_reload_authentication() -> Result<()> {
        let token = "operator-fixture".repeat(4);
        let first = ControlAuth::from_lookup(&config(), |_| Some(token.clone()))?;
        let clone = first.clone();
        let reloaded = ControlAuth::from_lookup(&config(), |_| Some(token.clone()))?;
        assert_ne!(first.comparison_key, reloaded.comparison_key);
        for auth in [&first, &clone, &reloaded] {
            let caller = auth
                .authenticate(token.as_bytes())
                .ok_or_else(|| anyhow::anyhow!("operator rejected"))?;
            assert_eq!(caller.id(), "admin");
            assert!(caller.permits(ControlScope::Reload));
            assert!(auth.authenticate(b"unrecognized-token").is_none());
        }
        Ok(())
    }

    #[test]
    fn invalid_credential_configuration_is_rejected() {
        let mut duplicate = config();
        let mut second = duplicate.credentials[0].clone();
        second.id = "second".into();
        duplicate.credentials.push(second);
        assert!(ControlAuth::from_lookup(&duplicate, |_| Some("a".repeat(32))).is_err());
        let mut no_read = config();
        no_read.credentials[0].scopes = vec![ControlScope::Reload];
        assert!(ControlAuth::from_lookup(&no_read, |_| Some("a".repeat(32))).is_err());
        let mut bad_env = config();
        bad_env.credentials[0].token_env = "TOKEN=secret".into();
        assert!(ControlAuth::from_lookup(&bad_env, |_| Some("a".repeat(32))).is_err());
        assert!(ControlAuth::from_lookup(&config(), |_| Some("short".into())).is_err());
        assert!(ControlAuth::from_lookup(&config(), |_| None).is_err());
    }

    #[test]
    fn tenancy_operator_identity_is_bounded_and_survives_token_rotation() -> Result<()> {
        use bitrouter_ai::tenancy::identity::PrincipalId;
        let mut config = config();
        config.credentials[0].scopes = vec![ControlScope::Tenancy];
        for id in ["_Operator.one-2".into(), "x".repeat(119)] {
            config.credentials[0].id = id.clone();
            for token in ["a".repeat(32), "b".repeat(32)] {
                let auth = ControlAuth::from_lookup(&config, |_| Some(token.clone()))?;
                if token.starts_with('b') {
                    assert!(auth.authenticate("a".repeat(32).as_bytes()).is_none());
                }
                let caller = auth
                    .authenticate(token.as_bytes())
                    .ok_or_else(|| anyhow::anyhow!("operator rejected"))?;
                assert_eq!(
                    PrincipalId::for_control_credential(caller.id())?.as_str(),
                    format!("operator.{id}")
                );
            }
        }
        for length in [120, 256] {
            config.credentials[0].id = "x".repeat(length);
            assert!(ControlAuth::from_lookup(&config, |_| Some("a".repeat(32))).is_err());
        }
        config.credentials[0].scopes = vec![ControlScope::Read];
        assert!(ControlAuth::from_lookup(&config, |_| Some("a".repeat(32))).is_ok());
        assert_ne!(
            PrincipalId::for_control_credential("Admin")?,
            PrincipalId::for_control_credential("admin")?
        );
        for id in ["", "1operator", "operator:one", "operator/one", "操作员"] {
            assert!(PrincipalId::for_control_credential(id).is_err());
        }
        Ok(())
    }
}
