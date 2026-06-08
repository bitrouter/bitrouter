//! OpenTelemetry exporter with multi-tenant attribution.
//!
//! Four-layer span hierarchy aligned with the OTel GenAI semantic
//! conventions (<https://opentelemetry.io/docs/specs/semconv/gen-ai/>):
//! HTTP SERVER → `chat` INTERNAL → (`route` INTERNAL, per-hop `chat` CLIENT
//! spans, `settle` INTERNAL). Inbound W3C trace context is honoured;
//! outbound `traceparent` is injected on every upstream hop.
//!
//! The OTLP wire transport is selected at compile time by the crate's feature
//! flags — `otel-http` (OTLP/HTTP+protobuf) and/or `otel-grpc` (OTLP/gRPC).
//! See the `transport` module for the selection rules. Both traces and metrics
//! ride the same chosen transport.

mod cardinality;
mod config;
mod exporter;
pub mod http_layer;
mod metrics;
mod span_attributes;
mod transport;

pub use cardinality::CardinalityLimiter;
pub use config::{
    BatchConfig, ContentCaptureMode, MetricsConfig, OtelConfig, SamplerKind, TraceConfig,
};
pub use exporter::{OtelExporter, OtelObserveHook, OtelStatus};
pub use span_attributes::SpanAttributes;
