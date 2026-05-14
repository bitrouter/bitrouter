//! # bitrouter-observe
//!
//! Observability plugin. Provides `ObserveHook` implementations:
//! - [`PrometheusHook`] — always available, dependency-free, renders the
//!   Prometheus text exposition format;
//! - [`otlp::OtlpExportHook`] — behind the `otlp` feature, a self-contained
//!   OTLP/HTTP JSON trace exporter that completes v0's unfinished #409.
//!
//! Every hook here is read-only and error-swallowing (003 §4.6).

#![forbid(unsafe_code)]

pub mod prometheus;

#[cfg(feature = "otlp")]
pub mod otlp;

pub use prometheus::PrometheusHook;

#[cfg(feature = "otlp")]
pub use otlp::OtlpExportHook;

/// Whether the OTLP exporter is compiled in.
pub const OTLP_ENABLED: bool = cfg!(feature = "otlp");
