//! Inbound HTTP layer — creates the SERVER span at request ingress and
//! parents it on any inbound W3C trace context.
//!
//! Composition:
//! - [`tower_http::trace::TraceLayer`] creates a `tracing::Span` for every
//!   inbound request. Spec:
//!   <https://docs.rs/tower-http/latest/tower_http/trace/index.html>
//! - [`tracing_opentelemetry`] is the `tracing` ↔ OpenTelemetry bridge that
//!   maps `otel.*` sentinel fields on the `tracing::Span` into a real OTel
//!   span. The bridge layer itself is installed by the host (binary) via
//!   [`crate::otel::subscriber::tracing_subscriber_layer`], which lives in
//!   its own module so consumers that want only the bridge do not compile
//!   the axum front-end this module needs.
//! - In the `make_span_with` callback we extract any inbound
//!   `traceparent` / `tracestate` and call
//!   [`tracing_opentelemetry::OpenTelemetrySpanExt::set_parent`] so the
//!   span — and everything inside it, including the `chat` INTERNAL span
//!   the exporter creates later — nests under the upstream caller's trace
//!   when one exists.
//!
//! Specs:
//! - W3C Trace Context: <https://www.w3.org/TR/trace-context/>
//! - OTel HTTP semantic conventions: <https://opentelemetry.io/docs/specs/semconv/http/>

use std::time::Duration;

use axum::Router;
use opentelemetry::global;
use opentelemetry::propagation::Extractor;
use tower_http::trace::TraceLayer;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Inbound header extractor for W3C trace context. Identical in shape to
/// the equivalent in `exporter.rs`; kept local so this module compiles
/// without exporting trace-extraction internals.
struct HeaderExtractor<'a>(&'a http::HeaderMap);

impl<'a> Extractor for HeaderExtractor<'a> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(http::HeaderName::as_str).collect()
    }
}

/// Build a router wrapper that layers a `tower-http` `TraceLayer` plus
/// inbound W3C trace-context propagation onto the host's axum [`Router`].
/// Pass the returned closure to
/// [`crate::server::RouterOptions::with_router_wrapper`].
pub fn router_wrapper() -> impl Fn(Router) -> Router + Send + Sync + 'static {
    move |router: Router| {
        router.layer(
            TraceLayer::new_for_http()
                .make_span_with(make_http_server_span)
                .on_response(record_http_response_status),
        )
    }
}

/// `tower-http` callback: build the inbound-request `tracing::Span` with
/// `otel.*` sentinel fields, then bind it to any inbound W3C trace context
/// so the resulting OTel span parents on the upstream caller's trace.
fn make_http_server_span<B>(request: &http::Request<B>) -> Span {
    let method = request.method().as_str();
    let route = request.uri().path();
    // Span name follows the HTTP semconv: "{METHOD} {route}".
    let span_name = format!("{method} {route}");

    // The `target:` is pinned deliberately. Without it `tracing` derives the
    // target from `module_path!()`, so moving this file between crates or
    // modules would silently rename it. The target is a load-bearing
    // `RUST_LOG` selector: the `serve` subscriber is an operator-controlled
    // `EnvFilter`, and a filter that drops this INFO span also drops the
    // SERVER span the `chat` span parents on — orphaning every trace with no
    // diagnostic anywhere. Keep it stable and independent of file location.
    let span = tracing::info_span!(
        target: "bitrouter::observe::http",
        "http_request",
        otel.name = %span_name,
        otel.kind = "server",
        http.request.method = %method,
        http.route = %route,
        url.path = %route,
        http.response.status_code = tracing::field::Empty,
    );

    let parent = global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderExtractor(request.headers()))
    });
    if let Err(error) = span.set_parent(parent) {
        tracing::debug!(
            target: "bitrouter::observe::http",
            ?error,
            "could not attach inbound trace context"
        );
    }
    span
}

/// `tower-http` callback: backfill the response status on the SERVER span.
fn record_http_response_status<B>(response: &http::Response<B>, _latency: Duration, span: &Span) {
    span.record(
        "http.response.status_code",
        i64::from(response.status().as_u16()),
    );
}
