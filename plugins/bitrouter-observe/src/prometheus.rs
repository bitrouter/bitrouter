//! `PrometheusHook` — a dependency-free `language_model::ObserveHook` that
//! accumulates request metrics and renders them in the Prometheus text
//! exposition format.
//!
//! Like every `ObserveHook` it is **read-only and error-swallowing** — it never
//! influences the request. Metrics live behind a `Mutex` because the hook is
//! shared (`Arc<dyn ObserveHook>`) across concurrent requests.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use bitrouter_sdk::MetricsRenderer;
use bitrouter_sdk::language_model::{
    ObserveHook, Phase, PipelineContext, RequestOutcome, StreamContext, StreamInterest, StreamPart,
};

#[derive(Default)]
struct Metrics {
    /// `requests_total{outcome}` counters.
    requests_total: HashMap<&'static str, u64>,
    /// Sum of request latencies (ms) — paired with `requests_total` for an avg.
    latency_ms_sum: u64,
    /// Total stream parts observed.
    stream_parts_total: u64,
}

/// A Prometheus-exposition `ObserveHook`.
#[derive(Default)]
pub struct PrometheusHook {
    metrics: Mutex<Metrics>,
}

impl PrometheusHook {
    /// A fresh hook with zeroed metrics.
    pub fn new() -> Self {
        Self::default()
    }

    /// Render the accumulated metrics in the Prometheus text exposition format.
    /// Renders an empty body on lock poison (rather than panicking) so a
    /// `/metrics` scrape never brings the server down — matches the
    /// error-swallowing contract of the observe hook write path.
    pub fn render(&self) -> String {
        let Ok(m) = self.metrics.lock() else {
            return String::new();
        };
        let mut out = String::new();
        out.push_str("# HELP bitrouter_requests_total Total requests by outcome.\n");
        out.push_str("# TYPE bitrouter_requests_total counter\n");
        let mut grand_total = 0u64;
        for (outcome, count) in &m.requests_total {
            out.push_str(&format!(
                "bitrouter_requests_total{{outcome=\"{outcome}\"}} {count}\n"
            ));
            grand_total = grand_total.saturating_add(*count);
        }
        out.push_str("# HELP bitrouter_requests_grand_total Total requests across all outcomes.\n");
        out.push_str("# TYPE bitrouter_requests_grand_total counter\n");
        out.push_str(&format!("bitrouter_requests_grand_total {grand_total}\n"));
        out.push_str("# HELP bitrouter_request_latency_ms_sum Sum of request latency in ms.\n");
        out.push_str("# TYPE bitrouter_request_latency_ms_sum counter\n");
        out.push_str(&format!(
            "bitrouter_request_latency_ms_sum {}\n",
            m.latency_ms_sum
        ));
        out.push_str("# HELP bitrouter_stream_parts_total Total stream parts observed.\n");
        out.push_str("# TYPE bitrouter_stream_parts_total counter\n");
        out.push_str(&format!(
            "bitrouter_stream_parts_total {}\n",
            m.stream_parts_total
        ));
        out
    }
}

#[async_trait]
impl ObserveHook for PrometheusHook {
    async fn after_phase(&self, _phase: Phase, _ctx: &PipelineContext) {
        // Per-phase timing is recorded at request end from the execution result.
    }

    fn stream_interest(&self) -> StreamInterest {
        // Count every streamed part — cheap, and gives a throughput signal.
        StreamInterest::all()
    }

    async fn on_stream_part(&self, _ctx: &StreamContext, _part: &StreamPart) {
        if let Ok(mut m) = self.metrics.lock() {
            m.stream_parts_total += 1;
        }
    }

    async fn on_request_end(&self, ctx: &PipelineContext, outcome: &RequestOutcome) {
        let outcome_label = match outcome {
            RequestOutcome::Completed => "completed",
            RequestOutcome::Failed(_) => "failed",
            RequestOutcome::ClientDisconnected => "disconnected",
        };
        if let Ok(mut m) = self.metrics.lock() {
            *m.requests_total.entry(outcome_label).or_insert(0) += 1;
            if let Some(exec) = &ctx.execution_result {
                m.latency_ms_sum += exec.latency_ms;
            }
        }
    }
}

/// Lets the HTTP server's `GET /metrics` read the same accumulator the
/// ObserveHook writes to (the same Arc is registered as both).
impl MetricsRenderer for PrometheusHook {
    fn render(&self) -> String {
        Self::render(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::language_model::{GenerationParams, Message, PipelineRequest, Prompt, Role};

    fn ctx() -> PipelineContext {
        let prompt = Prompt {
            model: "m".to_string(),
            system: None,
            messages: vec![Message::text(Role::User, "hi")],
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            stream: false,
        };
        PipelineContext::new(PipelineRequest::new(
            "m",
            CallerContext::new("k", "u"),
            prompt,
        ))
    }

    #[tokio::test]
    async fn renders_prometheus_text_after_requests() {
        let hook = PrometheusHook::new();
        hook.on_request_end(&ctx(), &RequestOutcome::Completed)
            .await;
        hook.on_request_end(&ctx(), &RequestOutcome::Completed)
            .await;
        hook.on_request_end(
            &ctx(),
            &RequestOutcome::Failed(bitrouter_sdk::BitrouterError::internal("x")),
        )
        .await;
        let text = hook.render();
        assert!(text.contains("bitrouter_requests_total{outcome=\"completed\"} 2"));
        assert!(text.contains("bitrouter_requests_total{outcome=\"failed\"} 1"));
        assert!(text.contains("# TYPE bitrouter_requests_total counter"));
    }
}
