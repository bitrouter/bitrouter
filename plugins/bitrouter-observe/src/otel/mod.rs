//! OpenTelemetry exporter with multi-tenant attribution.
//!
//! Two spans per request (request root + execution child), GenAI-semconv
//! attributes, W3C `traceparent` propagation, and OTLP/HTTP+protobuf push for
//! both traces and metrics.

mod cardinality;
mod config;
mod exporter;
mod metrics;

pub use cardinality::CardinalityLimiter;
pub use config::{BatchConfig, MetricsConfig, OtelConfig, SamplerKind, TraceConfig};
pub use exporter::{OtelExporter, OtelObserveHook, OtelStatus};
