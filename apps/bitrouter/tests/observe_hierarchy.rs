//! Inbound HTTP SERVER span ↔ bitrouter `chat` INTERNAL span parenting
//! end-to-end, against a live OTLP collector stub.
//!
//! What this exercises that the in-process unit tests don't:
//! - `otel::http_layer::router_wrapper` is actually installed on the axum
//!   `Router` via `RouterOptions::with_router_wrapper`, from the live
//!   exporter's tracer — the same path `serve` takes.
//! - The bitrouter pipeline's `Phase::PreRequest` reaches that ingress span
//!   through `opentelemetry::Context::current()` and parents the root `chat`
//!   INTERNAL span on it, across the real request future's suspension points.
//! - The spans survive OTLP encode/export and arrive at a collector with the
//!   instrumentation scope intact.
//!
//! **The assertions below are unchanged from the pre-D3 version of this
//! test; the wiring is not.** It used to install `tower-http`'s `TraceLayer`
//! plus the `tracing-opentelemetry` bridge and assert that the pipeline
//! reached the bridged span via `tracing::Span::current().context()`. The
//! ingress span is now an OTel span in its own right, so the bridge is not on
//! this path at all. What the test guards — SERVER present, root `chat`
//! parented on it, same trace, scope name and version — is identical.
//!
//! The bridge path still exists and still has a consumer; it is covered by a
//! unit test in `bitrouter-sdk` (`bridge_ingress_span_still_parents_chat`)
//! rather than here.
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

/// Install a deliberately hostile `tracing` filter and no OTel bridge.
///
/// This is the inverse of what it used to be, and the inversion is the point.
/// The old version installed the `tracing-opentelemetry` bridge and picked a
/// filter (`info,bitrouter_observe=warn,bitrouter_sdk=warn`) that would starve
/// it if the ingress span ever lost its explicit `target:` pin — making this
/// end-to-end test a structural regression guard for that pin.
///
/// After D3 that guard is gone, and **silently**: an OTel-native ingress span
/// never passes through the `tracing` subscriber, so no filter can suppress
/// it and the SERVER assertion below would pass whether or not the pin
/// survived. The direct replacement is
/// `otel::http_layer::tests::ingress_emits_the_pinned_log_target_at_debug`,
/// which asserts the target and level against a capturing subscriber.
///
/// The filter stays anyway, tightened to `warn`, to assert the *new* property
/// this design bought: span export is now completely independent of
/// `RUST_LOG`. An operator running a blanket `warn` — previously enough to
/// orphan every `chat` span with no diagnostic anywhere — must now get a
/// fully intact trace tree. That is what the assertions below are proving
/// under this filter.
fn install_tracing_subscriber() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("warn"))
        .with(tracing_subscriber::fmt::layer())
        .init();
}

/// The instrumentation scope every exported bitrouter span must carry. This
/// name is OTLP wire contract: backend dashboards and collector rules select
/// on it, so it is asserted here rather than left to drift with the crate that
/// happens to host the exporter.
const EXPECTED_SCOPE_NAME: &str = "io.bitrouter.observe";

/// Decode OTLP/HTTP+protobuf trace exports from the wiremock collector.
/// The config below turns OTLP metric export off, so the collector only
/// ever receives `ExportTraceServiceRequest` bodies — every captured
/// POST is a trace and decodes cleanly.
async fn collect_exported_trace_spans(
    collector: &MockServer,
) -> Vec<opentelemetry_proto::tonic::trace::v1::Span> {
    collect_exported_trace_scopes(collector)
        .await
        .into_iter()
        .flat_map(|scope_spans| scope_spans.spans)
        .collect()
}

/// Same decode as [`collect_exported_trace_spans`], but keeps the
/// `scope_spans` wrapper so the caller can assert on the instrumentation
/// scope (`name` / `version`) the exporter stamped onto the batch.
async fn collect_exported_trace_scopes(
    collector: &MockServer,
) -> Vec<opentelemetry_proto::tonic::trace::v1::ScopeSpans> {
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
    use prost::Message;
    let requests = collector.received_requests().await.unwrap_or_default();
    let mut scopes = Vec::new();
    for req in &requests {
        let parsed = ExportTraceServiceRequest::decode(req.body.as_slice())
            .expect("every collector body is a trace export (metric export is off)");
        for resource_spans in parsed.resource_spans {
            scopes.extend(resource_spans.scope_spans);
        }
    }
    scopes
}

