//! # bitrouter-observe
//!
//! Observability plugin providing OpenTelemetry traces and metrics with
//! multi-tenant attribution. All observability is read-only and error-swallowing
//! to ensure it never impacts request processing.

#![forbid(unsafe_code)]

// The observability stack is transport-agnostic but cannot function without a
// wire transport. `otel-base` carries the stack; `otel-http` / `otel-grpc` add
// a transport. Guard against `otel-base` being enabled on its own (e.g. a
// downstream typo or a stray `dep:` activation) with a clear message instead
// of a wall of "cannot find function `span_exporter`" errors.
#[cfg(all(
    feature = "otel-base",
    not(any(feature = "otel-http", feature = "otel-grpc"))
))]
compile_error!(
    "the OpenTelemetry stack needs a transport: enable `otel-http` for \
     OTLP/HTTP, or `otel-grpc` for OTLP/gRPC"
);

#[cfg(feature = "otel-base")]
pub mod otel;

/// Whether the OpenTelemetry exporter is compiled in (under any transport).
pub const OTEL_ENABLED: bool = cfg!(feature = "otel-base");
