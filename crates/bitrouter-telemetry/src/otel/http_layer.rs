//! Inbound HTTP layer — creates the OTel SERVER span at request ingress and
//! parents it on any inbound W3C trace context.
//!
//! The span is an **OpenTelemetry span, built directly against the API
//! crate** — not a `tracing` span bridged into OTel. That distinction is the
//! whole point of this module, and it removes a silent failure mode rather
//! than saving a dependency:
//!
//! The previous implementation built a `tracing::info_span!` through
//! tower-http's `TraceLayer` and relied on `tracing-opentelemetry` to mirror
//! it into OTel. That made the entire trace tree contingent on the host's
//! `EnvFilter`: an operator running `RUST_LOG=warn` — or anything that
//! suppressed this module's target at INFO — starved the bridge, and every
//! `chat` span exported as an orphan root instead of a child of its HTTP
//! request, **with no error reported anywhere**. An OTel-native span never
//! passes through the `tracing` subscriber, so no filter can suppress it.
//!
//! Composition:
//! - An `axum` middleware creates one `SpanKind::Server` span per inbound
//!   request, from the tracer the caller's [`OtelExporter`] owns.
//! - The inbound `traceparent` / `tracestate` are extracted with the
//!   registered propagator, so the span nests under the upstream caller's
//!   trace when one exists.
//! - The span is published on the OTel [`Context`] for the request future via
//!   `FutureExt::with_context`, so everything downstream — including the
//!   `chat` INTERNAL span the exporter creates later — parents off
//!   `Context::current()`. This survives suspension points: the context is
//!   re-attached on every poll of the wrapped future.
//!
//! `bitrouter::observe::http` is still emitted on, and is still a pinned
//! `RUST_LOG` selector, but what it now carries is **diagnostics only** —
//! DEBUG events. Suppressing it no longer affects span export. See
//! `docs/CLI.md`.
//!
//! Specs:
//! - W3C Trace Context: <https://www.w3.org/TR/trace-context/>
//! - OTel HTTP semantic conventions: <https://opentelemetry.io/docs/specs/semconv/http/>

use axum::Router;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::context::FutureExt as _;
use opentelemetry::propagation::Extractor;
use opentelemetry::trace::{SpanKind, Status, TraceContextExt as _, Tracer as _};
use opentelemetry::{Context, KeyValue, global};

use crate::otel::exporter::OtelExporter;

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

/// Build a router wrapper that opens an OTel SERVER span per inbound request,
/// parented on any inbound W3C trace context. Pass the returned closure to
/// [`bitrouter_sdk::server::RouterOptions::with_router_wrapper`].
///
/// Takes the exporter because the span is created from **its** tracer. The SDK
/// deliberately does not install a global `TracerProvider` — doing so would
/// clobber any other OpenTelemetry consumer in the process — so
/// `global::tracer()` here would return a `NoopTracer` and drop every ingress
/// span silently. See `docs/OTEL_TIERING_SPEC.md` D4 for the same constraint
/// on the `tracing` bridge.
pub fn router_wrapper(
    exporter: &OtelExporter,
) -> impl Fn(Router) -> Router + Clone + Send + Sync + 'static {
    let tracer = exporter.tracer_clone();
    move |router: Router| {
        let tracer = tracer.clone();
        router.layer(axum::middleware::from_fn(
            move |request: Request, next: Next| {
                let tracer = tracer.clone();
                async move { server_span(tracer, request, next).await }
            },
        ))
    }
}

