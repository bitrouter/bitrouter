//! Standalone connector configuration, separate from gateway configuration.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

/// The standalone, fixed-model connector format.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConnectorConfig {
    #[schemars(range(min = 1, max = 1))]
    pub version: u32,
    pub pool: PoolConfig,
    #[serde(default)]
    pub observation: ObservationConfig,
    /// Absent configurations remain observation-only.
    pub execution: Option<ExecutionConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PoolConfig {
    #[schemars(length(min = 1, max = 64))]
    pub id: String,
    #[schemars(length(min = 1, max = 256))]
    pub model: String,
    pub scope: PoolScope,
    #[schemars(length(min = 1, max = 64))]
    pub members: Vec<MemberConfig>,
}

/// A complete member list declared by the operator, not discovered from a router.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PoolScope {
    StaticMembers {},
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MemberConfig {
    #[schemars(length(min = 1, max = 64))]
    pub id: String,
    #[schemars(length(min = 1, max = 2048))]
    pub inference_endpoint: String,
    #[schemars(length(min = 1, max = 2048))]
    pub metrics_endpoint: String,
    /// Complete set of vLLM `engine` labels at this direct member endpoint.
    #[schemars(length(min = 1, max = 256))]
    pub engines: Vec<String>,
    /// The variable name only. Its value is read just before a metrics request.
    pub metrics_bearer_env: Option<String>,
    /// Optional local inference credential; never transmitted to callers.
    pub inference_bearer_env: Option<String>,
}

/// Explicit operator-selected pilot limits; no capacity is inferred from GPU use.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfig {
    pub enabled: bool,
    /// Confirms this persistent authority volume is shared by every instance
    /// allowed to admit extra traffic for the declared pool.
    #[schemars(extend("const" = true))]
    pub exclusive_pool_owner: bool,
    /// Explicit acknowledgement when exporter sample timestamps are unavailable.
    #[serde(default)]
    pub allow_collection_only_metrics: bool,
    #[schemars(length(min = 1, max = 4096))]
    pub state_dir: String,
    #[schemars(length(min = 1, max = 128), extend("pattern" = "^[A-Za-z_][A-Za-z0-9_]{0,127}$"))]
    pub auth_bearer_env: String,
    #[schemars(range(max = 1000), extend("exclusiveMinimum" = 0))]
    pub max_dispatch_rps: f64,
    #[schemars(range(min = 1, max = 1000))]
    pub burst: u32,
    #[schemars(range(min = 1, max = 1000))]
    pub max_extra_inflight: u32,
    /// Retained identities are never purged or made executable again.
    #[schemars(range(min = 1, max = 100000))]
    pub max_records: u32,
    #[schemars(range(min = 1, max = 1000000))]
    pub max_input_tokens: u32,
    #[schemars(range(min = 1, max = 1000000))]
    pub max_output_tokens: u32,
    #[schemars(range(min = 100, max = 300000))]
    pub max_deadline_ms: u64,
    #[schemars(range(min = 1024, max = 4194304))]
    pub max_request_bytes: usize,
    #[schemars(range(min = 1024, max = 4194304))]
    pub max_response_bytes: usize,
    #[schemars(range(min = 10, max = 300000))]
    pub stream_idle_timeout_ms: u64,
    #[schemars(range(min = 100, max = 30000))]
    pub lease_ms: u64,
    #[schemars(range(min = 10, max = 15000))]
    pub update_interval_ms: u64,
    #[schemars(range(min = 10, max = 300000))]
    pub health_window_ms: u64,
    /// Aggregate engine gauges include both primary and extra traffic.
    #[schemars(range(min = 1, max = 1000000))]
    pub max_running_requests: u64,
    #[schemars(range(min = 0, max = 1000000))]
    pub max_waiting_requests: u64,
    #[schemars(range(max = 1), extend("exclusiveMinimum" = 0))]
    pub max_kv_cache_usage_fraction: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ObservationConfig {
    #[schemars(range(min = 10, max = 30000))]
    pub timeout_ms: u64,
    #[schemars(range(min = 1024, max = 4194304))]
    pub max_body_bytes: usize,
    #[schemars(range(min = 100, max = 300000))]
    pub max_sample_age_ms: u64,
}

impl Default for ObservationConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 2_000,
            max_body_bytes: 1_048_576,
            max_sample_age_ms: 15_000,
        }
    }
}

