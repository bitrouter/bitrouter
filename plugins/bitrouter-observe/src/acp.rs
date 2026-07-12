//! GenAI **agent** spans for the ACP substrate path (`bitrouter acp …`).
//!
//! Maps substrate-shaped events onto the OTel GenAI *agent* semantic
//! conventions (<https://opentelemetry.io/docs/specs/semconv/gen-ai/> —
//! Development status, so attribute churn is expected):
//!
//! - one INTERNAL `invoke_agent <agent>` span per completed prompt turn,
//!   carrying `gen_ai.operation.name = invoke_agent`, `gen_ai.agent.name`,
//!   and `gen_ai.conversation.id` (the session's stable `record_id`);
//! - one INTERNAL `execute_tool <tool>` span per completed tool call.
//!   ACP `session/update`s carry no causal parent ids, so tool spans are
//!   *siblings* of the turn span, correlated by `gen_ai.conversation.id`.
//!
//! Context-window occupancy rides `bitrouter.context.used`/`.size` — the
//! substrate's usage signal reports occupancy, **not** per-turn token deltas,
//! so the `gen_ai.usage.*` token attributes are deliberately not emitted
//! (writing occupancy into them would corrupt token dashboards).
//!
//! `gen_ai.conversation.id` is also the join key to the HTTP LLM-router
//! plane: when the fronted agent routes its model calls through bitrouter,
//! one backend sees both the agent turns (here) and the `chat <model>`
//! generations they caused.
//!
//! The substrate crate stays OTel-free: the CLI maps its own event types onto
//! [`TurnRecord`] / the tool methods here.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use opentelemetry::KeyValue;
use opentelemetry::trace::{Span as _, SpanKind, Status, Tracer as _};

use crate::otel::OtelExporter;

/// One completed prompt turn, as reported by the substrate's telemetry
/// channel.
#[derive(Debug, Clone)]
pub struct TurnRecord {
    /// Stop reason rendered as a string (e.g. `"EndTurn"`).
    pub stop_reason: String,
    /// Wall-clock turn latency; the span's start time is derived from it.
    pub latency: Duration,
    /// Context-window occupancy (tokens in context / window size) as of the
    /// latest upstream usage report, when one was seen.
    pub context_used: Option<u64>,
    pub context_size: Option<u64>,
}

/// A tool call that started but has not reported a terminal status yet.
struct OpenTool {
    started_at: SystemTime,
    title: String,
}

/// Emits GenAI agent spans for one ACP session. Cheap to share (`Arc`);
/// internal state is just the open-tool table.
pub struct AcpSpanRecorder {
    tracer: opentelemetry_sdk::trace::Tracer,
    agent: String,
    conversation_id: String,
    open_tools: Mutex<HashMap<String, OpenTool>>,
}

impl AcpSpanRecorder {
    /// A recorder bound to `exporter`'s tracer for one session: `agent` is
    /// the configured agent id, `conversation_id` the session's `record_id`.
    pub fn new(
        exporter: &OtelExporter,
        agent: impl Into<String>,
        conversation_id: impl Into<String>,
    ) -> Self {
        Self::with_tracer(exporter.tracer_clone(), agent, conversation_id)
    }

    /// Test seam: bind directly to a tracer (the in-process tests capture
    /// spans without an OTLP endpoint).
    pub(crate) fn with_tracer(
        tracer: opentelemetry_sdk::trace::Tracer,
        agent: impl Into<String>,
        conversation_id: impl Into<String>,
    ) -> Self {
        Self {
            tracer,
            agent: agent.into(),
            conversation_id: conversation_id.into(),
            open_tools: Mutex::new(HashMap::new()),
        }
    }

    /// Emit the `invoke_agent <agent>` span for a completed turn. The span's
    /// start time is back-dated by the turn latency so its duration is real.
    pub fn turn_completed(&self, turn: &TurnRecord) {
        let end = SystemTime::now();
        let start = end.checked_sub(turn.latency).unwrap_or(end);
        let mut attrs = vec![
            KeyValue::new("gen_ai.operation.name", "invoke_agent"),
            KeyValue::new("gen_ai.agent.name", self.agent.clone()),
            KeyValue::new("gen_ai.conversation.id", self.conversation_id.clone()),
            KeyValue::new("bitrouter.stop_reason", turn.stop_reason.clone()),
        ];
        if let Some(used) = turn.context_used {
            attrs.push(KeyValue::new(
                "bitrouter.context.used",
                i64::try_from(used).unwrap_or(i64::MAX),
            ));
        }
        if let Some(size) = turn.context_size {
            attrs.push(KeyValue::new(
                "bitrouter.context.size",
                i64::try_from(size).unwrap_or(i64::MAX),
            ));
        }
        let mut span = self
            .tracer
            .span_builder(format!("invoke_agent {}", self.agent))
            .with_kind(SpanKind::Internal)
            .with_start_time(start)
            .with_attributes(attrs)
            .start(&self.tracer);
        span.end_with_timestamp(end);
    }

