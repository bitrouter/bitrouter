//! Process-local bounds and explicit selection of shared gateway authority.

use serde::{Deserialize, Serialize};

use crate::gateway::error::{BitrouterError, Result};

/// Local SQLite needs no external service. Shared operation is an explicit opt-in.
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayStateConfig {
    /// Existing single-process authority and lifecycle.
    Local {},
    /// PostgreSQL authority shared by independently launched gateway processes.
    Shared(SharedGatewayConfig),
}

impl Default for GatewayStateConfig {
    fn default() -> Self {
        Self::Local {}
    }
}

/// These settings belong to each process; routing/policy data is published separately.
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SharedGatewayConfig {
    /// Explicit initialized cluster identity; 1..64 ASCII letters, digits, '-' or '_'.
    pub namespace: String,
    /// Environment variable containing a base64-encoded 32-byte deployment key.
    pub deployment_key_env: String,
    /// Total database connections (2..128), including one reserved for heartbeat.
    #[serde(default = "default_pool_connections")]
    pub pool_connections: u32,
    /// Database acquisition/transaction deadline, in milliseconds (100..30000).
    #[serde(default = "default_transaction_timeout_ms")]
    pub transaction_timeout_ms: u64,
    /// Owner-incarnation lifetime in database milliseconds (1000..300000).
    #[serde(default = "default_owner_lease_ms")]
    pub owner_lease_ms: u64,
    /// Heartbeat interval; leave room for three intervals plus a transaction.
    #[serde(default = "default_heartbeat_ms")]
    pub heartbeat_ms: u64,
    /// Nonqueueing admission bound, including slow response consumers (1..65536).
    #[serde(default = "default_local_max_in_flight")]
    pub local_max_in_flight: usize,
    /// Maximum retired routing generations still retaining resources (1..64).
    /// The current generation and one candidate are additional slots.
    #[serde(default = "default_max_retired")]
    pub max_retired: usize,
    /// Recovery/publication polling interval, in milliseconds (100..30000).
    #[serde(default = "default_worker_interval_ms")]
    pub worker_interval_ms: u64,
    /// Maximum records visited by one background pass (1..256).
    #[serde(default = "default_worker_batch_size")]
    pub worker_batch_size: u64,
    /// Graceful drain deadline, in milliseconds (100..300000).
    #[serde(default = "default_drain_timeout_ms")]
    pub drain_timeout_ms: u64,
    /// Maximum accepted ingress bytes (1024..16777216).
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: usize,
    /// Maximum live or unreaped HTTP connections (1..65536); no waiting queue.
    #[serde(default = "default_local_max_in_flight")]
    pub max_connections: usize,
    /// Request header deadline in milliseconds (100..300000).
    #[serde(default = "default_transport_timeout_ms")]
    pub header_timeout_ms: u64,
    /// Body ingress deadline in milliseconds (100..300000), excluding inference.
    #[serde(default = "default_transport_timeout_ms")]
    pub request_timeout_ms: u64,
    /// Output backpressure deadline in milliseconds (100..300000).
    #[serde(default = "default_transport_timeout_ms")]
    pub write_stall_timeout_ms: u64,
}

impl GatewayStateConfig {
    /// Validate without resolving a secret, opening a pool, or modifying local state.
    pub fn validate(&self, database_url: &str, skip_auth: bool) -> Result<()> {
        let Self::Shared(shared) = self else {
            return Ok(());
        };
        let invalid = |message| BitrouterError::bad_request(message);
        if skip_auth {
            return Err(invalid(
                "gateway_state.mode=shared requires server.skip_auth=false",
            ));
        }
        let url = url::Url::parse(database_url)
            .map_err(|_| invalid("shared gateway requires a PostgreSQL database URL"))?;
        if !matches!(url.scheme(), "postgres" | "postgresql")
            || url.host_str().is_none()
            || url.path().trim_matches('/').is_empty()
        {
            return Err(invalid(
                "shared gateway requires a named PostgreSQL database",
            ));
        }
        if shared.namespace.is_empty()
            || shared.namespace.len() > 64
            || !shared
                .namespace
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(invalid(
                "gateway_state.namespace must contain 1..64 ASCII letters, digits, '-' or '_'",
            ));
        }
        let name = shared.deployment_key_env.as_bytes();
        if name.is_empty()
            || name.len() > 128
            || !(name[0].is_ascii_alphabetic() || name[0] == b'_')
            || !name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
        {
            return Err(invalid(
                "gateway_state.deployment_key_env must name an environment variable",
            ));
        }
        if !(2..=128).contains(&shared.pool_connections)
            || !(100..=30_000).contains(&shared.transaction_timeout_ms)
            || !(1_000..=300_000).contains(&shared.owner_lease_ms)
            || !(100..=30_000).contains(&shared.heartbeat_ms)
            || shared
                .heartbeat_ms
                .saturating_mul(3)
                .saturating_add(shared.transaction_timeout_ms)
                >= shared.owner_lease_ms
            || !(1..=65_536).contains(&shared.local_max_in_flight)
            || !(1..=64).contains(&shared.max_retired)
            || !(100..=30_000).contains(&shared.worker_interval_ms)
            || !(1..=256).contains(&shared.worker_batch_size)
            || !(100..=300_000).contains(&shared.drain_timeout_ms)
            || !(1_024..=16_777_216).contains(&shared.max_request_bytes)
            || !(1..=65_536).contains(&shared.max_connections)
            || !(100..=300_000).contains(&shared.header_timeout_ms)
            || !(100..=300_000).contains(&shared.request_timeout_ms)
            || !(100..=300_000).contains(&shared.write_stall_timeout_ms)
        {
            return Err(invalid(
                "invalid shared gateway bounds; owner lease must exceed three heartbeats plus one transaction deadline",
            ));
        }
        Ok(())
    }
}

