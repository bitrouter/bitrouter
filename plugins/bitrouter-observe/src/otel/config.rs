//! Configuration for the OpenTelemetry exporter.
//!
//! The exporter is OTLP/HTTP+protobuf only. HTTP/JSON and gRPC are left for
//! a follow-up (the issue lists them; the previous draft shipped a config
//! variant that hard-errored at runtime, which is worse than absent).
//!
//! All standard `OTEL_*` env vars take precedence over YAML, matching the
//! upstream OTel SDK spec: <https://opentelemetry.io/docs/specs/otel/configuration/sdk-environment-variables/>.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// OpenTelemetry exporter configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OtelConfig {
    /// OTLP/HTTP+protobuf endpoint. Defaults to `OTEL_EXPORTER_OTLP_ENDPOINT`
    /// env var, then to the OTel-spec default `http://localhost:4318`.
    pub endpoint: String,

    /// Additional headers (e.g. vendor API keys).
    pub headers: HashMap<String, String>,

    /// Service name reported on the resource. Defaults to
    /// `OTEL_SERVICE_NAME` env var, then to `"bitrouter"`.
    pub service_name: String,

    /// Extra resource attributes (k=v list, merged from
    /// `OTEL_RESOURCE_ATTRIBUTES` env var).
    pub resource_attributes: HashMap<String, String>,

    /// Sampler kind; matches the OTel-spec `OTEL_TRACES_SAMPLER` values.
    pub sampler: SamplerKind,

    /// Argument to the sampler (only used by `traceidratio` and
    /// `parentbased_traceidratio`; ignored otherwise).
    pub sampler_arg: Option<f64>,

    /// Trace configuration.
    pub traces: TraceConfig,

    /// Metrics configuration.
    pub metrics: MetricsConfig,
}

/// Subset of OTel-spec sampler kinds the SDK actually supports. The default
/// (`parentbased_always_on`) matches the OTel-spec default — every trace is
/// sampled unless the inbound `traceparent` says otherwise.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SamplerKind {
    AlwaysOn,
    AlwaysOff,
    #[serde(rename = "traceidratio")]
    TraceIdRatio,
    #[serde(rename = "parentbased_always_on")]
    ParentBasedAlwaysOn,
    #[serde(rename = "parentbased_always_off")]
    ParentBasedAlwaysOff,
    #[serde(rename = "parentbased_traceidratio")]
    ParentBasedTraceIdRatio,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TraceConfig {
    /// Batch processor configuration.
    pub batch: BatchConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BatchConfig {
    /// Maximum queue size before drop.
    pub max_queue_size: usize,

    /// Scheduled flush interval in milliseconds.
    pub flush_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricsConfig {
    /// Enable metrics export.
    pub enabled: bool,

    /// Export interval in milliseconds.
    pub export_interval_ms: u64,

    /// Cardinality cap for the `api_key_id` *metric* dimension. Spans always
    /// carry the raw value — capping only applies to metrics, where unbounded
    /// label cardinality is a real problem.
    pub api_key_id_cap: usize,

    /// Cardinality cap for the `user_id` metric dimension.
    pub user_id_cap: usize,
}

impl Default for OtelConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:4318".to_string(),
            headers: HashMap::new(),
            service_name: "bitrouter".to_string(),
            resource_attributes: HashMap::new(),
            sampler: SamplerKind::ParentBasedAlwaysOn,
            sampler_arg: None,
            traces: TraceConfig::default(),
            metrics: MetricsConfig::default(),
        }
    }
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            max_queue_size: 2048,
            flush_ms: 5000,
        }
    }
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            export_interval_ms: 60000,
            api_key_id_cap: 1024,
            user_id_cap: 256,
        }
    }
}

impl OtelConfig {
    /// Apply OTel-spec env-var overrides from the process environment. Env
    /// vars take precedence over YAML, matching the upstream SDK contract.
    pub fn with_env_overrides(self) -> Self {
        self.with_env_from(|k| std::env::var(k).ok())
    }