    /// Record that a tool call started (or was first observed running). The
    /// span is emitted when [`tool_finished`](Self::tool_finished) reports a
    /// terminal status; a tool that never finishes emits no span.
    pub fn tool_started(&self, id: impl Into<String>, title: impl Into<String>) {
        if let Ok(mut open) = self.open_tools.lock() {
            open.entry(id.into()).or_insert(OpenTool {
                started_at: SystemTime::now(),
                title: title.into(),
            });
        }
    }

    /// Emit the `execute_tool <tool>` span for a terminally-reported tool
    /// call. `title` refines the name when the terminal update carries one; a
    /// finish without a matching start is emitted as a zero-ish-duration span
    /// (the start update may have been produced before the recorder attached).
    pub fn tool_finished(&self, id: &str, ok: bool, title: Option<&str>) {
        let open = self
            .open_tools
            .lock()
            .ok()
            .and_then(|mut guard| guard.remove(id));
        let end = SystemTime::now();
        let (started_at, recorded_title) = match open {
            Some(tool) => (tool.started_at, tool.title),
            None => (end, String::new()),
        };
        let mut name = title.map(str::to_string).unwrap_or(recorded_title);
        if name.is_empty() {
            name = id.to_string();
        }
        let mut attrs = vec![
            KeyValue::new("gen_ai.operation.name", "execute_tool"),
            KeyValue::new("gen_ai.tool.name", name.clone()),
            KeyValue::new("gen_ai.conversation.id", self.conversation_id.clone()),
            KeyValue::new("gen_ai.agent.name", self.agent.clone()),
        ];
        if !ok {
            attrs.push(KeyValue::new("error.type", "tool_failed"));
        }
        let mut span = self
            .tracer
            .span_builder(format!("execute_tool {name}"))
            .with_kind(SpanKind::Internal)
            .with_start_time(started_at)
            .with_attributes(attrs)
            .start(&self.tracer);
        if !ok {
            span.set_status(Status::error("tool reported failure"));
        }
        span.end_with_timestamp(end);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use opentelemetry::Context;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::trace::{
        SdkTracerProvider, Span as SdkSpan, SpanData, SpanProcessor, Tracer as SdkTracer,
    };

    use super::*;

    #[derive(Debug)]
    struct CapturingProcessor {
        captured: Arc<std::sync::Mutex<Vec<SpanData>>>,
    }

    impl SpanProcessor for CapturingProcessor {
        fn on_start(&self, _span: &mut SdkSpan, _cx: &Context) {}
        fn on_end(&self, span: SpanData) {
            let mut guard = match self.captured.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.push(span);
        }
        fn force_flush(&self) -> OTelSdkResult {
            Ok(())
        }
        fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
            Ok(())
        }
    }

    fn capturing_tracer() -> (SdkTracer, Arc<std::sync::Mutex<Vec<SpanData>>>) {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = SdkTracerProvider::builder()
            .with_span_processor(CapturingProcessor {
                captured: Arc::clone(&captured),
            })
            .build();
        (provider.tracer("acp-test"), captured)
    }

    fn attr<'a>(span: &'a SpanData, key: &str) -> Option<&'a opentelemetry::Value> {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| &kv.value)
    }

    #[test]
    fn turn_completed_emits_invoke_agent_span() {
        let (tracer, captured) = capturing_tracer();
        let recorder = AcpSpanRecorder::with_tracer(tracer, "claude-acp", "rec-1");

        recorder.turn_completed(&TurnRecord {
            stop_reason: "EndTurn".to_string(),
            latency: Duration::from_millis(1500),
            context_used: Some(1500),
            context_size: Some(200_000),
        });

        let spans = captured.lock().expect("captured");
        assert_eq!(spans.len(), 1);
        let span = &spans[0];
        assert_eq!(span.name, "invoke_agent claude-acp");
        assert_eq!(
            attr(span, "gen_ai.operation.name").map(ToString::to_string),
            Some("invoke_agent".to_string())
        );
        assert_eq!(
            attr(span, "gen_ai.conversation.id").map(ToString::to_string),
            Some("rec-1".to_string())
        );
        assert_eq!(
            attr(span, "bitrouter.context.used").map(ToString::to_string),
            Some("1500".to_string())
        );
        // The span duration is the reported latency (back-dated start).
        let duration = span
            .end_time
            .duration_since(span.start_time)
            .expect("end after start");
        assert!(duration >= Duration::from_millis(1400), "got {duration:?}");
    }

    #[test]
    fn tool_lifecycle_emits_execute_tool_span_with_failure_status() {
        let (tracer, captured) = capturing_tracer();
        let recorder = AcpSpanRecorder::with_tracer(tracer, "claude-acp", "rec-1");

        recorder.tool_started("t1", "Read file");
        recorder.tool_finished("t1", false, None);
        // A finish without a start still emits (recorder attached late).
        recorder.tool_finished("t2", true, Some("Write file"));

        let spans = captured.lock().expect("captured");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].name, "execute_tool Read file");
        assert_eq!(
            attr(&spans[0], "error.type").map(ToString::to_string),
            Some("tool_failed".to_string())
        );
        assert_eq!(spans[1].name, "execute_tool Write file");
        assert!(attr(&spans[1], "error.type").is_none());
    }
}
