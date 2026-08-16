//! [`SpanAttributes`] — a generic, serialize-only pipeline event that carries
//! extra span attributes forward to the OTel exporter's `on_request_end`.
//!
//! A deployment (e.g. `bitrouter-cloud`) that computes attributes the SDK does
//! not know about — request cost, namespace, routing profile — emits a
//! `SpanAttributes` from its [`SettlementRecorder`]; the exporter stamps every
//! entry onto the request's root `chat` span. Keeping it a plain
//! `serde_json::Map` means no backend concept (billing, tenancy, …) leaks into
//! the SDK: every future attribute rides for free, named by the emitter (e.g.
//! PostHog's `$ai_total_cost_usd`).
//!
//! [`SettlementRecorder`]: crate::language_model::SettlementRecorder

use crate::PipelineEvent;
use serde::Serialize;
use serde_json::{Map, Value};

/// Extra span attributes forwarded from a settlement recorder to the OTel
/// exporter. Keys are used as span-attribute keys; values are stamped by JSON
/// type (string / bool / integer / float). Null and nested JSON (array /
/// object) are skipped — OTel attribute values are scalar.
///
/// **Part of the key space is reserved and will be dropped, not stamped.** The
/// span schema owns the `bitrouter.*` and `gen_ai.*` prefixes, plus every key
/// it already declares on any span (`$screen_name`, `error.type`,
/// `server.address`, `http.route`, …). An entry under one of those is
/// discarded with a DEBUG line on the `bitrouter::observe::http`-style pinned
/// target `bitrouter::observe::span_attributes`, because a deployment that
/// could redefine `bitrouter.*` would make the schema mean something different
/// per deployment — the one property it exists to have. Everything outside
/// that region rides for free, named by the emitter (e.g. PostHog's
/// `$ai_total_cost_usd`).
///
/// The full reserved region is the committed artifact
/// `crates/bitrouter-sdk/span-schema.json`.
#[derive(Debug, Clone, Serialize)]
pub struct SpanAttributes(pub Map<String, Value>);

impl PipelineEvent for SpanAttributes {
    fn event_name(&self) -> &'static str {
        "observe.span_attributes"
    }
}
