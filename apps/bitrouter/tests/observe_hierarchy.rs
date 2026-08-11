//! Inbound HTTP SERVER span ↔ bitrouter `chat` INTERNAL span parenting
//! end-to-end. Separate test binary so the one-shot `tracing` global
//! subscriber doesn't collide with the other integration tests.
//!
//! What this exercises that the in-process unit tests don't:
//! - `tower-http`'s `TraceLayer` is actually installed on the axum
//!   `Router` via `RouterOptions::with_router_wrapper`.
//! - The `tracing-opentelemetry` bridge layer is installed on the
//!   global `tracing` subscriber, parameterised on the live exporter's
//!   SDK tracer.
//! - The bitrouter pipeline's `Phase::PreRequest` reaches the bridged
//!   SERVER span via `tracing::Span::current().context()` (the bridge
//!   does NOT synchronise `opentelemetry::Context::current()` with
//!   tracing's current span across async awaits) and parents the root
//!   `chat` INTERNAL span on it.
//!
//! Specs:
//! - GenAI semantic conventions: <https://opentelemetry.io/docs/specs/semconv/gen-ai/>
//! - OTel HTTP semantic conventions: <https://opentelemetry.io/docs/specs/semconv/http/>
//! - W3C Trace Context: <https://www.w3.org/TR/trace-context/>

use std::time::Duration;

use axum_test::TestServer;
use bitrouter_sdk::config;
use bitrouter_sdk::server::{AppState, RouterOptions, build_router_with_options};
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Polling budget for the metric flush window.
const ASYNC_WAIT_BUDGET_MS: u64 = 5_000;
const POLL_INTERVAL_MS: u64 = 50;

/// Install the same fmt + OTel bridge layers the real binary's `serve`
/// command installs after the exporter is built. One-shot per process — but
/// this is a dedicated test binary, so it runs exactly once.
///
/// The filter is fixed (never read from the environment) so external
/// `RUST_LOG` settings cannot suppress the INFO-level HTTP SERVER span being
/// asserted below. It is deliberately *not* a bare `info`: the two
/// `<crate>=warn` directives silence every module-path target in the crates
/// that could plausibly host `http_layer.rs` — `bitrouter_observe` today,
/// `bitrouter_sdk` after the OTel modules move there. The ingress span
/// survives only because it pins an explicit
/// `target: "bitrouter::observe::http"` that no module path matches, so the
/// end-to-end assertion below doubles as the regression test for that pin:
/// drop the explicit target and the span is filtered out before the
/// tracing-opentelemetry bridge can export it, and the SERVER-span assertion
/// fails.
///
/// This mirrors a real operator setting — `RUST_LOG=info,bitrouter_sdk=warn`
/// is plausible noise reduction, since the whole routing pipeline logs under
/// that target — which must not silently orphan every `chat` span.
fn install_tracing_subscriber(exporter: &bitrouter_observe::otel::OtelExporter) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let env_filter =
        tracing_subscriber::EnvFilter::new("info,bitrouter_observe=warn,bitrouter_sdk=warn");
    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer())
        .with(bitrouter_observe::otel::http_layer::tracing_subscriber_layer(exporter))
        .init();
}

/// Decode OTLP/HTTP+protobuf trace exports from the wiremock collector.
/// The config below turns OTLP metric export off, so the collector only
/// ever receives `ExportTraceServiceRequest` bodies — every captured
/// POST is a trace and decodes cleanly.
async fn collect_exported_trace_spans(
    collector: &MockServer,
) -> Vec<opentelemetry_proto::tonic::trace::v1::Span> {
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
    use prost::Message;
    let requests = collector.received_requests().await.unwrap_or_default();
    let mut spans = Vec::new();
    for req in &requests {
        let parsed = ExportTraceServiceRequest::decode(req.body.as_slice())
            .expect("every collector body is a trace export (metric export is off)");
        for resource_spans in parsed.resource_spans {
            for scope_spans in resource_spans.scope_spans {
                spans.extend(scope_spans.spans);
            }
        }
    }
    spans
}

/// Poll until the collector has received a SERVER span, or the budget
/// expires. Reports whether it arrived rather than panicking, so the caller
/// can force a final flush and let the real assertion produce the diagnostic.
///
/// Waiting for *any* export is not enough, and was the cause of an
/// intermittent macOS CI failure: the `route` and `chat` spans close while the
/// request is still in flight and batch out first, but the tower-http SERVER
/// span only closes when the response future completes on the server task —
/// which races with the client-side read returning here. The old code took the
/// first export as its cue and shut the pipeline down, so on an unlucky
/// interleaving the SERVER span's batch never went out and the span was lost
/// rather than late.
///
/// Polling for the span we actually assert on removes the race instead of
/// widening a sleep: the batch processor is configured at `flush_ms: 100`
/// below, so a closed span appears within a tick.
async fn server_span_exported(collector: &MockServer) -> bool {
    use opentelemetry_proto::tonic::trace::v1::span::SpanKind as ProtoKind;
    for _ in 0..(ASYNC_WAIT_BUDGET_MS / POLL_INTERVAL_MS) {
        if collect_exported_trace_spans(collector)
            .await
            .iter()
            .any(|s| s.kind == ProtoKind::Server as i32)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
    false
}

/// Minimal OpenAI-style SSE response: text delta + finish chunk with
/// usage. Matches what the OpenAI inbound adapter expects.
fn minimal_sse_body() -> String {
    let body = serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "model": "test-model",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": "hi"}, "finish_reason": null}],
    });
    let finish = serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "model": "test-model",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    });
    format!("data: {body}\n\ndata: {finish}\n\ndata: [DONE]\n\n")
}

