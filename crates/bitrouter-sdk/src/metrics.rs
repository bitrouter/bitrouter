//! Observability rendering interface — the `GET /metrics` endpoint contract.
//!
//! This is a *pull*-shaped seam: the HTTP server needs to render Prometheus
//! text without knowing where the counters come from. The accumulator side is
//! deployment-specific, and the OSS binary no longer has one: it pushes
//! metrics over OTLP via the SDK's own [`otel`](crate::otel) feature and
//! mounts a stub renderer that serves a migration banner. A deployment that
//! still wants a pull-based endpoint registers its own
//! [`ObserveHook`](crate::language_model::ObserveHook) and points this trait
//! at it.
//!
//! The SDK's *push* path is a different kind of thing and does not weaken the
//! rule here. What `otel` owns is the **span schema** — the span names, the
//! `bitrouter.*` attribute vocabulary, and the invariants a deployment cannot
//! be trusted to re-derive; the OTLP renderer ships with it because there is
//! one renderer and it is default-off. `MetricsRenderer` is unchanged by it:
//! the SDK still owns the trait and never the accumulator.
//!
//! Spend / token / rate aggregations are *not* SDK concerns. Any deployment
//! that needs them owns its own storage; see the OSS binary's `metering`
//! module for the reference implementation.

/// A renderer of Prometheus-style text-exposition metrics. The SDK's HTTP
/// server mounts `GET /metrics` against this trait. The trait is
/// deliberately tiny — synchronous, returns owned text — so any in-process
/// accumulator can implement it without dragging Prometheus library types
/// into the SDK.
pub trait MetricsRenderer: Send + Sync {
    /// Render the current accumulator state as a Prometheus text-exposition
    /// payload. Called once per `GET /metrics` request.
    fn render(&self) -> String;

    /// The MIME content-type the renderer wants to advertise on the response.
    /// Defaults to the Prometheus text exposition v0.0.4 type.
    fn content_type(&self) -> &'static str {
        "text/plain; version=0.0.4; charset=utf-8"
    }
}
