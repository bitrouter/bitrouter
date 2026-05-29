//! OpenTelemetry exporter with multi-tenant attribution.
//!
//! Four-layer span hierarchy aligned with the OTel GenAI semantic
//! conventions (<https://opentelemetry.io/docs/specs/semconv/gen-ai/>):
//! HTTP SERVER → `chat` INTERNAL → (`route` INTERNAL, per-hop `chat` CLIENT
//! spans, `settle` INTERNAL). Inbound W3C trace context is honoured;
//! outbound `traceparent` is injected on every upstream hop. OTLP/HTTP+
//! protobuf push for both traces and metrics.

mod cardinality;
mod config;
mod exporter;
pub mod http_layer;
mod metrics;

pub use cardinality::CardinalityLimiter;
pub use config::{BatchConfig, MetricsConfig, OtelConfig, SamplerKind, TraceConfig};
pub use exporter::{OtelExporter, OtelObserveHook, OtelStatus};
