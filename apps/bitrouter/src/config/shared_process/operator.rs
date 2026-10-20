//! Explicit process-local operator authorization, never inference authority.

use std::{collections::BTreeSet, net::SocketAddr};

use anyhow::{Result, ensure};
use bitrouter_ai::gateway::operator::Grant;
use serde::Deserialize;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub listen: SocketAddr,
    pub credentials: Vec<Credential>,
    #[serde(default = "default_connections")]
    pub max_connections: usize,
}

fn default_connections() -> usize {
    16
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Credential {
    pub id: String,
    pub token_env: String,
    pub grants: Vec<Grant>,
}

impl Config {
    /// Run on the literal document before any environment resolution.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.listen.ip().is_loopback() && self.listen.port() != 0,
            "operator listener must use a numeric loopback address and nonzero port"
        );
        ensure!(
            (1..=256).contains(&self.max_connections),
            "operator connection limit must be 1..256"
        );
        ensure!(
            (1..=64).contains(&self.credentials.len()),
            "operator credentials must contain 1..64 entries"
        );
        let mut ids = BTreeSet::new();
        for credential in &self.credentials {
            ensure!(
                !credential.id.is_empty()
                    && credential.id.len() <= 128
                    && credential
                        .id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte)),
                "invalid operator identity"
            );
            ensure!(ids.insert(&credential.id), "duplicate operator identity");
            let name = credential.token_env.as_bytes();
            ensure!(
                !name.is_empty()
                    && name.len() <= 128
                    && (name[0].is_ascii_alphabetic() || name[0] == b'_')
                    && name
                        .iter()
                        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_'),
                "operator token_env must name a literal environment variable"
            );
            ensure!(
                !credential.grants.is_empty()
                    && credential.grants.iter().collect::<BTreeSet<_>>().len()
                        == credential.grants.len(),
                "operator grants must be explicit, nonempty and unique"
            );
        }
        Ok(())
    }
}
