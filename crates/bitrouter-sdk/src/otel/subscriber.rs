//! The `tracing` ↔ OpenTelemetry bridge layer.
//!
//! [`tracing_opentelemetry`] maps `otel.*` sentinel fields on a
//! `tracing::Span` into a real OTel span. The bridge layer itself is not
//! installed by this crate — the host (binary) composes it onto its global
//! subscriber via [`tracing_subscriber_layer`], alongside any `fmt` / file
//! layers it already runs.
//!
//! This module is deliberately free of any HTTP-server dependency so a
//! consumer that only wants the tracing bridge does not have to compile the
//! axum front-end. The inbound SERVER span lives next door in
//! [`crate::otel::http_layer`], behind the `server` feature.

use crate::otel::exporter::OtelExporter;

/// Build a `tracing_subscriber::Layer` that bridges `tracing` spans into
/// the OTel tracer behind `exporter`. Install on the global tracing
/// subscriber alongside any `fmt` / file layers.
///
/// `tracing_opentelemetry::OpenTelemetryLayer` captures its tracer at
/// construction, and `tracing-opentelemetry` only implements
/// `PreSampledTracer` for `opentelemetry_sdk::trace::Tracer` /
/// `opentelemetry::trace::noop::NoopTracer` (not for the `BoxedTracer` you
/// would get from `global::tracer`) — so this helper takes the exporter
/// directly and hands the bridge its concrete SDK tracer. The host binary
/// calls this after building the exporter.
///
/// The return type is `impl Layer<S>` rather than the concrete
/// `OpenTelemetryLayer<S, Tracer>` so that no `opentelemetry*` type appears
/// in this crate's public API — the SDK exports OTLP without making the
/// OpenTelemetry version part of its own semver contract. Callers compose
/// the value straight into a `Registry` chain, which never needs the type
/// named.
///
/// Two deliberate non-choices, both load-bearing:
///
/// - The return type carries **no** `+ Send + Sync + 'static` bound.
///   `OpenTelemetryLayer<S, T>` holds a `PhantomData<S>`, so spelling those
///   auto traits here would force `S: Send + Sync` into the where-clause and
///   narrow the public bound. RPIT auto-trait leakage already delivers
///   `Send`/`Sync` to every caller whose `S` has them.
/// - The layer is **not** boxed. `OpenTelemetrySpanExt::{set_parent,
///   context}` rely on `downcast_raw` finding `WithContext`; boxing hides it
///   behind a forwarding impl and silently breaks SERVER → `chat` parenting.
///
/// The generic `S` is the subscriber the layer is composed onto.
pub fn tracing_subscriber_layer<S>(exporter: &OtelExporter) -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    tracing_opentelemetry::layer().with_tracer(exporter.tracer_clone())
}
