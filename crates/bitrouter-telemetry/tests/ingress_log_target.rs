//! `bitrouter::observe::http` is a pinned `RUST_LOG` selector. This asserts
//! that the ingress layer still emits on it, at the documented level.
//!
//! **Why this is a whole test binary for one assertion.** Before D3 the pin was
//! guarded *structurally*: the ingress span was a `tracing` span, so dropping
//! the explicit `target:` let a `bitrouter_sdk=warn` filter suppress it, which
//! starved the `tracing-opentelemetry` bridge, which broke SERVER → `chat`
//! parenting, which failed `apps/bitrouter/tests/observe_hierarchy.rs`. An
//! OTel-native ingress span severs that chain: no filter can suppress an OTel
//! span, so that end-to-end test now passes whether or not the pin survives.
//! It did not start failing — it stopped testing. This is its replacement, and
//! it asserts the target and level directly, independent of span export.
//!
//! It cannot live beside the other unit tests. `tracing`'s per-callsite
//! `Interest` cache and max-level filter are **process-global**, even though
//! `set_default` scopes a subscriber to one thread: a sibling test that
//! exercises this same `debug!` callsite with no subscriber installed causes
//! `Interest::never` to be cached for it, and this assertion then sees nothing.
//! Observed directly — the in-module version of this test passed alone and
//! failed roughly two runs in five inside the suite. A dedicated binary is a
//! dedicated process, which removes the interference rather than racing it.
//! `apps/bitrouter/tests/observe_hierarchy.rs` is split out for the same
//! reason.

#![cfg(all(feature = "server", feature = "otel-http"))]

use std::sync::{Arc, Mutex};

use bitrouter_telemetry::otel::{OtelConfig, OtelExporter};
use tower::ServiceExt as _;

/// Records every `tracing` event's target and level. A bare `Subscriber`
/// rather than an `EnvFilter` stack on purpose: the assertion is about what
/// the ingress layer *emits*, and routing it through a filter would test the
/// filter.
#[derive(Clone, Default)]
struct TargetCapture {
    events: Arc<Mutex<Vec<(String, tracing::Level)>>>,
}

impl tracing::Subscriber for TargetCapture {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::Id {
        tracing::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::Id, _follows: &tracing::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let metadata = event.metadata();
        if let Ok(mut events) = self.events.lock() {
            events.push((metadata.target().to_owned(), *metadata.level()));
        }
    }
    fn enter(&self, _span: &tracing::Id) {}
    fn exit(&self, _span: &tracing::Id) {}
}

#[tokio::test]
async fn ingress_emits_the_pinned_log_target_at_debug() {
    let capture = TargetCapture::default();
    let events = Arc::clone(&capture.events);
    // Global, not thread-local: this binary runs exactly one test, and a
    // global subscriber is immune to the callsite-interest race described in
    // the module docs.
    tracing::subscriber::set_global_default(capture).expect("first subscriber in this process");

    // A real exporter, because `router_wrapper` takes one — the ingress span
    // is built from its tracer. The endpoint is never reached: nothing here
    // waits on an export, and the batch processor's failures are its own
    // business.
    let exporter = OtelExporter::new(
        OtelConfig {
            endpoint: "http://127.0.0.1:1/v1/traces".to_string(),
            ..OtelConfig::default()
        },
        None,
    )
    .expect("exporter builds");

    let router = bitrouter_telemetry::otel::http_layer::router_wrapper(&exporter)(
        axum::Router::new().route("/probe", axum::routing::get(|| async { "ok" })),
    );
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

    let events = events.lock().expect("events").clone();
    assert!(
        events
            .iter()
            .any(|(target, level)| target == "bitrouter::observe::http"
                && *level == tracing::Level::DEBUG),
        "ingress must emit on the pinned `bitrouter::observe::http` target at DEBUG \
         (docs/CLI.md documents it as an operator selector); saw {events:?}"
    );

    exporter.shutdown();
}
