//! Opt-in process-local provider scheduling configuration.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Deserializer, Serialize};

use crate::gateway::error::{BitrouterError, Result};

/// Local provider scheduling settings, disabled by default.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = enabled_destinations)]
pub struct InferenceRoutingConfig {
    pub enabled: bool,
    pub policy: Policy,
    pub queue: QueueConfig,
    pub limits: LimitConfig,
    pub prefix: PrefixConfig,
    #[schemars(length(max = 4096))]
    pub destinations: Vec<DestinationConfig>,
    #[schemars(transform = weights_schema)]
    pub tenant_weights: BTreeMap<String, u32>,
}

/// Provider ranking policy within frozen, eligible provider membership.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    #[default]
    LoadAware,
    PrefixAffinity,
}

/// Finite queue storage and total request deadlines, in bytes and milliseconds.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct QueueConfig {
    #[schemars(range(min = 1, max = u32::MAX))]
    pub max_requests: u32,
    #[schemars(range(min = 1, max = u32::MAX))]
    pub max_bytes: u32,
    #[schemars(range(min = 1, max = u32::MAX))]
    pub per_tenant_requests: u32,
    #[schemars(range(min = 1, max = u32::MAX))]
    pub per_tenant_bytes: u32,
    #[schemars(range(min = 1, max = 3_600_000))]
    pub wait_timeout_ms: u32,
    #[schemars(range(min = 1, max = 3_600_000))]
    pub request_timeout_ms: u32,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            max_requests: 1024,
            max_bytes: 67_108_864,
            per_tenant_requests: 128,
            per_tenant_bytes: 8_388_608,
            wait_timeout_ms: 2000,
            request_timeout_ms: 120_000,
        }
    }
}

/// Process-local concurrency, uncertain work retention and logical state budgets.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LimitConfig {
    #[schemars(range(min = 1, max = u32::MAX))]
    pub max_active: u32,
    #[schemars(range(min = 1, max = u32::MAX))]
    pub max_uncertain: u32,
    #[schemars(range(min = 1, max = 3_600_000))]
    pub uncertainty_ttl_ms: u32,
    #[schemars(range(min = 1, max = u32::MAX))]
    pub max_state_entries: u32,
    #[schemars(range(min = 1, max = u32::MAX))]
    pub max_state_bytes: u32,
}

impl Default for LimitConfig {
    fn default() -> Self {
        Self {
            max_active: 128,
            max_uncertain: 128,
            uncertainty_ttl_ms: 120_000,
            max_state_entries: 4096,
            max_state_bytes: 33_554_432,
        }
    }
}

/// Bounds for approximate prefix observations; lengths are bytes, never tokens.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PrefixConfig {
    #[schemars(range(min = 1, max = u32::MAX))]
    pub max_entries: u32,
    #[schemars(range(min = 1, max = u32::MAX))]
    pub max_bytes: u32,
    #[schemars(range(min = 256, max = 1_048_576))]
    pub max_projection_bytes: u32,
    #[schemars(range(min = 1, max = 3_600_000))]
    pub ttl_ms: u32,
}

impl Default for PrefixConfig {
    fn default() -> Self {
        Self {
            max_entries: 65_536,
            max_bytes: 16_777_216,
            max_projection_bytes: 32_768,
            ttl_ms: 120_000,
        }
    }
}

/// Exact configured destination. Credentials are never part of this declaration.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DestinationConfig {
    #[schemars(
        length(min = 1, max = 128),
        regex(pattern = "^[a-zA-Z0-9][a-zA-Z0-9_.-]*$")
    )]
    pub id: String,
    #[schemars(length(min = 1, max = 128))]
    pub provider: String,
    #[schemars(length(min = 1, max = 512))]
    pub model: String,
    #[schemars(length(max = 2048), extend("format" = "uri"))]
    pub endpoint: String,
    #[serde(
        default,
        deserialize_with = "account",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "String", length(min = 1, max = 128))]
    pub account: Option<String>,
    #[serde(default = "destination_capacity")]
    #[schemars(range(min = 1, max = u32::MAX))]
    pub max_active: u32,
    /// Compiled-in engine module. Absent destinations keep beta.4 scheduling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<EngineConfig>,
}

