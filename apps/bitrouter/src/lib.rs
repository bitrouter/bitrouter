//! # bitrouter (library)
//!
//! Assembly layer: turns a [`crate::config::Config`] into a running
//! [`crate::gateway::assembly::App`], and carries the management-command logic.
//!
//! Foreground custom hosts register typed capabilities through
//! [`crate::gateway::extensions::registry::ExtensionApi`] and [`host::serve_with_extensions`],
//! sharing the product's complete service and management lifecycle. Embeddings
//! that own their listeners can use [`assemble::build_app_with_extensions`].
//! Router configuration explicitly binds capabilities; host assembly retains
//! execution and receipt ownership.
//! The default binary does not link concrete extension implementations. The SDK
//! never depends back on this product host; `main.rs` is the CLI entry point.

#![forbid(unsafe_code)]
#![recursion_limit = "256"]

pub mod acp_trajectory;
pub mod actions;
pub mod administration_target;
pub mod artifact;
pub mod assemble;
pub mod auth;
pub mod claude_code;
pub mod client;
pub mod cloud;
mod codex_router;
pub mod commands;
pub mod config;
pub mod connected_agents;
pub mod contexts;
pub mod continuation;
pub mod control_output;
pub mod daemon;
pub mod daemon_handoff;
pub mod daemon_locator;
pub mod db;
pub mod error_report;
pub mod eval;
pub mod gateway;
pub mod host;
pub mod mcp_config;
pub mod mcp_registry;
pub mod metering;
mod model_policy;

pub mod offline;
pub mod onboarding;
pub mod output;
pub mod paths;
pub mod policy;
pub mod policy_lock;
pub mod policy_table_router;
mod prompt;
mod provider_credentials;
pub mod providers;
pub mod reload;
pub mod remote_control;
pub mod request_checks;
mod runtime_crypto;
pub mod session_diagnostics;
pub mod session_identity;
pub mod skills;
pub mod storage;
pub mod style;
mod tenancy;
#[cfg(feature = "mcp")]
pub mod tools;
mod tracing_filter;
pub mod trajectory;
pub mod update;
pub mod upgrade;
pub mod upgrade_preflight;

pub use assemble::{Assembled, build_app, build_app_with_path, merge_registry_into};

/// Crate version string, surfaced by `bitrouter --version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Build fingerprint of the local handoff and migration contract.
pub const HANDOFF_BUILD_ID: &str = env!("BITROUTER_HANDOFF_BUILD_ID");

/// Optional application telemetry transports.
pub mod telemetry;
