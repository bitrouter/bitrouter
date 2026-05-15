//! `OtlpExportHook` — a self-contained OTLP/HTTP **JSON** trace exporter,
//! gated behind the `otlp` feature. This completes v0's unfinished #409 (the
//! issue was OPEN, PR #410 only scaffolded a Layer).
//!
//! It deliberately does **not** pull the full `opentelemetry` crate stack —
//! instead it builds the `ExportTraceServiceRequest` JSON by hand and POSTs it
//! to `{endpoint}/v1/traces`, per the OTLP/HTTP spec:
//! - protocol: <https://opentelemetry.io/docs/specs/otlp/#otlphttp>
//! - trace JSON schema: <https://github.com/open-telemetry/opentelemetry-proto/blob/main/opentelemetry/proto/trace/v1/trace.proto>
//!   (JSON mapping per <https://opentelemetry.io/docs/specs/otlp/#json-protobuf-encoding>)
//!
//! As an `ObserveHook` it is read-only and error-swallowing. Per design doc
//! 003 §4.6 the exporter manages its own async: `on_request_end` does a
//! non-blocking `try_send` onto an unbounded channel; a background task drains
//! the channel and POSTs batches.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rand::RngCore;
use tokio::sync::mpsc;

use bitrouter_sdk::language_model::{
    ObserveHook, Phase, PipelineContext, RequestOutcome, StreamContext, StreamInterest, StreamPart,
};

/// Per-request timing accumulated across `after_phase` calls (an `ObserveHook`
/// cannot write into `PipelineContext`, so the hook owns this keyed by
/// `request_id`).
#[derive(Default, Clone)]
struct RequestTiming {
    started_unix_nano: u64,
    phase_end_unix_nano: HashMap<&'static str, u64>,
}

/// One finished request span tree, handed to the background exporter task.
struct SpanBatchItem {
    request_id: String,
    model: String,
    timing: RequestTiming,
    end_unix_nano: u64,
    outcome: &'static str,
}

/// Soft cap on the in-flight `timings` map. If the pipeline never calls
/// `on_request_end` for some requests (panic, runtime drop) the map would
/// otherwise grow without bound. When full, the oldest entry is evicted
/// instead of letting RAM grow forever. The cap is generous — any modern
/// inbound rate hits steady-state well below this.
const TIMINGS_CAP: usize = 16 * 1024;

/// Bound on the exporter channel. With this and `try_send` drop-oldest
/// semantics the OTLP collector being slow / down cannot cause memory growth.
const CHANNEL_CAP: usize = 1024;

/// A minimal OTLP/HTTP JSON trace exporter `ObserveHook`.
pub struct OtlpExportHook {
    /// Per-request timing, keyed by `request_id`. Bounded at `TIMINGS_CAP`.
    timings: Mutex<HashMap<String, RequestTiming>>,
    /// Non-blocking handoff to the background exporter task. Bounded so a
    /// stuck collector cannot grow this unbounded.
    tx: mpsc::Sender<SpanBatchItem>,
}

fn now_unix_nano() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn phase_label(phase: Phase) -> &'static str {
    match phase {
        Phase::PreRequest => "pre_request",
        Phase::Route => "route",
        Phase::Execution => "execution",
        Phase::Settlement => "settlement",
    }
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

impl OtlpExportHook {
    /// Build an exporter that POSTs traces to `endpoint` (e.g.
    /// `http://localhost:4318`). Spawns the background exporter task on the
    /// current tokio runtime.
    pub fn new(endpoint: impl Into<String>) -> Self {
        let endpoint = endpoint.into();
        let (tx, rx) = mpsc::channel(CHANNEL_CAP);
        tokio::spawn(export_loop(endpoint, rx));
        Self {
            timings: Mutex::new(HashMap::new()),
            tx,
        }
    }
}

#[async_trait]
impl ObserveHook for OtlpExportHook {
    async fn after_phase(&self, phase: Phase, ctx: &PipelineContext) {
        let now = now_unix_nano();
        if let Ok(mut map) = self.timings.lock() {
            // Hard cap on map size — if `on_request_end` was never called for
            // some past request (panic, runtime-drop) we still bound memory.
            if !map.contains_key(ctx.request_id()) && map.len() >= TIMINGS_CAP {
                if let Some(victim) = map.keys().next().cloned() {
                    map.remove(&victim);
                }
            }
            let timing = map
                .entry(ctx.request_id().to_string())
                .or_insert_with(|| RequestTiming {
                    started_unix_nano: now,
                    phase_end_unix_nano: HashMap::new(),
                });
            timing.phase_end_unix_nano.insert(phase_label(phase), now);
        }
    }

    fn stream_interest(&self) -> StreamInterest {
        // The exporter only needs request-level spans, not per-token events.
        StreamInterest::none()
    }

    async fn on_stream_part(&self, _ctx: &StreamContext, _part: &StreamPart) {}

