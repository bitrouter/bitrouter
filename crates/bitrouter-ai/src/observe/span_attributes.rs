//! Additional observation attributes supplied by a host or extension.
//!
//! Hosts emit this event during settlement. Exporters apply the schema's
//! reserved-key and scalar-value rules without exposing transport types here.

use crate::event::PipelineEvent;
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
/// `crates/bitrouter-ai/span-schema.json`.
#[derive(Debug, Clone, Serialize)]
pub struct SpanAttributes(pub Map<String, Value>);

impl PipelineEvent for SpanAttributes {
    fn event_name(&self) -> &'static str {
        "observe.span_attributes"
    }
}
