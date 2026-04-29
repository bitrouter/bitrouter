//! Telemetry configuration — OpenTelemetry GenAI export settings.
//!
//! Parsed from the `telemetry:` section of `bitrouter.yaml`. The actual
//! exporter implementation lives in `bitrouter-observe` behind the optional
//! `otlp` feature; this module owns only the config schema and validation.
//!
//! ```yaml
//! telemetry:
//!   capture_tier: metadata        # metadata | capture | audit
//!   destinations:
//!     - name: bitrouter-cloud
//!       endpoint: https://console.bitrouter.io/otlp/v1/traces
//!       headers: { authorization: "Bearer ${BITROUTER_TOKEN}" }
//!       sampling: { rate: 1.0, by: gen_ai.conversation.id }
//!       redact: [gen_ai.input.messages, gen_ai.output.messages]
//! ```
//!
//! `${ENV_VAR}` interpolation in any string value is handled upstream by
//! [`crate::env::substitute_in_value`] before deserialization, so values like
//! `Bearer ${BITROUTER_TOKEN}` arrive here already substituted.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};

/// Environment variable that gates the dangerous `audit` capture tier.
///
/// The `audit` tier captures raw HTTP request and response bodies which
/// frequently contain bearer tokens, API keys, and end-user PII. Requiring
/// an explicit env-var opt-in prevents a copy-pasted YAML snippet from
/// silently exfiltrating that data.
pub const AUDIT_TIER_ENV_GATE: &str = "BITROUTER_ENABLE_AUDIT_TIER";

/// Header names that are always stripped under `audit` tier, regardless of
/// any per-destination `redact` list. Defense in depth against token leaks.
pub const AUDIT_TIER_FORCED_HEADER_SCRUB: &[&str] = &[
    "authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "x-goog-api-key",
    "anthropic-api-key",
    "openai-api-key",
];

/// Top-level telemetry configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelemetryConfig {
    /// What level of detail to attach to spans at the source.
    #[serde(default)]
    pub capture_tier: CaptureTier,

    /// One or more OTLP/HTTP destinations to fan spans out to.
    ///
    /// Empty means the exporter is fully inactive even when the
    /// `bitrouter-observe/otlp` feature is compiled in.
    #[serde(default)]
    pub destinations: Vec<TelemetryDestination>,
}

impl TelemetryConfig {
    /// Returns `true` if at least one destination is configured.
    pub fn is_active(&self) -> bool {
        !self.destinations.is_empty()
    }

    /// Validates the config against runtime invariants that cannot be
    /// expressed in serde (env-var gates, sample-rate bounds, duplicate
    /// destination names). Reads the audit-tier gate from the process
    /// environment.
    pub fn validate(&self) -> Result<()> {
        self.validate_with_audit_gate(std::env::var(AUDIT_TIER_ENV_GATE).is_ok())
    }

    /// Like [`validate`](Self::validate) but with an explicit audit-gate
    /// signal. Lets tests exercise the audit-tier check without mutating
    /// real process env vars.
    pub fn validate_with_audit_gate(&self, audit_gate_enabled: bool) -> Result<()> {
        if self.capture_tier == CaptureTier::Audit && !audit_gate_enabled {
            return Err(ConfigError::ConfigParse(format!(
                "telemetry.capture_tier = audit requires {AUDIT_TIER_ENV_GATE}=1 in the \
                 environment; the audit tier captures raw request/response bodies and is \
                 not safe to enable from YAML alone"
            )));
        }

        let mut seen = std::collections::HashSet::new();
        for dest in &self.destinations {
            if !seen.insert(dest.name.as_str()) {
                return Err(ConfigError::ConfigParse(format!(
                    "telemetry.destinations contains duplicate name '{}'",
                    dest.name
                )));
            }
            if dest.endpoint.is_empty() {
                return Err(ConfigError::ConfigParse(format!(
                    "telemetry.destinations[{}].endpoint is empty",
                    dest.name
                )));
            }
            let rate = dest.sampling.rate;
            if !(0.0..=1.0).contains(&rate) || rate.is_nan() {
                return Err(ConfigError::ConfigParse(format!(
                    "telemetry.destinations[{}].sampling.rate = {rate} is outside [0.0, 1.0]",
                    dest.name
                )));
            }
        }
        Ok(())
    }
}

/// Source-side capture tier. Gates what attributes the exporter constructs
/// on each span; per-destination [`TelemetryDestination::redact`] then
/// strips attributes from the export payload.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureTier {
    /// Span carries provider, model, route, latency, token counts, errors.
    /// Never includes prompts or responses. Default.
    #[default]
    Metadata,
    /// Plus content attributes (`gen_ai.input.messages`,
    /// `gen_ai.output.messages`, `gen_ai.system_instructions`,
    /// `gen_ai.tool.call.arguments`, `gen_ai.tool.call.result`).
    Capture,
    /// Plus raw HTTP request/response bodies. Requires
    /// [`AUDIT_TIER_ENV_GATE`]; auth headers are auto-scrubbed. Debug only.
    Audit,
}

/// One OTLP/HTTP destination spans are fanned out to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryDestination {
    /// Human-readable identifier used in `bitrouter telemetry status` output
    /// and in error messages. Must be unique within `destinations`.
    pub name: String,

    /// OTLP/HTTP endpoint URL (the full path, e.g. `.../v1/traces`).
    pub endpoint: String,

    /// HTTP headers added to every export request. Typical use: bearer token
    /// or vendor-specific API key. `${ENV_VAR}` interpolation happens at
    /// config-load time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,

    /// Sampling decision applied per-trace before export.
    #[serde(default)]
    pub sampling: TelemetrySampling,

    /// Attribute names to strip from spans before sending to this
    /// destination. Lets one destination receive full content while another
    /// receives metadata only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redact: Vec<String>,
}