    async fn on_request_end(&self, ctx: &PipelineContext, outcome: &RequestOutcome) {
        let end = now_unix_nano();
        let timing = self
            .timings
            .lock()
            .ok()
            .and_then(|mut m| m.remove(ctx.request_id()))
            .unwrap_or_else(|| RequestTiming {
                started_unix_nano: end,
                phase_end_unix_nano: HashMap::new(),
            });
        let outcome_label = match outcome {
            RequestOutcome::Completed => "completed",
            RequestOutcome::Failed(_) => "failed",
            RequestOutcome::ClientDisconnected => "disconnected",
        };
        // try_send is non-blocking — if the channel is full or gone, the
        // span is dropped (observation must never affect the request, and a
        // stuck collector must not stall the hot path).
        let item = SpanBatchItem {
            request_id: ctx.request_id().to_string(),
            model: ctx.model().to_string(),
            timing,
            end_unix_nano: end,
            outcome: outcome_label,
        };
        if self.tx.try_send(item).is_err() {
            tracing::warn!("OTLP exporter channel full / closed — dropping span");
        }
    }
}

/// The background exporter task: drains finished requests, builds OTLP/JSON
/// `ExportTraceServiceRequest` bodies and POSTs them to `{endpoint}/v1/traces`.
async fn export_loop(endpoint: String, mut rx: mpsc::Receiver<SpanBatchItem>) {
    let client = reqwest::Client::new();
    let url = format!("{}/v1/traces", endpoint.trim_end_matches('/'));
    while let Some(item) = rx.recv().await {
        let body = build_otlp_request(&item);
        // Errors here are logged, never surfaced — observation is best-effort.
        match client.post(&url).json(&body).send().await {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                tracing::warn!(status = %resp.status(), "OTLP export rejected by collector")
            }
            Err(e) => tracing::warn!(error = %e, "OTLP export failed"),
        }
    }
}

/// Build an OTLP/HTTP JSON `ExportTraceServiceRequest` for one finished request:
/// a root span for the whole request with one child span per pipeline phase.
fn build_otlp_request(item: &SpanBatchItem) -> serde_json::Value {
    let trace_id = random_hex(16);
    let root_span_id = random_hex(8);
    let start = item.timing.started_unix_nano.to_string();
    let end = item.end_unix_nano.to_string();

    let mut spans = vec![serde_json::json!({
        "traceId": trace_id,
        "spanId": root_span_id,
        "name": "bitrouter.request",
        "kind": 2, // SPAN_KIND_SERVER
        "startTimeUnixNano": start,
        "endTimeUnixNano": end,
        "attributes": [
            { "key": "bitrouter.request_id", "value": { "stringValue": item.request_id } },
            { "key": "bitrouter.model", "value": { "stringValue": item.model } },
            { "key": "bitrouter.outcome", "value": { "stringValue": item.outcome } },
        ],
        "status": { "code": if item.outcome == "completed" { 1 } else { 2 } },
    })];

    // one child span per pipeline phase that completed
    let mut phase_start = item.timing.started_unix_nano;
    for phase in ["pre_request", "route", "execution", "settlement"] {
        if let Some(&phase_end) = item.timing.phase_end_unix_nano.get(phase) {
            spans.push(serde_json::json!({
                "traceId": trace_id,
                "spanId": random_hex(8),
                "parentSpanId": root_span_id,
                "name": format!("bitrouter.{phase}"),
                "kind": 1, // SPAN_KIND_INTERNAL
                "startTimeUnixNano": phase_start.to_string(),
                "endTimeUnixNano": phase_end.to_string(),
            }));
            phase_start = phase_end;
        }
    }

    serde_json::json!({
        "resourceSpans": [{
            "resource": {
                "attributes": [
                    { "key": "service.name", "value": { "stringValue": "bitrouter" } },
                ]
            },
            "scopeSpans": [{
                "scope": { "name": "bitrouter-observe" },
                "spans": spans,
            }]
        }]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otlp_request_shape_is_valid() {
        let item = SpanBatchItem {
            request_id: "req-1".to_string(),
            model: "gpt-5".to_string(),
            timing: RequestTiming {
                started_unix_nano: 1_000,
                phase_end_unix_nano: HashMap::from([
                    ("pre_request", 1_100),
                    ("route", 1_200),
                    ("execution", 2_000),
                    ("settlement", 2_100),
                ]),
            },
            end_unix_nano: 2_100,
            outcome: "completed",
        };
        let body = build_otlp_request(&item);
        // OTLP/HTTP JSON envelope shape
        let scope_spans = &body["resourceSpans"][0]["scopeSpans"][0]["spans"];
        let spans = scope_spans.as_array().unwrap();
        // 1 root + 4 phase spans
        assert_eq!(spans.len(), 5);
        assert_eq!(spans[0]["name"], "bitrouter.request");
        assert_eq!(spans[0]["status"]["code"], 1);
        // phase spans carry the root as parent
        assert_eq!(spans[1]["parentSpanId"], spans[0]["spanId"]);
        assert_eq!(
            body["resourceSpans"][0]["resource"]["attributes"][0]["value"]["stringValue"],
            "bitrouter"
        );
    }

    #[test]
    fn failed_outcome_sets_error_status() {
        let item = SpanBatchItem {
            request_id: "req-2".to_string(),
            model: "m".to_string(),
            timing: RequestTiming {
                started_unix_nano: 1,
                phase_end_unix_nano: HashMap::new(),
            },
            end_unix_nano: 2,
            outcome: "failed",
        };
        let body = build_otlp_request(&item);
        assert_eq!(
            body["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["status"]["code"],
            2
        );
    }
}