/// Which compiled-in module normalizes one destination. The scheduler does not
/// branch on this name beyond selecting that module.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EngineModule {
    Vllm,
    Sglang,
}

/// Router ceilings and the scrape target for one local engine process.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    pub module: EngineModule,
    #[schemars(length(max = 2048), extend("format" = "uri"))]
    pub metrics_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub metrics_bearer_env: Option<String>,
    #[schemars(range(min = 1, max = 60_000))]
    pub max_metric_age_ms: u32,
    #[schemars(range(min = 0, max = 100_000))]
    pub max_running: u32,
    #[schemars(range(min = 0, max = 100_000))]
    pub max_waiting: u32,
    pub max_kv_usage: KvUsage,
    #[serde(default = "count_input_default")]
    pub count_input: bool,
}

/// KV ceiling as thousandths of engine capacity, greater than 0 and at most 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KvUsage(u16);

fn count_input_default() -> bool {
    true
}

impl KvUsage {
    pub(crate) fn millis(self) -> u16 {
        self.0
    }
}

impl Serialize for KvUsage {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_f64(f64::from(self.0) / 1000.0)
    }
}

impl<'de> Deserialize<'de> for KvUsage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Visit;
        impl serde::de::Visitor<'_> for Visit {
            type Value = KvUsage;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a decimal fraction greater than 0 and at most 1")
            }
            fn visit_f64<E: serde::de::Error>(self, value: f64) -> std::result::Result<KvUsage, E> {
                kv_fraction(value).map_err(E::custom)
            }
            fn visit_u64<E: serde::de::Error>(self, value: u64) -> std::result::Result<KvUsage, E> {
                kv_fraction(value as f64).map_err(E::custom)
            }
            fn visit_i64<E: serde::de::Error>(self, value: i64) -> std::result::Result<KvUsage, E> {
                kv_fraction(value as f64).map_err(E::custom)
            }
        }
        deserializer.deserialize_any(Visit)
    }
}

impl schemars::JsonSchema for KvUsage {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("KvUsage")
    }
    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "number",
            "exclusiveMinimum": 0,
            "maximum": 1,
            "multipleOf": 0.001,
            "description": "Fraction of normalized engine KV capacity. At most three decimal places."
        })
    }
}

fn kv_fraction(value: f64) -> std::result::Result<KvUsage, &'static str> {
    if !value.is_finite() || value <= 0.0 || value > 1.0 {
        return Err("max_kv_usage must be greater than 0 and at most 1");
    }
    let millis = (value * 1000.0).round();
    if (millis / 1000.0 - value).abs() > 1e-6 || !(1.0..=1000.0).contains(&millis) {
        return Err("max_kv_usage keeps at most three decimal places");
    }
    Ok(KvUsage(millis as u16))
}

fn account<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

fn destination_capacity() -> u32 {
    8
}

fn enabled_destinations(schema: &mut schemars::Schema) {
    schema.insert("allOf".into(), serde_json::json!([{
        "if": {"properties": {"enabled": {"const": true}}, "required": ["enabled"]},
        "then": {"required": ["destinations"], "properties": {"destinations": {"type": "array", "minItems": 1}}}
    }]));
}

fn weights_schema(schema: &mut schemars::Schema) {
    schema.insert("maxProperties".into(), serde_json::json!(128));
    schema.insert(
        "propertyNames".into(),
        serde_json::json!({"type": "string", "minLength": 1, "maxLength": 128}),
    );
    schema.insert(
        "additionalProperties".into(),
        serde_json::json!({"type": "integer", "minimum": 1, "maximum": 100}),
    );
}

impl InferenceRoutingConfig {
    /// Validate retained settings even while disabled, before allocating runtime resources.
    pub fn validate(&self) -> Result<()> {
        self.validate_settings()?;
        if self.enabled {
            if !cfg!(feature = "inference-routing") {
                return Err(invalid("enabled requires the inference-routing feature"));
            }
            #[cfg(feature = "inference-routing")]
            crate::gateway::scheduling::Scheduler::capacity(self)?;
            if self.destinations.is_empty() {
                return Err(invalid("enabled requires at least one destination"));
            }
        }
        Ok(())
    }

