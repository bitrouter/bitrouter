//! OpenTelemetry exporter with multi-tenant attribution.
//!
//! Four-layer span hierarchy aligned with the OTel GenAI semantic
//! conventions (<https://opentelemetry.io/docs/specs/semconv/gen-ai/>):
//! HTTP SERVER → `chat` INTERNAL → (`route` INTERNAL, per-hop `chat` CLIENT
//! spans, `settle` INTERNAL). Inbound W3C trace context is honoured;
//! outbound `traceparent` is injected on every upstream hop.
//!
//! Every one of those names, and every attribute stamped on them, is declared
//! in [`bitrouter_sdk::observe::schema`] rather than here. This module is one
//! renderer of that declaration, and its conformance tests assert as much: an
//! attribute this module emits that the schema does not declare is a test
//! failure, not a new attribute.
//!
//! The OTLP wire transport is selected at compile time by the crate's feature
//! flags — `otel-http` (OTLP/HTTP+protobuf) and/or `otel-grpc` (OTLP/gRPC).
//! See the `transport` module for the selection rules. Both traces and metrics
//! ride the same chosen transport.
//!
//! No `opentelemetry*` type appears in this module's public API. That was
//! originally a constraint on the SDK's semver contract; it is kept here
//! because it is the property that lets the OpenTelemetry version move without
//! moving BitRouter's, and because `otel::subscriber`'s bridge would otherwise
//! put a `tracing-opentelemetry` type into a published signature.

pub mod acp;
// The custom OTLP/HTTP client that injects a live bearer per export. HTTP-only
// — the cloud/default telemetry path is OTLP/HTTP; the gRPC transport keeps the
// static-header behaviour (see `transport`).
#[cfg(feature = "otel-http")]
mod auth_client;
mod bearer;
mod cardinality;
mod config;
mod exporter;
// The inbound SERVER span wraps the SDK's axum `Router`, so it only compiles
// when that front-end does. The transport-agnostic half — the `tracing` ↔ OTel
// bridge — lives in `subscriber` and has no such gate, so a consumer that
// installs the bridge on its own server does not pay for axum.
#[cfg(feature = "server")]
#[cfg_attr(docsrs, doc(cfg(all(feature = "otel", feature = "server"))))]
pub mod http_layer;
mod metrics;
mod processor_runtime;
pub mod subscriber;
mod transport;

pub use bearer::TelemetryBearer;
pub use cardinality::CardinalityLimiter;
pub use config::{
    BatchConfig, ContentCaptureMode, DEFAULT_CONTENT_ATTR_MAX_BYTES, MetricsConfig, OtelConfig,
    SamplerKind, TraceConfig,
};
pub use exporter::{OtelExporter, OtelObserveHook, OtelStatus};