    /// Apply OTel-spec env-var overrides from an arbitrary source. Used by
    /// `with_env_overrides` against the process env, and by tests against a
    /// HashMap — tests can't mutate the process env here because
    /// `env::set_var` is `unsafe` under Rust 2024 and the crate is
    /// `#![forbid(unsafe_code)]`.
    pub fn with_env_from<F>(mut self, lookup: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        if let Some(endpoint) = lookup("OTEL_EXPORTER_OTLP_ENDPOINT") {
            self.endpoint = endpoint;
        }
        if let Some(headers_str) = lookup("OTEL_EXPORTER_OTLP_HEADERS") {
            for pair in headers_str.split(',') {
                if let Some((k, v)) = pair.split_once('=') {
                    self.headers
                        .insert(k.trim().to_string(), v.trim().to_string());
                }
            }
        }
        if let Some(name) = lookup("OTEL_SERVICE_NAME") {
            self.service_name = name;
        }
        if let Some(attrs) = lookup("OTEL_RESOURCE_ATTRIBUTES") {
            for pair in attrs.split(',') {
                if let Some((k, v)) = pair.split_once('=') {
                    self.resource_attributes
                        .insert(k.trim().to_string(), v.trim().to_string());
                }
            }
        }
        if let Some(s) = lookup("OTEL_TRACES_SAMPLER")
            && let Some(kind) = parse_sampler(&s)
        {
            self.sampler = kind;
        }
        if let Some(arg) = lookup("OTEL_TRACES_SAMPLER_ARG")
            && let Ok(ratio) = arg.parse::<f64>()
        {
            self.sampler_arg = Some(ratio);
        }
        self
    }
}

fn parse_sampler(s: &str) -> Option<SamplerKind> {
    match s {
        "always_on" => Some(SamplerKind::AlwaysOn),
        "always_off" => Some(SamplerKind::AlwaysOff),
        "traceidratio" => Some(SamplerKind::TraceIdRatio),
        "parentbased_always_on" => Some(SamplerKind::ParentBasedAlwaysOn),
        "parentbased_always_off" => Some(SamplerKind::ParentBasedAlwaysOff),
        "parentbased_traceidratio" => Some(SamplerKind::ParentBasedTraceIdRatio),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn env_overrides_endpoint_and_service_name() {
        let cfg = OtelConfig::default().with_env_from(env(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318"),
            ("OTEL_SERVICE_NAME", "myrouter"),
        ]));
        assert_eq!(cfg.endpoint, "http://collector:4318");
        assert_eq!(cfg.service_name, "myrouter");
    }

    #[test]
    fn env_overrides_resource_attributes_and_sampler() {
        let cfg = OtelConfig::default().with_env_from(env(&[
            (
                "OTEL_RESOURCE_ATTRIBUTES",
                "deployment.environment=prod,team=infra",
            ),
            ("OTEL_TRACES_SAMPLER", "parentbased_traceidratio"),
            ("OTEL_TRACES_SAMPLER_ARG", "0.25"),
        ]));
        assert_eq!(
            cfg.resource_attributes
                .get("deployment.environment")
                .map(String::as_str),
            Some("prod"),
        );
        assert_eq!(
            cfg.resource_attributes.get("team").map(String::as_str),
            Some("infra")
        );
        assert_eq!(cfg.sampler, SamplerKind::ParentBasedTraceIdRatio);
        assert_eq!(cfg.sampler_arg, Some(0.25));
    }

    #[test]
    fn env_overrides_headers_parses_comma_list() {
        let cfg = OtelConfig::default().with_env_from(env(&[(
            "OTEL_EXPORTER_OTLP_HEADERS",
            "x-honeycomb-team=secret,x-team=infra",
        )]));
        assert_eq!(
            cfg.headers.get("x-honeycomb-team").map(String::as_str),
            Some("secret")
        );
        assert_eq!(cfg.headers.get("x-team").map(String::as_str), Some("infra"));
    }
}