/// One inbound request, wrapped in a SERVER span.
async fn server_span(
    tracer: opentelemetry_sdk::trace::Tracer,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    // `http.route` is the matched *template*, not the request path: the
    // semconv requires it to be low cardinality, and this router mounts
    // `POST /mcp/{server}` and `POST /v1beta/models/{model_action}`, so using
    // the path made every MCP server id its own span name and its own
    // `http.route` value. `MatchedPath` is in the request extensions here
    // because `router_wrapper` applies this middleware with `Router::layer`,
    // which runs after routing. The path is the fallback for the one case that
    // has no template — a request that matched no route at all.
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or_else(|| path.clone(), |matched| matched.as_str().to_owned());

    let parent_cx = global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderExtractor(request.headers()))
    });
    let inbound_is_valid = parent_cx.span().span_context().is_valid();
    if !inbound_is_valid && request.headers().contains_key("traceparent") {
        // A header arrived and produced nothing usable — malformed, or a
        // version this propagator does not understand. Worth saying, unlike
        // the ordinary no-header case, which is most requests.
        tracing::debug!(
            target: "bitrouter::observe::http",
            "inbound `traceparent` did not parse; starting a new trace"
        );
    }

    // Span name follows the HTTP semconv: "{METHOD} {route}".
    let builder = tracer
        .span_builder(format!("{method} {route}"))
        .with_kind(SpanKind::Server)
        .with_attributes(vec![
            KeyValue::new("http.request.method", method.clone()),
            KeyValue::new("http.route", route.clone()),
            KeyValue::new("url.path", path.clone()),
        ]);
    let cx = if inbound_is_valid {
        // Extend the *extracted* context rather than the ambient one, so
        // anything else the propagator carried (baggage under a composite
        // propagator, for instance) survives into the request rather than
        // being dropped on the floor here.
        let span = builder.start_with_context(&tracer, &parent_cx);
        parent_cx.with_span(span)
    } else {
        Context::current_with_span(builder.start(&tracer))
    };

    // The `target:` is pinned deliberately. Without it `tracing` derives the
    // target from `module_path!()`, so moving this file between crates or
    // modules would silently rename it, and `bitrouter::observe::http` is a
    // documented operator-facing `RUST_LOG` selector (`docs/CLI.md`). Unlike
    // the `tracing`-span implementation this replaced, suppressing it costs
    // only the diagnostic — never the span.
    tracing::debug!(
        target: "bitrouter::observe::http",
        method = %method,
        route = %route,
        path = %path,
        "ingress span opened"
    );

    let response = next.run(request).with_context(cx.clone()).await;

    // The status is known as soon as the head is produced; the *span* is not
    // finished then. For a streamed response `next.run` returns when the body
    // starts, not when it drains — which for this gateway is before the model
    // has generated anything. Ending here would make the SERVER span's
    // duration time-to-headers and leave its own `chat` child outliving it.
    // So the span rides the body and ends when the body does.
    let status = response.status();
    cx.span().set_attribute(KeyValue::new(
        "http.response.status_code",
        i64::from(status.as_u16()),
    ));
    // HTTP semconv: a SERVER span MUST be `Error` for a 5xx and MUST be left
    // `Unset` for a 4xx — a client sending a bad request is not this server
    // failing, and marking it so would drown the error rate a backend computes
    // from span status in ordinary 404s. Without this every ingress span
    // exported as `Unset`, so a 500 was indistinguishable from a 200 to any
    // consumer that filters on status rather than re-deriving it from the
    // attribute.
    if status.is_server_error() {
        // `error.type` is the status code as a string: the semconv's fallback
        // when no more specific classification is available, which is the case
        // here — this layer sees a response, not the failure that produced it.
        cx.span()
            .set_attribute(KeyValue::new("error.type", status.as_u16().to_string()));
        // Description left empty deliberately. The semconv says it SHOULD NOT
        // repeat what `error.type` and `http.response.status_code` already
        // carry.
        cx.span().set_status(Status::error(""));
    }
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        axum::body::Body::new(SpanBody {
            inner: body,
            _span: SpanEndGuard(cx),
        }),
    )
}

/// Ends the ingress span when dropped.
///
/// Drop rather than an explicit call at end-of-stream, because the cases that
/// matter most are the ones with no end-of-stream: a client that disconnects
/// mid-generation, or a body dropped without being polled to completion. Both
/// must still close the span, and both are drops.
struct SpanEndGuard(Context);