    pub(crate) fn validate_destinations(&self, config: &super::Config) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if !matches!(
            config.gateway_state,
            super::gateway_state::GatewayStateConfig::Local {}
        ) {
            return Err(invalid("shared gateway scheduling is unsupported"));
        }
        if config.tenancy.enabled {
            return Ok(());
        }
        for destination in &self.destinations {
            let eligible = [
                bitrouter_ai::types::ApiProtocol::ChatCompletions,
                bitrouter_ai::types::ApiProtocol::Messages,
                bitrouter_ai::types::ApiProtocol::Responses,
            ]
            .into_iter()
            .any(|protocol| {
                let prefs = crate::gateway::pipeline::routing::RoutingPrefs {
                    inbound_protocol: Some(protocol),
                    ..Default::default()
                };
                super::routing_table::resolve_clean_route_chain(
                    config,
                    &format!("{}:{}", destination.provider, destination.model),
                    &prefs,
                )
                .is_ok_and(|targets| {
                    targets.iter().any(|target| {
                        target.provider_name == destination.provider
                            && target.service_id == destination.model
                            && target.effective_api_base() == destination.endpoint
                            && target.account_label == destination.account
                            && matches!(
                                target.api_protocol,
                                bitrouter_ai::types::ApiProtocol::ChatCompletions
                                    | bitrouter_ai::types::ApiProtocol::Messages
                                    | bitrouter_ai::types::ApiProtocol::Responses
                            )
                    })
                })
            });
            if !eligible {
                return Err(invalid(
                    "destination does not resolve to an eligible configured target",
                ));
            }
        }
        Ok(())
    }

    /// Only policy and time budgets may change without replacing the capacity pool.
    pub(crate) fn restart_required(&self, candidate: &Self) -> bool {
        let mut hot = self.clone();
        hot.enabled = candidate.enabled;
        hot.policy = candidate.policy;
        hot.tenant_weights.clone_from(&candidate.tenant_weights);
        hot.queue.wait_timeout_ms = candidate.queue.wait_timeout_ms;
        hot.queue.request_timeout_ms = candidate.queue.request_timeout_ms;
        hot.prefix.ttl_ms = candidate.prefix.ttl_ms;
        hot != *candidate
    }

    pub(crate) fn validate_settings(&self) -> Result<()> {
        let q = &self.queue;
        let l = &self.limits;
        let p = &self.prefix;
        if [
            q.max_requests,
            q.max_bytes,
            q.per_tenant_requests,
            q.per_tenant_bytes,
            l.max_active,
            l.max_uncertain,
            l.max_state_entries,
            l.max_state_bytes,
            p.max_entries,
            p.max_bytes,
        ]
        .contains(&0)
            || [
                q.wait_timeout_ms,
                q.request_timeout_ms,
                l.uncertainty_ttl_ms,
                p.ttl_ms,
            ]
            .iter()
            .any(|value| !(1..=3_600_000).contains(value))
            || q.per_tenant_requests > q.max_requests
            || q.per_tenant_bytes > q.max_bytes
            || q.wait_timeout_ms > q.request_timeout_ms
            || l.max_uncertain > l.max_active
            || !(256..=1_048_576).contains(&p.max_projection_bytes)
            || p.max_projection_bytes > p.max_bytes
            || self.destinations.len() > 4096
            || self.tenant_weights.len() > 128
            || self
                .tenant_weights
                .iter()
                .any(|(id, weight)| !bounded(id, 128) || !(1..=100).contains(weight))
        {
            return Err(invalid(
                "invalid local budgets, durations or tenant weights",
            ));
        }
        let mut ids = HashSet::new();
        let mut destinations = HashSet::new();
        for d in &self.destinations {
            let mut id = d.id.bytes();
            if !bounded(&d.id, 128)
                || !id.next().is_some_and(|c| c.is_ascii_alphanumeric())
                || !id.all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
                || !bounded(&d.provider, 128)
                || !bounded(&d.model, 512)
                || d.account.as_ref().is_some_and(|value| !bounded(value, 128))
                || d.max_active == 0
                || d.max_active > l.max_active
                || !ids.insert(&d.id)
                || !destinations.insert((&d.provider, &d.model, &d.endpoint, &d.account))
            {
                return Err(invalid(
                    "invalid or duplicate destination identity/capacity",
                ));
            }
            let endpoint = url::Url::parse(&d.endpoint)
                .map_err(|_| invalid("invalid destination endpoint"))?;
            if d.endpoint.chars().count() > 2048
                || !matches!(endpoint.scheme(), "http" | "https")
                || endpoint.host_str().is_none()
                || !endpoint.username().is_empty()
                || endpoint.password().is_some()
                || endpoint.query().is_some()
                || endpoint.fragment().is_some()
            {
                return Err(invalid(
                    "destination endpoint requires absolute HTTP(S) without credentials, query or fragment",
                ));
            }
            if let Some(engine) = &d.engine {
                validate_engine(engine, &endpoint)?;
            }
        }
        Ok(())
    }
}

