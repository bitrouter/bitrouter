//! OpenTelemetry exporter with multi-tenant attribution.
//!
//! Provides traces and metrics via OTLP with tenant-aware attributes
//! (api_key_id, user_id) and provider account labels. MVP implementation
//! with two spans: request (root) and execution (child).

mod config;
mod exporter;
mod metrics;
mod cardinality;

pub use config::OtelConfig;
pub use exporter::OtelExporter;

// Re-export for use in benchmarks
#[cfg(test)]
pub use cardinality::CardinalityLimiter;