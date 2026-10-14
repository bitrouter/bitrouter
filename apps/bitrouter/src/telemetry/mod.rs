//! Application-owned telemetry egress for the public AI observation contract.
//!
//! Operators retain `plugins.bitrouter-telemetry` and `BITROUTER_TELEMETRY_*`.
//! Wire scopes, meter names and log targets remain stable across this move.
//! HTTP takes precedence when both transports are enabled; gRPC uses static
//! metadata, while HTTP can resolve a fresh bearer for each export.

#![forbid(unsafe_code)]

/// Whether this build contains an OTLP transport.
pub const OTEL_ENABLED: bool = cfg!(any(feature = "otel-http", feature = "otel-grpc"));

#[cfg(all(feature = "harness", any(feature = "otel-http", feature = "otel-grpc")))]
pub mod acp;
#[cfg(feature = "otel-http")]
mod auth_client;
#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
pub mod bearer;
#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
pub mod cardinality;
#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
pub mod config;
#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
pub mod exporter;
#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
pub mod http_layer;
#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
mod metrics;
#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
mod processor_runtime;
#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
pub mod subscriber;
#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
mod transport;