fn default_pool_connections() -> u32 {
    16
}
fn default_transaction_timeout_ms() -> u64 {
    3_000
}
fn default_owner_lease_ms() -> u64 {
    30_000
}
fn default_heartbeat_ms() -> u64 {
    5_000
}
fn default_local_max_in_flight() -> usize {
    256
}
fn default_max_retired() -> usize {
    4
}
fn default_worker_interval_ms() -> u64 {
    1_000
}
fn default_worker_batch_size() -> u64 {
    64
}
fn default_drain_timeout_ms() -> u64 {
    30_000
}
fn default_max_request_bytes() -> usize {
    4_194_304
}
fn default_transport_timeout_ms() -> u64 {
    30_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_shared_mode_validates_before_resolving_secrets() -> anyhow::Result<()> {
        let base = serde_json::json!({"mode":"shared", "namespace":"fixture-cluster",
            "deployment_key_env":"FIXTURE_UNSET_DEPLOYMENT_KEY"});
        let config: GatewayStateConfig = serde_json::from_value(base.clone())?;
        config.validate("postgres://user:secret@localhost/fixture", false)?;
        assert!(config.validate("sqlite::memory:", false).is_err());
        assert!(
            config
                .validate("postgres://localhost/fixture", true)
                .is_err()
        );
        for (field, invalid) in [
            ("namespace", serde_json::json!("a/b")),
            ("deployment_key_env", serde_json::json!("inline=secret")),
            ("pool_connections", serde_json::json!(0)),
            ("pool_connections", serde_json::json!(1)),
            ("pool_connections", serde_json::json!(129)),
            ("transaction_timeout_ms", serde_json::json!(30001)),
            ("owner_lease_ms", serde_json::json!(15000)),
            ("local_max_in_flight", serde_json::json!(0)),
            ("max_retired", serde_json::json!(0)),
            ("max_retired", serde_json::json!(65)),
            ("worker_batch_size", serde_json::json!(257)),
            ("max_connections", serde_json::json!(0)),
            ("header_timeout_ms", serde_json::json!(99)),
            ("request_timeout_ms", serde_json::json!(300001)),
            ("write_stall_timeout_ms", serde_json::json!(0)),
        ] {
            let mut value = base.clone();
            value[field] = invalid;
            let config: GatewayStateConfig = serde_json::from_value(value)?;
            assert!(
                config
                    .validate("postgres://localhost/fixture", false)
                    .is_err(),
                "accepted {field}"
            );
        }
        for limit in [2, 128] {
            let mut value = base.clone();
            value["pool_connections"] = serde_json::json!(limit);
            let config: GatewayStateConfig = serde_json::from_value(value)?;
            config.validate("postgres://localhost/fixture", false)?;
        }
        if let GatewayStateConfig::Shared(shared) = config {
            assert_eq!(shared.max_retired, 4);
        }
        for limit in [1, 64] {
            let mut value = base.clone();
            value["max_retired"] = serde_json::json!(limit);
            let config: GatewayStateConfig = serde_json::from_value(value)?;
            config.validate("postgres://localhost/fixture", false)?;
        }
        assert!(
            serde_json::from_value::<GatewayStateConfig>(serde_json::json!({
                "mode":"local", "namespace":"ignored-authority"
            }))
            .is_err()
        );
        assert!(matches!(
            crate::config::parse("{}")?.gateway_state,
            GatewayStateConfig::Local {}
        ));
        Ok(())
    }
}