/// Deterministic sampling configuration.
///
/// Hashing on a stable key (default `gen_ai.conversation.id`) ensures every
/// span of a single session ships together — random per-span sampling would
/// fragment traces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetrySampling {
    /// Fraction of traces to keep, in `[0.0, 1.0]`.
    #[serde(default = "default_sampling_rate")]
    pub rate: f64,

    /// Span attribute name whose value is hashed for the sampling decision.
    /// Spans missing this attribute are always kept (better to over-collect
    /// than to silently drop unattributed traffic).
    #[serde(default = "default_sampling_by")]
    pub by: String,
}

impl Default for TelemetrySampling {
    fn default() -> Self {
        Self {
            rate: default_sampling_rate(),
            by: default_sampling_by(),
        }
    }
}

fn default_sampling_rate() -> f64 {
    1.0
}

fn default_sampling_by() -> String {
    "gen_ai.conversation.id".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest(name: &str, endpoint: &str) -> TelemetryDestination {
        TelemetryDestination {
            name: name.to_owned(),
            endpoint: endpoint.to_owned(),
            headers: None,
            sampling: TelemetrySampling::default(),
            redact: vec![],
        }
    }

    #[test]
    fn default_config_is_inactive() {
        let cfg = TelemetryConfig::default();
        assert!(!cfg.is_active());
        assert_eq!(cfg.capture_tier, CaptureTier::Metadata);
    }

    #[test]
    fn validate_accepts_simple_destination() {
        let cfg = TelemetryConfig {
            capture_tier: CaptureTier::Metadata,
            destinations: vec![dest("cloud", "https://example.com/v1/traces")],
        };
        cfg.validate_with_audit_gate(false).unwrap();
    }

    #[test]
    fn validate_rejects_duplicate_destination_names() {
        let cfg = TelemetryConfig {
            capture_tier: CaptureTier::Metadata,
            destinations: vec![
                dest("cloud", "https://example.com/v1/traces"),
                dest("cloud", "https://other.example.com/v1/traces"),
            ],
        };
        let err = cfg.validate_with_audit_gate(false).unwrap_err().to_string();
        assert!(err.contains("duplicate"), "got: {err}");
    }

    #[test]
    fn validate_rejects_empty_endpoint() {
        let cfg = TelemetryConfig {
            capture_tier: CaptureTier::Metadata,
            destinations: vec![dest("cloud", "")],
        };
        let err = cfg.validate_with_audit_gate(false).unwrap_err().to_string();
        assert!(err.contains("endpoint"), "got: {err}");
    }

    #[test]
    fn validate_rejects_out_of_range_sampling_rate() {
        let mut d = dest("cloud", "https://example.com/v1/traces");
        d.sampling.rate = 1.5;
        let cfg = TelemetryConfig {
            capture_tier: CaptureTier::Metadata,
            destinations: vec![d],
        };
        let err = cfg.validate_with_audit_gate(false).unwrap_err().to_string();
        assert!(err.contains("sampling.rate"), "got: {err}");
    }

    #[test]
    fn validate_rejects_audit_tier_without_env_gate() {
        let cfg = TelemetryConfig {
            capture_tier: CaptureTier::Audit,
            destinations: vec![dest("debug", "http://localhost:4318/v1/traces")],
        };
        let err = cfg.validate_with_audit_gate(false).unwrap_err().to_string();
        assert!(err.contains(AUDIT_TIER_ENV_GATE), "got: {err}");
    }

    #[test]
    fn validate_accepts_audit_tier_with_env_gate() {
        let cfg = TelemetryConfig {
            capture_tier: CaptureTier::Audit,
            destinations: vec![dest("debug", "http://localhost:4318/v1/traces")],
        };
        cfg.validate_with_audit_gate(true).unwrap();
    }

    #[test]
    fn deserializes_capture_tier_lowercase() {
        for (s, expected) in [
            ("\"metadata\"", CaptureTier::Metadata),
            ("\"capture\"", CaptureTier::Capture),
            ("\"audit\"", CaptureTier::Audit),
        ] {
            let tier: CaptureTier = serde_json::from_str(s).unwrap();
            assert_eq!(tier, expected);
        }
    }

    #[test]
    fn deserializes_full_destination_block() {
        let json = serde_json::json!({
            "name": "honeycomb",
            "endpoint": "https://api.honeycomb.io/v1/traces",
            "headers": { "x-honeycomb-team": "key123" },
            "sampling": { "rate": 0.1, "by": "gen_ai.conversation.id" },
            "redact": ["gen_ai.input.messages"]
        });
        let d: TelemetryDestination = serde_json::from_value(json).unwrap();
        assert_eq!(d.name, "honeycomb");
        assert_eq!(d.endpoint, "https://api.honeycomb.io/v1/traces");
        assert_eq!(d.headers.as_ref().unwrap().len(), 1);
        assert!((d.sampling.rate - 0.1).abs() < f64::EPSILON);
        assert_eq!(d.redact, vec!["gen_ai.input.messages"]);
    }

    #[test]
    fn destination_uses_default_sampling_when_omitted() {
        let json = serde_json::json!({
            "name": "minimal",
            "endpoint": "https://example.com/v1/traces"
        });
        let d: TelemetryDestination = serde_json::from_value(json).unwrap();
        assert!((d.sampling.rate - 1.0).abs() < f64::EPSILON);
        assert_eq!(d.sampling.by, "gen_ai.conversation.id");
        assert!(d.redact.is_empty());
    }
}
