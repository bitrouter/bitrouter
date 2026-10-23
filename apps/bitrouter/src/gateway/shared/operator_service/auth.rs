//! Immutable explicit operator identities. No plaintext token survives setup.

use std::{collections::BTreeSet, sync::Arc};

use anyhow::{Context, Result, ensure};
use axum::http::{HeaderMap, header};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use super::super::operator::Actor;
use crate::config::shared_process::operator::Config;

struct Credential {
    actor: Arc<Actor>,
    digest: [u8; 32],
}

pub(in crate::gateway::shared) struct Auth {
    comparison_key: [u8; 32],
    credentials: Vec<Credential>,
}

impl Auth {
    pub fn new(config: &Config) -> Result<Self> {
        Self::from_lookup(config, |name| std::env::var(name).ok())
    }

    pub(super) fn from_lookup(
        config: &Config,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        config.validate()?;
        let comparison_key: [u8; 32] = rand::random();
        let mut credentials = Vec::with_capacity(config.credentials.len());
        let mut digests = BTreeSet::new();
        for entry in &config.credentials {
            let token = lookup(&entry.token_env)
                .context("operator token environment variable is unavailable")?;
            ensure!(
                valid_token(&token),
                "operator token must contain 32..4096 Bearer-compatible bytes"
            );
            let mut digest = Hmac::<Sha256>::new_from_slice(&comparison_key)
                .map_err(|_| anyhow::anyhow!("operator comparison key initialization failed"))?;
            digest.update(token.as_bytes());
            let digest: [u8; 32] = digest.finalize().into_bytes().into();
            ensure!(
                digests.insert(digest),
                "operator credentials must use distinct tokens"
            );
            credentials.push(Credential {
                actor: Arc::new(Actor::new(
                    entry.id.clone(),
                    entry.grants.iter().copied().collect(),
                )?),
                digest,
            });
        }
        Ok(Self {
            comparison_key,
            credentials,
        })
    }

    pub fn authenticate(&self, headers: &HeaderMap) -> Option<Arc<Actor>> {
        let mut values = headers.get_all(header::AUTHORIZATION).iter();
        let value = values.next()?.to_str().ok()?;
        if values.next().is_some() {
            return None;
        }
        // Authentication schemes are case-insensitive; credentials remain opaque.
        // https://www.rfc-editor.org/rfc/rfc9110.html#section-11.1
        // https://www.rfc-editor.org/rfc/rfc6750.html#section-2.1
        let (scheme, token) = value.split_once(' ')?;
        let token = token.trim_start_matches(' ');
        if !scheme.eq_ignore_ascii_case("Bearer") || !valid_token(token) {
            return None;
        }
        let mut selected = None;
        // Visit every credential; constant-time MAC verification avoids comparing
        // token bytes or exposing the index of the matching identity.
        // https://docs.rs/hmac/latest/hmac/#examples
        for entry in &self.credentials {
            let valid =
                Hmac::<Sha256>::new_from_slice(&self.comparison_key).is_ok_and(|mut digest| {
                    digest.update(token.as_bytes());
                    digest.verify_slice(&entry.digest).is_ok()
                });
            if valid {
                selected = Some(entry.actor.clone());
            }
        }
        selected
    }
}

fn valid_token(token: &str) -> bool {
    // https://www.rfc-editor.org/rfc/rfc6750.html#section-2.1
    let value = token.trim_end_matches('=');
    (32..=4096).contains(&token.len())
        && !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~+/".contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::shared_process::operator::Credential;
    use bitrouter_ai::gateway::operator::Grant;

    fn config() -> Result<Config> {
        Ok(Config {
            listen: "127.0.0.1:4358".parse()?,
            max_connections: 16,
            credentials: vec![Credential {
                id: "operator".into(),
                token_env: "OPERATOR_TOKEN".into(),
                grants: vec![Grant::Read],
            }],
        })
    }

    #[test]
    fn malformed_configuration_rejects_before_any_secret_lookup() -> Result<()> {
        let mut config = config()?;
        config.credentials.push(Credential {
            id: "second".into(),
            token_env: "${TOKEN}".into(),
            grants: vec![Grant::Read],
        });
        let looked_up = std::cell::Cell::new(false);
        ensure!(
            Auth::from_lookup(&config, |_| {
                looked_up.set(true);
                None
            })
            .is_err()
        );
        ensure!(!looked_up.get());
        Ok(())
    }

    #[test]
    fn only_explicit_unique_operator_tokens_authenticate() -> Result<()> {
        const TOKEN: &str = "fixture-operator-0123456789abcdef012345";
        let auth = Auth::from_lookup(&config()?, |name| {
            (name == "OPERATOR_TOKEN").then(|| TOKEN.into())
        })?;
        let mut headers = HeaderMap::new();
        ensure!(auth.authenticate(&headers).is_none());
        headers.insert(header::AUTHORIZATION, format!("bEaReR  {TOKEN}").parse()?);
        ensure!(auth.authenticate(&headers).is_some());
        headers.append(header::AUTHORIZATION, format!("Bearer {TOKEN}").parse()?);
        ensure!(auth.authenticate(&headers).is_none());
        headers.remove(header::AUTHORIZATION);
        headers.insert(
            header::AUTHORIZATION,
            "Bearer fixture-inference-token-0123456789abcdef".parse()?,
        );
        ensure!(auth.authenticate(&headers).is_none());
        let mut duplicate = config()?;
        duplicate.credentials.push(Credential {
            id: "second".into(),
            token_env: "OTHER_TOKEN".into(),
            grants: vec![Grant::PolicyWrite],
        });
        ensure!(Auth::from_lookup(&duplicate, |_| Some(TOKEN.into())).is_err());
        Ok(())
    }

    #[test]
    fn setup_and_request_share_the_same_bounded_bearer_grammar() -> Result<()> {
        for token in [
            "a".repeat(32),
            "a".repeat(4096),
            format!("{}-._~+/==", "a".repeat(32)),
        ] {
            let auth = Auth::from_lookup(&config()?, |_| Some(token.clone()))?;
            for scheme in ["Bearer ", "bEaReR   "] {
                let mut headers = HeaderMap::new();
                headers.insert(header::AUTHORIZATION, format!("{scheme}{token}").parse()?);
                ensure!(auth.authenticate(&headers).is_some());
            }
        }
        for token in [
            "a".repeat(31),
            "a".repeat(4097),
            "=".repeat(32),
            format!("{}=x", "a".repeat(32)),
            format!("{} ", "a".repeat(32)),
            format!("{}:x", "a".repeat(32)),
            format!("{}é", "a".repeat(32)),
        ] {
            let error = Auth::from_lookup(&config()?, |_| Some(token.clone()))
                .err()
                .context("accepted invalid operator token")?;
            ensure!(!format!("{error:#}").contains(&token));
        }
        let token = "a".repeat(32);
        let auth = Auth::from_lookup(&config()?, |_| Some(token.clone()))?;
        for header in [
            format!("Basic {token}"),
            format!("Bearer\t{token}"),
            format!("Bearer {token} "),
            format!("Bearer {token},other"),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, header.parse()?);
            ensure!(auth.authenticate(&headers).is_none());
        }
        Ok(())
    }
}