#[tokio::test]
async fn e2e_server_span_parents_chat_via_tracing_opentelemetry_bridge() {
    // ── upstream + OTLP collector ──
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wm_path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(minimal_sse_body(), "text/event-stream"),
        )
        .mount(&upstream)
        .await;

    let otlp_collector = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wm_path("/v1/traces"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&otlp_collector)
        .await;

    // ── minimal config with OTel wired, auth + policy + guardrails off.
    //    Metric export is off (nested `otel:` block — the only form that
    //    carries the knob) so the collector receives only trace bodies. ──
    let otlp_traces = format!("{}/v1/traces", otlp_collector.uri());
    let yaml = format!(
        r#"
server:
  listen: "127.0.0.1:0"
  skip_auth: true
database:
  url: "sqlite::memory:"
providers:
  mock:
    api_base: {upstream}
    api_key: test-key
    api_protocol:
      - "*": chat_completions
    models:
      - id: test-model
        pricing:
          input_micro_usd_per_token: 1.0
          output_micro_usd_per_token: 1.0
plugins:
  bitrouter-observe:
    otel:
      endpoint: {otlp}
      traces:
        batch:
          # Deliberately hostile. At 1ms the batch processor exports while the
          # request is still in flight, so the `route` and `chat` spans reach
          # the collector before the tower-http SERVER span has closed — the
          # exact interleaving that made this test fail intermittently on
          # macOS CI and pass everywhere else.
          #
          # Holding the race open on every run turns a rare remote failure into
          # a deterministic local one: with the old "wait for any export, then
          # shut down" logic this configuration fails 12 times out of 12.
          flush_ms: 1
      metrics:
        enabled: false
"#,
        upstream = upstream.uri(),
        otlp = otlp_traces,
    );
    let cfg: config::Config = config::parse_with(&yaml, |_| None).expect("config parses");
    let assembled = bitrouter::build_app(&cfg).await.expect("app assembles");
    let exporter = assembled
        .otel_exporter
        .as_deref()
        .expect("OTel exporter must be wired");

    // Install the subscriber stack now that the exporter exists. The
    // bridge layer captures the SDK tracer eagerly — same ordering the
    // real binary follows in `apps/bitrouter/src/main.rs::serve`.
    install_tracing_subscriber(exporter);

    // ── router with tower-http TraceLayer installed via the observe
    //    plugin's `router_wrapper()` — same path the binary takes. ──
    let state = AppState {
        language_model: assembled.app.language_model().unwrap().clone(),
        mcp: assembled.app.mcp().cloned(),
        skip_auth: assembled.app.skip_auth(),
        metrics_renderer: assembled.app.metrics_renderer().cloned(),
        prompt_transforms: assembled.app.prompt_transforms().to_vec(),
    };
    let options = RouterOptions::default()
        .with_router_wrapper(bitrouter_observe::otel::http_layer::router_wrapper());
    let router = build_router_with_options(state, options);
    let server = TestServer::new(router);

    // ── make a streaming request ──
    let resp = server
        .post("/v1/chat/completions")
        .add_header("accept", "text/event-stream")
        .json(&serde_json::json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true,
        }))
        .await;
    resp.assert_status_ok();
    let _ = resp.text();
    drop(resp);
    drop(server);

    // ── flush + decode ──
    // Let the batch processor export the SERVER span on its own schedule…
    let exported = server_span_exported(&otlp_collector).await;
    // …then force the final flush regardless, so the assertions below see
    // every span the run produced.
    assembled.observe.shutdown().await;
    // If the wait timed out, the span may have closed inside the final batch
    // interval and only reached the collector via that forced flush. One more
    // window, so a genuine failure is distinguishable from a slow runner.
    if !exported {
        let _ = server_span_exported(&otlp_collector).await;
    }

    let spans = collect_exported_trace_spans(&otlp_collector).await;
    assert!(
        spans.iter().any(|s| s.trace_id.len() == 16),
        "collector must have received at least one real trace span"
    );

    use opentelemetry_proto::tonic::trace::v1::span::SpanKind as ProtoKind;
    let span_summary = || {
        spans
            .iter()
            .map(|s| {
                format!(
                    "{}:{} parent={:?} span={:?}",
                    s.name, s.kind, s.parent_span_id, s.span_id
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let server_span = spans
        .iter()
        .find(|s| s.kind == ProtoKind::Server as i32)
        .unwrap_or_else(|| {
            panic!(
                "tower-http TraceLayer + tracing-opentelemetry bridge must emit a SERVER span \
                 at HTTP ingress; exported spans: {}",
                span_summary()
            )
        });
    let chat_span = spans
        .iter()
        .find(|s| s.name == "chat test-model" && s.kind == ProtoKind::Internal as i32)
        .expect("root chat INTERNAL span");

    assert_eq!(
        chat_span.parent_span_id, server_span.span_id,
        "root chat INTERNAL span must parent on the SERVER span — that's the canonical \
         service-map shape the issue's locked plan calls out"
    );
    assert_eq!(
        chat_span.trace_id, server_span.trace_id,
        "chat and SERVER spans must live in the same trace"
    );
}