fn validate_engine(engine: &EngineConfig, endpoint: &url::Url) -> Result<()> {
    if !(1..=60_000).contains(&engine.max_metric_age_ms)
        || engine.max_running > 100_000
        || engine.max_waiting > 100_000
        || engine.metrics_url.chars().count() > 2048
        || engine
            .metrics_bearer_env
            .as_ref()
            .is_some_and(|name| !bearer_env_name(name))
    {
        return Err(invalid("invalid engine ceilings or metrics bearer"));
    }
    let metrics =
        url::Url::parse(&engine.metrics_url).map_err(|_| invalid("invalid engine metrics url"))?;
    if !matches!(metrics.scheme(), "http" | "https")
        || metrics.host().is_none()
        || !metrics.username().is_empty()
        || metrics.password().is_some()
        || metrics.query().is_some()
        || metrics.fragment().is_some()
        || !same_engine_origin(endpoint, &metrics)
    {
        return Err(invalid(
            "engine metrics url must share the destination endpoint scheme, host and port",
        ));
    }
    Ok(())
}

fn bearer_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_uppercase() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') && name.len() <= 128
}

fn same_engine_origin(endpoint: &url::Url, metrics: &url::Url) -> bool {
    endpoint.scheme() == metrics.scheme()
        && endpoint.port_or_known_default() == metrics.port_or_known_default()
        && match (endpoint.host(), metrics.host()) {
            (Some(url::Host::Domain(left)), Some(url::Host::Domain(right))) => {
                left.eq_ignore_ascii_case(right)
            }
            (Some(left), Some(right)) => left == right,
            _ => false,
        }
}

fn bounded(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.chars().count() <= max
}

fn invalid(message: &str) -> BitrouterError {
    BitrouterError::bad_request(format!("inference_routing: {message}"))
}

#[cfg(test)]
mod tests {
    use super::InferenceRoutingConfig;

    fn engine(metrics: &str) -> serde_json::Value {
        serde_json::json!({
            "module": "vllm",
            "metrics_url": metrics,
            "max_metric_age_ms": 1000,
            "max_running": 0,
            "max_waiting": 1,
            "max_kv_usage": 0.5
        })
    }

    fn config(metrics: &str) -> serde_json::Result<InferenceRoutingConfig> {
        serde_json::from_value(serde_json::json!({
            "enabled": false,
            "destinations": [{
                "id": "local-a",
                "provider": "local",
                "model": "model",
                "endpoint": "http://127.0.0.1:8000/v1",
                "engine": engine(metrics)
            }]
        }))
    }

    #[test]
    fn metrics_url_must_share_the_endpoint_origin() -> std::result::Result<(), String> {
        let same = config("http://127.0.0.1:8000/metrics").map_err(|error| error.to_string())?;
        same.validate().map_err(|error| error.to_string())?;
        let shifted = config("http://127.0.0.1:9000/metrics").map_err(|error| error.to_string())?;
        assert!(shifted.validate().is_err());
        Ok(())
    }
}
