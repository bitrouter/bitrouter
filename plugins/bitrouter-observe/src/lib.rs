//! # bitrouter-observe
//!
//! Observability plugin providing OpenTelemetry traces and metrics with
//! multi-tenant attribution. All observability is read-only and error-swallowing
//! to ensure it never impacts request processing.

#![forbid(unsafe_code)]

#[cfg(feature = "otel")]
pub mod otel;

/// Whether the OpenTelemetry exporter is compiled in.
pub const OTEL_ENABLED: bool = cfg!(feature = "otel");