/// Poll until the collector has received **both** spans this test asserts on,
/// or the budget expires. Reports whether they arrived rather than panicking,
/// so the caller can force a final flush and let the real assertion produce
/// the diagnostic.
///
/// Waiting for *any* export is not enough, and once caused an intermittent
/// macOS CI failure. Wait for exactly what is asserted — the rule has held
/// through two inversions of which span closes last, and both are worth
/// recording because each broke the version of this helper written for the
/// other:
///
/// - **Under `tower-http`:** `route` and `chat` closed while the request was
///   still in flight and batched out first; the SERVER span closed only when
///   the response future completed on the server task. So the helper waited on
///   the SERVER span.
/// - **Under the OTel-native layer:** the order inverts. The middleware's span
///   closes as soon as `next.run` returns — which for a streamed response is
///   when the body *starts*, not when it drains — so the SERVER span now
///   arrives first and `chat` closes later, at `on_request_end`. A helper that
///   still waited on SERVER would shut the pipeline down before `chat` existed
///   and fail on a missing root span, which is exactly what it did.
///
/// Waiting on both is invariant to the ordering, so the next change to it does
/// not break this test again. The batch processor is at `flush_ms: 1` below, so
/// a closed span appears within a tick.
async fn asserted_spans_exported(collector: &MockServer) -> bool {
    use opentelemetry_proto::tonic::trace::v1::span::SpanKind as ProtoKind;
    for _ in 0..(ASYNC_WAIT_BUDGET_MS / POLL_INTERVAL_MS) {
        let spans = collect_exported_trace_spans(collector).await;
        let server = spans.iter().any(|s| s.kind == ProtoKind::Server as i32);
        let root_chat = spans
            .iter()
            .any(|s| s.name == "chat test-model" && s.kind == ProtoKind::Internal as i32);
        if server && root_chat {
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
async fn e2e_otel_native_server_span_parents_chat_under_a_hostile_log_filter() {
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
          # request is still in flight, so spans reach the collector piecemeal
          # as they close rather than in one batch at shutdown — the class of
          # interleaving that made this test fail intermittently on macOS CI
          # and pass everywhere else. (Which span closes last has since changed
          # twice; see `asserted_spans_exported`, which is written not to care.)
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

    // Blanket `warn`. Under the previous design this alone was enough to
    // orphan every `chat` span; the assertions below are what proves it no
    // longer is.
    install_tracing_subscriber();

    // ── router with the OTel-native ingress layer installed via
    //    `router_wrapper(exporter)` — same path the binary takes. ──
    let state = AppState {
        language_model: assembled.app.language_model().unwrap().clone(),
        mcp: assembled.app.mcp().cloned(),
        skip_auth: assembled.app.skip_auth(),
        metrics_renderer: assembled.app.metrics_renderer().cloned(),
        prompt_transforms: assembled.app.prompt_transforms().to_vec(),
    };
    let options = RouterOptions::default()
        .with_router_wrapper(bitrouter_sdk::otel::http_layer::router_wrapper(exporter));
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
    // Let the batch processor export both asserted spans on its own schedule…
    let exported = asserted_spans_exported(&otlp_collector).await;
    // …then force the final flush regardless, so the assertions below see
    // every span the run produced.
    assembled.observe.shutdown().await;
    // If the wait timed out, a span may have closed inside the final batch
    // interval and only reached the collector via that forced flush. One more
    // window, so a genuine failure is distinguishable from a slow runner.
    if !exported {
        let _ = asserted_spans_exported(&otlp_collector).await;
    }

    // ── instrumentation scope is wire contract ──
    // `scope.name` is what backends and collector rules select bitrouter's
    // spans by; renaming it silently breaks every dashboard built on it. The
    // version is stamped from the exporter crate's `CARGO_PKG_VERSION`, which
    // is the workspace version this test crate also builds under.
    let scopes = collect_exported_trace_scopes(&otlp_collector).await;
    assert!(
        !scopes.is_empty(),
        "collector must have received at least one scope_spans batch"
    );
    for scope_spans in &scopes {
        let scope = scope_spans
            .scope
            .as_ref()
            .expect("every exported batch carries an instrumentation scope");
        assert_eq!(
            scope.name, EXPECTED_SCOPE_NAME,
            "exported instrumentation scope name is OTLP wire contract"
        );
        assert_eq!(
            scope.version,
            env!("CARGO_PKG_VERSION"),
            "instrumentation scope version must track the workspace crate version"
        );
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
                "otel::http_layer must emit an OTel SERVER span \
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