/// Diagnostics never embed YAML, endpoint text, or credential values.
#[derive(Debug, Error)]
#[error("invalid connector configuration: {0}")]
pub struct ConfigError(&'static str);

impl ConnectorConfig {
    pub fn from_yaml(raw: &str) -> Result<Self, ConfigError> {
        if raw.len() > 65_536 {
            return Err(ConfigError("document exceeds 64 KiB"));
        }
        let config: Self = serde_saphyr::from_str(raw)
            .map_err(|_| ConfigError("expected the strict version 1 YAML structure"))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version != 1 {
            return Err(ConfigError("unsupported version; expected 1"));
        }
        if !identifier(&self.pool.id) {
            return Err(ConfigError("pool id must be a short ASCII identifier"));
        }
        if self.pool.model.is_empty()
            || self.pool.model.len() > 256
            || self.pool.model.chars().any(char::is_whitespace)
            || self.pool.model.chars().any(char::is_control)
        {
            return Err(ConfigError(
                "model must be a nonempty identifier without whitespace",
            ));
        }
        if self.pool.members.is_empty() || self.pool.members.len() > 64 {
            return Err(ConfigError("declare between 1 and 64 direct pool members"));
        }
        let mut ids = BTreeSet::new();
        let mut endpoints = BTreeSet::new();
        let mut metrics_endpoints = BTreeSet::new();
        for member in &self.pool.members {
            if !identifier(&member.id) || !ids.insert(&member.id) {
                return Err(ConfigError(
                    "member ids must be unique short ASCII identifiers",
                ));
            }
            let inference = endpoint(&member.inference_endpoint)?;
            let metrics = endpoint(&member.metrics_endpoint)?;
            if self
                .execution
                .as_ref()
                .is_some_and(|execution| execution.enabled)
                && !matches!(inference.path(), "/v1" | "/v1/")
            {
                return Err(ConfigError(
                    "execution requires direct vLLM inference endpoints ending in /v1",
                ));
            }
            if !metrics_endpoints.insert(metrics.to_string()) {
                return Err(ConfigError("direct metrics endpoints must be unique"));
            }
            let inference_key = if self
                .execution
                .as_ref()
                .is_some_and(|execution| execution.enabled)
            {
                inference.as_str().trim_end_matches('/').to_string()
            } else {
                inference.to_string()
            };
            if !endpoints.insert(inference_key) {
                return Err(ConfigError("direct inference endpoints must be unique"));
            }
            if member.engines.is_empty() || member.engines.len() > 256 {
                return Err(ConfigError(
                    "declare between 1 and 256 engine labels per member",
                ));
            }
            let mut engines = BTreeSet::new();
            for engine in &member.engines {
                if engine.is_empty()
                    || engine.len() > 10
                    || !engine.bytes().all(|byte| byte.is_ascii_digit())
                    || !engines.insert(engine)
                {
                    return Err(ConfigError(
                        "engine labels must be unique nonempty decimal strings",
                    ));
                }
            }
            if member
                .metrics_bearer_env
                .as_deref()
                .is_some_and(|name| !environment_name(name))
                || member
                    .inference_bearer_env
                    .as_deref()
                    .is_some_and(|name| !environment_name(name))
            {
                return Err(ConfigError(
                    "bearer credential references must be environment variable names",
                ));
            }
        }
        if !(10..=30_000).contains(&self.observation.timeout_ms)
            || !(1_024..=4_194_304).contains(&self.observation.max_body_bytes)
            || !(100..=300_000).contains(&self.observation.max_sample_age_ms)
            || self.observation.timeout_ms > self.observation.max_sample_age_ms
        {
            return Err(ConfigError(
                "invalid observation bounds: timeout 10..30000 ms, body 1024..4194304 bytes, sample age 100..300000 ms, timeout <= sample age",
            ));
        }
        if let Some(execution) = &self.execution {
            execution.validate(&self.observation)?;
        }
        Ok(())
    }
}

impl ExecutionConfig {
    fn validate(&self, observation: &ObservationConfig) -> Result<(), ConfigError> {
        let path = std::path::Path::new(&self.state_dir);
        if !self.exclusive_pool_owner
            || !path.is_absolute()
            || self.state_dir.len() > 4096
            || self.state_dir.contains(['?', '%', '#', '\0'])
            || self.state_dir.chars().any(char::is_control)
            || !environment_name(&self.auth_bearer_env)
        {
            return Err(ConfigError(
                "execution requires an exclusive shared persistent authority directory and a local auth environment reference",
            ));
        }
        if !self.max_dispatch_rps.is_finite()
            || self.max_dispatch_rps <= 0.0
            || self.max_dispatch_rps > 1000.0
            || !(1..=1000).contains(&self.burst)
            || !(1..=1000).contains(&self.max_extra_inflight)
            || !(1..=100_000).contains(&self.max_records)
            || !(1..=1_000_000).contains(&self.max_input_tokens)
            || !(1..=1_000_000).contains(&self.max_output_tokens)
            || !(100..=300_000).contains(&self.max_deadline_ms)
            || !(1024..=4_194_304).contains(&self.max_request_bytes)
            || !(1024..=4_194_304).contains(&self.max_response_bytes)
            || !(10..=self.max_deadline_ms).contains(&self.stream_idle_timeout_ms)
            || !(100..=30_000).contains(&self.lease_ms)
            || self.lease_ms > observation.max_sample_age_ms
            || !(10..=self.lease_ms / 2).contains(&self.update_interval_ms)
            || !(self.update_interval_ms..=300_000).contains(&self.health_window_ms)
            || self.max_running_requests == 0
            || self.max_running_requests > 1_000_000
            || self.max_waiting_requests > 1_000_000
            || !self.max_kv_cache_usage_fraction.is_finite()
            || !(0.0..=1.0).contains(&self.max_kv_cache_usage_fraction)
            || self.max_kv_cache_usage_fraction == 0.0
        {
            return Err(ConfigError(
                "invalid explicit execution limits or observation/lease/recovery timing",
            ));
        }
        Ok(())
    }
}

fn environment_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
        })
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn endpoint(raw: &str) -> Result<Url, ConfigError> {
    let parsed = Url::parse(raw).map_err(|_| ConfigError("invalid endpoint URL"))?;
    if raw.len() > 2_048
        || !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(ConfigError(
            "endpoints require HTTP(S), a host, and no userinfo, query, or fragment",
        ));
    }
    Ok(parsed)
}
