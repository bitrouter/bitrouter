//! [`SpanAttributes`] — a generic, serialize-only pipeline event that carries
//! extra span attributes forward to the OTel exporter's `on_request_end`.
//!
//! A deployment (e.g. `bitrouter-cloud`) that computes attributes the SDK does
//! not know about — request cost, namespace, routing profile — emits a
//! `SpanAttributes` from its [`SettlementRecorder`]; the exporter stamps every
//! entry onto the request's root `chat` span. Keeping it a plain
//! `serde_json::Map` means no backend concept (billing, tenancy, …) leaks into
//! the SDK or this plugin: every future attribute rides for free, named by the
//! emitter (e.g. PostHog's `$ai_total_cost_usd`).
//!
//! [`SettlementRecorder`]: bitrouter_sdk::language_model::SettlementRecorder

use bitrouter_sdk::PipelineEvent;
use serde::Serialize;
use serde_json::{Map, Value};

/// Extra span attributes forwarded from a settlement recorder to the OTel
/// exporter. The map's keys are used verbatim as span-attribute keys; values
/// are stamped by JSON type (string / bool / integer / float). Null and nested
/// JSON (array / object) are skipped — OTel attribute values are scalar.
#[derive(Debug, Clone, Serialize)]
pub struct SpanAttributes(pub Map<String, Value>);

impl PipelineEvent for SpanAttributes {
    fn event_name(&self) -> &'static str {
        "observe.span_attributes"
    }
}