impl Drop for SpanEndGuard {
    fn drop(&mut self) {
        self.0.span().end();
    }
}

pin_project_lite::pin_project! {
    /// Transparent body wrapper whose only job is to own [`SpanEndGuard`] for
    /// the lifetime of the response body. Frames and trailers pass through
    /// untouched.
    struct SpanBody<B> {
        #[pin]
        inner: B,
        _span: SpanEndGuard,
    }
}

impl<B: http_body::Body> http_body::Body for SpanBody<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        self.project().inner.poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::routing::get;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::{
        RandomIdGenerator, Sampler, SdkTracerProvider, Span as SdkSpan, SpanData, SpanProcessor,
    };
    use tower::ServiceExt as _;

    /// Captures every ended span in-process, so assertions look at span
    /// structure directly instead of decoding OTLP wire bytes.
    #[derive(Debug, Clone)]
    struct CapturingProcessor {
        captured: Arc<Mutex<Vec<SpanData>>>,
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

    fn capturing_tracer() -> (
        opentelemetry_sdk::trace::Tracer,
        SdkTracerProvider,
        Arc<Mutex<Vec<SpanData>>>,
    ) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        global::set_text_map_propagator(TraceContextPropagator::new());
        let provider = SdkTracerProvider::builder()
            .with_span_processor(CapturingProcessor {
                captured: Arc::clone(&captured),
            })
            .with_sampler(Sampler::AlwaysOn)
            .with_id_generator(RandomIdGenerator::default())
            .build();
        let tracer = provider.tracer("io.bitrouter.observe.test");
        (tracer, provider, captured)
    }

    /// A route that suspends twice before responding, and records what
    /// `Context::current()` looks like from *inside* the handler — which is
    /// where the exporter reads it to parent the root `chat` span.
    fn probe_router(
        tracer: opentelemetry_sdk::trace::Tracer,
        seen: Arc<Mutex<Option<opentelemetry::trace::SpanContext>>>,
    ) -> Router {
        let wrapper = {
            // `router_wrapper` takes an `&OtelExporter`; the layer only needs
            // the tracer, so the test drives `server_span` through the same
            // `from_fn` shape rather than constructing a whole exporter.
            let tracer = tracer.clone();
            move |router: Router| {
                let tracer = tracer.clone();
                router.layer(axum::middleware::from_fn(
                    move |request: Request, next: Next| {
                        let tracer = tracer.clone();
                        async move { server_span(tracer, request, next).await }
                    },
                ))
            }
        };
        let router = Router::new()
            .route(
                "/boom",
                get(|| async { http::StatusCode::INTERNAL_SERVER_ERROR }),
            )
            // Same shape as the SDK router's `POST /mcp/{server}`: the
            // templated route whose path segment is caller-chosen, and so
            // unbounded in cardinality.
            .route("/mcp/{server}", axum::routing::post(|| async { "ok" }))
            .route(
                "/probe",
                get(move || {
                    let seen = Arc::clone(&seen);
                    async move {
                        tokio::task::yield_now().await;
                        tokio::task::yield_now().await;
                        let cx = Context::current();
                        if let Ok(mut slot) = seen.lock() {
                            *slot = Some(cx.span().span_context().clone());
                        }
                        "ok"
                    }
                }),
            );
        wrapper(router)
    }

    #[tokio::test]
    async fn ingress_span_is_otel_native_and_reaches_the_handler_across_awaits() {
        let (tracer, provider, captured) = capturing_tracer();
        let seen = Arc::new(Mutex::new(None));
        let router = probe_router(tracer, Arc::clone(&seen));

        let response = router
            .oneshot(
                http::Request::builder()
                    .uri("/probe")
                    .body(axum::body::Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("router responds");
        assert_eq!(response.status(), http::StatusCode::OK);
        // The span now rides the response body, so it is not finished until the
        // body is released.
        drop(response);
        assert!(provider.force_flush().is_ok());

        let spans = captured.lock().expect("captured").clone();
        let server = spans
            .iter()
            .find(|span| span.span_kind == SpanKind::Server)
            .expect("an OTel SERVER span is exported at ingress");
        assert_eq!(server.name, "GET /probe");

        // The handler saw the SERVER span as its current context, two
        // suspension points in. This is the mechanism the exporter relies on
        // to parent `chat`; without it every trace silently orphans.
        let seen = seen.lock().expect("seen").clone().expect("handler ran");
        assert!(seen.is_valid(), "handler observed a valid current span");
        assert_eq!(
            seen.span_id(),
            server.span_context.span_id(),
            "`Context::current()` inside the handler is the ingress SERVER span"
        );
    }

    #[tokio::test]
    async fn ingress_span_parents_on_inbound_traceparent() {
        let (tracer, provider, captured) = capturing_tracer();
        let router = probe_router(tracer, Arc::new(Mutex::new(None)));

        // A well-formed W3C header: version-traceid-spanid-flags.
        let trace_id = "4bf92f3577b34da6a3ce929d0e0e4736";
        let parent_span_id = "00f067aa0ba902b7";
        let response = router
            .oneshot(
                http::Request::builder()
                    .uri("/probe")
                    .header("traceparent", format!("00-{trace_id}-{parent_span_id}-01"))
                    .body(axum::body::Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("router responds");
        assert_eq!(response.status(), http::StatusCode::OK);
        // The span now rides the response body, so it is not finished until the
        // body is released.
        drop(response);
        assert!(provider.force_flush().is_ok());

        let spans = captured.lock().expect("captured").clone();
        let server = spans
            .iter()
            .find(|span| span.span_kind == SpanKind::Server)
            .expect("SERVER span exported");
        assert_eq!(
            format!(
                "{:032x}",
                u128::from_be_bytes(server.span_context.trace_id().to_bytes())
            ),
            trace_id,
            "ingress joins the caller's trace"
        );
        assert_eq!(
            format!(
                "{:016x}",
                u64::from_be_bytes(server.parent_span_id.to_bytes())
            ),
            parent_span_id,
            "ingress parents on the caller's span"
        );
    }

    #[tokio::test]
    async fn ingress_span_records_the_response_status() {
        let (tracer, provider, captured) = capturing_tracer();
        let router = probe_router(tracer, Arc::new(Mutex::new(None)));

        let response = router
            .oneshot(
                http::Request::builder()
                    .uri("/missing")
                    .body(axum::body::Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("router responds");
        assert_eq!(response.status(), http::StatusCode::NOT_FOUND);
        drop(response);
        assert!(provider.force_flush().is_ok());

        let spans = captured.lock().expect("captured").clone();
        let server = spans
            .iter()
            .find(|span| span.span_kind == SpanKind::Server)
            .expect("SERVER span exported");
        let status = server
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == "http.response.status_code")
            .map(|kv| kv.value.clone())
            .expect("status code recorded");
        assert_eq!(status, opentelemetry::Value::I64(404));
    }

    #[tokio::test]
    async fn ingress_span_stays_open_until_the_response_body_is_released() {
        // The SERVER span must cover the whole response, not just the head.
        // For this gateway the head is produced before the model has generated
        // anything, so ending the span at `next.run` would report the server
        // duration as time-to-headers and — worse — let the `chat` span it
        // parents outlive it. `tower-http`'s `TraceLayer`, which this replaced,
        // held its span through end-of-stream; losing that was a regression,
        // and this is its guard.
        let (tracer, provider, captured) = capturing_tracer();
        let router = probe_router(tracer, Arc::new(Mutex::new(None)));

        let response = router
            .oneshot(
                http::Request::builder()
                    .uri("/probe")
                    .body(axum::body::Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("router responds");
        assert_eq!(response.status(), http::StatusCode::OK);

        // Head produced, body still alive: no SERVER span may have ended.
        assert!(provider.force_flush().is_ok());
        assert!(
            captured
                .lock()
                .expect("captured")
                .iter()
                .all(|span| span.span_kind != SpanKind::Server),
            "the ingress span must not end while the response body is still live"
        );

        drop(response);
        assert!(provider.force_flush().is_ok());
        assert!(
            captured
                .lock()
                .expect("captured")
                .iter()
                .any(|span| span.span_kind == SpanKind::Server),
            "releasing the body ends the ingress span"
        );
    }

    #[tokio::test]
    async fn a_templated_route_reports_the_template_not_the_path() {
        // `http.route` and the span name must be the matched template. The SDK
        // router mounts `POST /mcp/{server}` and
        // `POST /v1beta/models/{model_action}`, so deriving them from the
        // request path made every MCP server id a new span name and a new
        // `http.route` value — an unbounded dimension on the one attribute the
        // semconv requires to be low cardinality. `url.path` keeps the literal
        // path, which is where the caller-specific detail belongs.
        let (tracer, provider, captured) = capturing_tracer();
        let router = probe_router(tracer, Arc::new(Mutex::new(None)));

        let response = router
            .oneshot(
                http::Request::builder()
                    .method(http::Method::POST)
                    .uri("/mcp/some-server")
                    .body(axum::body::Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("router responds");
        assert_eq!(response.status(), http::StatusCode::OK);
        drop(response);
        assert!(provider.force_flush().is_ok());

        let spans = captured.lock().expect("captured").clone();
        let server = spans
            .iter()
            .find(|span| span.span_kind == SpanKind::Server)
            .expect("SERVER span exported");
        assert_eq!(server.name, "POST /mcp/{server}");
        let attribute = |key: &str| {
            server
                .attributes
                .iter()
                .find(|kv| kv.key.as_str() == key)
                .map(|kv| kv.value.clone())
        };
        assert_eq!(
            attribute("http.route"),
            Some(opentelemetry::Value::String("/mcp/{server}".into())),
            "`http.route` must be the matched template"
        );
        assert_eq!(
            attribute("url.path"),
            Some(opentelemetry::Value::String("/mcp/some-server".into())),
            "`url.path` must stay the literal path"
        );
    }

    #[tokio::test]
    async fn an_unmatched_request_falls_back_to_the_literal_path() {
        // Nothing matched, so there is no template to report. The schema
        // declares `http.route` as required, so the literal path stands in
        // rather than the attribute going missing.
        let (tracer, provider, captured) = capturing_tracer();
        let router = probe_router(tracer, Arc::new(Mutex::new(None)));

        let response = router
            .oneshot(
                http::Request::builder()
                    .uri("/missing")
                    .body(axum::body::Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("router responds");
        assert_eq!(response.status(), http::StatusCode::NOT_FOUND);
        drop(response);
        assert!(provider.force_flush().is_ok());

        let spans = captured.lock().expect("captured").clone();
        let server = spans
            .iter()
            .find(|span| span.span_kind == SpanKind::Server)
            .expect("SERVER span exported");
        assert_eq!(server.name, "GET /missing");
        assert_eq!(
            server
                .attributes
                .iter()
                .find(|kv| kv.key.as_str() == "http.route")
                .map(|kv| kv.value.clone()),
            Some(opentelemetry::Value::String("/missing".into()))
        );
    }

    #[tokio::test]
    async fn a_server_error_marks_the_ingress_span_as_an_error() {
        // Per the HTTP semconv a SERVER span whose response is 5xx carries
        // `Status::Error` and an `error.type`. Before this the span recorded
        // only `http.response.status_code`, so every ingress span exported as
        // `Unset` and a backend's error rate — which reads span status — saw
        // no failures at all.
        let (tracer, provider, captured) = capturing_tracer();
        let router = probe_router(tracer, Arc::new(Mutex::new(None)));

        let response = router
            .oneshot(
                http::Request::builder()
                    .uri("/boom")
                    .body(axum::body::Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("router responds");
        assert_eq!(response.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
        drop(response);
        assert!(provider.force_flush().is_ok());

        let spans = captured.lock().expect("captured").clone();
        let server = spans
            .iter()
            .find(|span| span.span_kind == SpanKind::Server)
            .expect("SERVER span exported");
        assert!(
            matches!(server.status, Status::Error { .. }),
            "a 5xx must set the span status to error; got {:?}",
            server.status
        );
        let error_type = server
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == "error.type")
            .map(|kv| kv.value.clone())
            .expect("`error.type` recorded on a 5xx");
        assert_eq!(error_type, opentelemetry::Value::String("500".into()));
    }

    #[tokio::test]
    async fn a_client_error_leaves_the_ingress_span_unset() {
        // The other half of the semconv rule, and the one that is easy to get
        // wrong in the direction that hurts: marking 4xx as an error would bury
        // real failures under every 404 a scanner produces.
        let (tracer, provider, captured) = capturing_tracer();
        let router = probe_router(tracer, Arc::new(Mutex::new(None)));

        let response = router
            .oneshot(
                http::Request::builder()
                    .uri("/missing")
                    .body(axum::body::Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("router responds");
        assert_eq!(response.status(), http::StatusCode::NOT_FOUND);
        drop(response);
        assert!(provider.force_flush().is_ok());

        let spans = captured.lock().expect("captured").clone();
        let server = spans
            .iter()
            .find(|span| span.span_kind == SpanKind::Server)
            .expect("SERVER span exported");
        assert!(
            matches!(server.status, Status::Unset),
            "a 4xx must leave the span status unset; got {:?}",
            server.status
        );
        assert!(
            !server
                .attributes
                .iter()
                .any(|kv| kv.key.as_str() == "error.type"),
            "a 4xx must not carry `error.type`"
        );
    }

    #[tokio::test]
    async fn ingress_span_conforms_to_the_committed_span_schema() {
        // Phase 0 declared the ingress SERVER span in `otel::schema` but could
        // not check it: the exporter's conformance tests only see spans the
        // exporter emits, and this one is emitted here. An OTel-native ingress
        // span closes that gap — the SERVER row of the artifact is now
        // enforced by the same rule as every other row.
        use bitrouter_sdk::observe::schema::{Requirement, SpanKind as SchemaKind, span_def_for};

        let (tracer, provider, captured) = capturing_tracer();
        let router = probe_router(tracer, Arc::new(Mutex::new(None)));
        let response = router
            .oneshot(
                http::Request::builder()
                    .uri("/probe")
                    .body(axum::body::Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("router responds");
        assert_eq!(response.status(), http::StatusCode::OK);
        // The span now rides the response body, so it is not finished until the
        // body is released.
        drop(response);
        assert!(provider.force_flush().is_ok());

        let spans = captured.lock().expect("captured").clone();
        let server = spans
            .iter()
            .find(|span| span.span_kind == SpanKind::Server)
            .expect("SERVER span exported");
        let def = span_def_for(&server.name, SchemaKind::Server)
            .expect("the ingress span is declared in otel::schema");

        for kv in server.attributes.iter() {
            assert!(
                def.attributes.iter().any(|a| a.key == kv.key.as_str()),
                "ingress span carries `{}`, which otel::schema does not declare on it",
                kv.key
            );
        }
        for attr in def
            .attributes
            .iter()
            .filter(|a| matches!(a.requirement, Requirement::Required))
        {
            assert!(
                server
                    .attributes
                    .iter()
                    .any(|kv| kv.key.as_str() == attr.key),
                "ingress span is missing `{}`, which otel::schema declares as required",
                attr.key
            );
        }
    }
}
