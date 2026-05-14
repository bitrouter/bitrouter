//! # bitrouter-sdk
//!
//! The BitRouter SDK. Merges v0's `bitrouter-core` + `bitrouter-api` +
//! `bitrouter-config` + `bitrouter-providers` into a single crate.
//!
//! Each routing protocol is a module with its own `Pipeline`, `PipelineContext`,
//! `RoutingTable`, `Router` and hook traits. There is **no protocol generic and
//! no cross-protocol shared hook trait** (see design doc 003 §0). Reuse is via
//! shared library code (structs / fns) at the crate root, not shared traits.
//!
//! ## Feature flags
//!
//! - `server` — axum HTTP handlers, SSE, admin endpoints.
//! - `config_file` — yaml config loading (`serde-saphyr` + `tokio::fs`).

#![forbid(unsafe_code)]

// ===== shared library code (crate root) =====
pub mod app;
pub mod caller;
pub mod error;
pub mod event;
pub mod metrics;
pub mod mpp;
pub mod plugin;

#[cfg(feature = "config_file")]
pub mod config;

#[cfg(feature = "server")]
pub mod server;

// ===== per-protocol modules =====
pub mod acp;
pub mod language_model;
pub mod mcp;

pub use app::{App, AppBuilder, Plugin};
pub use caller::{CallerContext, FundingSource, PaymentMethod};
pub use error::{BitrouterError, Result};
pub use event::{EventBus, PipelineEvent};
pub use metrics::{MetricsStore, RateMetrics, RequestMetric, TimeWindow, TokenUsage};
pub use mpp::{MppVerification, MppVerifier};
pub use plugin::{MigrationContent, MigrationItem, PluginId};
