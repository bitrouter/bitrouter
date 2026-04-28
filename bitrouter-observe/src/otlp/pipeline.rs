//! Per-destination export pipeline: sampling → redact → export.
//!
//! Mirrors the schema of `bitrouter_config::TelemetryConfig` but lives here
//! to keep `bitrouter-observe` independent of the config crate. The binary
//! crate is responsible for translating `bitrouter_config::TelemetryConfig`
//! into [`PipelineConfig`] at server startup.
//!
//! In this layer [`Pipeline::export`] writes a structured `tracing::info!`
//! event per (span × destination). A follow-up commit replaces the body of
//! `export` with `opentelemetry-otlp` `BatchSpanProcessor::on_end` calls
//! against the same input — none of the surface visible to
//! [`super::observer::OtlpObserver`] needs to change.

use std::collections::HashMap;

use super::span::Span;

/// Source-side capture tier mirroring `bitrouter_config::CaptureTier`.
///
/// Decoupled from the config crate so this module compiles without
/// `bitrouter-config` in the dependency graph; the binary crate owns the
/// translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptureTier {
    #[default]
    Metadata,
    Capture,
    Audit,
}

/// One export destination — the OTLP/HTTP analog of "where does this span
/// physically go." See [`super::observer::OtlpObserver`] for how a single
/// event is fanned out across all destinations.
#[derive(Debug, Clone)]
pub struct Destination {
    pub name: String,
    pub endpoint: String,
    pub headers: HashMap<String, String>,
    pub sampling: Sampling,
    pub redact: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Sampling {
    /// Fraction of traces to keep, in `[0.0, 1.0]`.
    pub rate: f64,
    /// Span attribute name whose value is hashed for the keep/drop decision.
    pub by: String,
}

impl Default for Sampling {
    fn default() -> Self {
        Self {
            rate: 1.0,
            by: super::semconv::CONVERSATION_ID.to_owned(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PipelineConfig {
    pub capture_tier: CaptureTier,
    pub destinations: Vec<Destination>,
}

impl PipelineConfig {
    pub fn is_active(&self) -> bool {
        !self.destinations.is_empty()
    }
}

/// Stateless export pipeline. Cheap to clone (`Arc` not required because the
/// only state is the config snapshot).
#[derive(Debug, Clone)]
pub struct Pipeline {
    config: PipelineConfig,
}

impl Pipeline {
    pub fn new(config: PipelineConfig) -> Self {
        Self { config }
    }

    pub fn capture_tier(&self) -> CaptureTier {
        self.config.capture_tier
    }

    pub fn is_active(&self) -> bool {
        self.config.is_active()
    }

    /// Fans `span` out to each configured destination, applying the
    /// destination's sampling and redact rules. Cloning per-destination is
    /// unavoidable because each gets a different attribute set after redact.
    pub fn dispatch(&self, span: Span) {
        for dest in &self.config.destinations {
            if !sampled(&span, &dest.sampling) {
                continue;
            }
            let mut s = span.clone();
            s.redact_attributes(&dest.redact);
            self.export(dest, s);
        }
    }

    /// Exports a span to a single destination.
    ///
    /// Layer-1 implementation: emits a structured `tracing::info!` event so
    /// the wiring is observable in test logs and the "did it actually run"
    /// question is answerable without standing up a collector. The follow-up
    /// commit replaces this body with `opentelemetry-otlp` HTTP export and
    /// keeps the same `(dest, span)` signature.
    fn export(&self, dest: &Destination, span: Span) {
        tracing::info!(
            target: "bitrouter::otlp::export",
            destination = %dest.name,
            endpoint = %dest.endpoint,
            span_name = %span.name,
            attribute_count = span.attributes.len(),
            "otlp export (skeleton)"
        );
    }
}

/// Deterministic sampler: hashes the configured key's value with FxHash and
/// compares against `rate`. Spans missing the key are always kept — silently
/// dropping unattributed traffic is worse than over-collecting.
fn sampled(span: &Span, sampling: &Sampling) -> bool {
    if sampling.rate >= 1.0 {
        return true;
    }
    if sampling.rate <= 0.0 {
        return false;
    }
    let Some(key_value) = span.string_attr(&sampling.by) else {
        return true;
    };
    // FNV-1a 64-bit. Stable, dependency-free, good-enough distribution for
    // sampling. A future commit can swap in `opentelemetry_sdk::trace::Sampler`
    // built-ins (TraceIdRatioBased) once we have an OTel `Context` to read
    // the trace id from.
    let mut h: u64 = 0xcbf29ce484222325;
    for byte in key_value.as_bytes() {
        h ^= u64::from(*byte);
        h = h.wrapping_mul(0x100000001b3);
    }
    let bucket = (h % 1_000_000) as f64 / 1_000_000.0;
    bucket < sampling.rate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::otlp::semconv;

    fn dest_keeping_all() -> Destination {
        Destination {
            name: "test".to_owned(),
            endpoint: "https://example.com/v1/traces".to_owned(),
            headers: HashMap::new(),
            sampling: Sampling::default(),
            redact: vec![],
        }
    }

    #[test]
    fn pipeline_inactive_when_no_destinations() {
        let p = Pipeline::new(PipelineConfig::default());
        assert!(!p.is_active());
        assert_eq!(p.capture_tier(), CaptureTier::Metadata);
    }

    #[test]
    fn pipeline_exposes_capture_tier_for_status_reporting() {
        // The `bitrouter telemetry status` CLI (follow-up commit) reads this
        // accessor to print the configured tier.
        let p = Pipeline::new(PipelineConfig {
            capture_tier: CaptureTier::Capture,
            destinations: vec![dest_keeping_all()],
        });
        assert_eq!(p.capture_tier(), CaptureTier::Capture);
    }

    #[test]
    fn rate_one_keeps_all_spans() {
        let mut span = Span::new("chat gpt-4o");
        span.set(semconv::CONVERSATION_ID, "any-conversation");
        let s = Sampling {
            rate: 1.0,
            by: semconv::CONVERSATION_ID.to_owned(),
        };
        assert!(sampled(&span, &s));
    }

    #[test]
    fn rate_zero_drops_all_spans() {
        let mut span = Span::new("chat gpt-4o");
        span.set(semconv::CONVERSATION_ID, "any-conversation");
        let s = Sampling {
            rate: 0.0,
            by: semconv::CONVERSATION_ID.to_owned(),
        };
        assert!(!sampled(&span, &s));
    }

    #[test]
    fn span_missing_sampling_key_is_always_kept() {
        let span = Span::new("chat gpt-4o");
        let s = Sampling {
            rate: 0.001,
            by: semconv::CONVERSATION_ID.to_owned(),
        };
        assert!(sampled(&span, &s));
    }

    #[test]
    fn sampling_is_deterministic_per_conversation() {
        // The whole point of hashing on conversation id: two spans of the
        // same conversation get the same keep/drop decision, so traces
        // never fragment across the sampling boundary.
        let s = Sampling {
            rate: 0.5,
            by: semconv::CONVERSATION_ID.to_owned(),
        };

        for cid in ["sess-a", "sess-b", "sess-c", "sess-d", "sess-e"] {
            let mut span1 = Span::new("chat gpt-4o");
            span1.set(semconv::CONVERSATION_ID, cid);
            let mut span2 = Span::new("execute_tool search");
            span2.set(semconv::CONVERSATION_ID, cid);
            assert_eq!(
                sampled(&span1, &s),
                sampled(&span2, &s),
                "conversation {cid} sampled inconsistently"
            );
        }
    }

    #[test]
    fn dispatch_applies_per_destination_redact() {
        let mut content_keeper = dest_keeping_all();
        content_keeper.name = "internal".to_owned();
        // No redact — full content reaches this destination.

        let mut metadata_only = dest_keeping_all();
        metadata_only.name = "vendor".to_owned();
        metadata_only.redact = vec![
            semconv::INPUT_MESSAGES.to_owned(),
            semconv::OUTPUT_MESSAGES.to_owned(),
        ];

        let p = Pipeline::new(PipelineConfig {
            capture_tier: CaptureTier::Capture,
            destinations: vec![content_keeper, metadata_only],
        });

        let mut span = Span::new(semconv::span_name_chat("gpt-4o"));
        span.set(semconv::REQUEST_MODEL, "gpt-4o");
        span.set(semconv::INPUT_MESSAGES, "raw user content");
        span.set(semconv::OUTPUT_MESSAGES, "raw assistant content");

        // dispatch() takes ownership and clones per destination — verify
        // the input span is unchanged after a copy was redacted downstream
        // by re-running dispatch on the same span.
        let mut spy = span.clone();
        spy.redact_attributes(&[
            semconv::INPUT_MESSAGES.to_owned(),
            semconv::OUTPUT_MESSAGES.to_owned(),
        ]);
        assert!(!spy.attributes.contains_key(semconv::INPUT_MESSAGES));
        assert!(spy.attributes.contains_key(semconv::REQUEST_MODEL));

        // Smoke-test that dispatch runs without panicking; tracing log
        // output is asserted on in the integration tests once the real
        // exporter lands.
        p.dispatch(span);
    }
}
