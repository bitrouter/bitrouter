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

pub(crate) mod actions;
pub(crate) mod administration_target;
pub(crate) mod artifact;
pub mod assemble;
pub(crate) mod auth;
pub(crate) mod claude_code;
pub(crate) mod client;
pub(crate) mod cloud;
mod codex_router;
pub(crate) mod commands;
pub mod config;
pub(crate) mod contexts;
pub(crate) mod continuation;
pub(crate) mod daemon;
pub(crate) mod daemon_locator;
pub(crate) mod db;
pub(crate) mod error_report;
pub(crate) mod eval;
pub mod gateway;
pub mod host;
pub(crate) mod managed_contract;
pub(crate) mod mcp_config;
pub(crate) mod mcp_registry;
pub(crate) mod metering;
mod model_policy;

pub(crate) mod offline;
pub(crate) mod onboarding;
pub(crate) mod output;
pub(crate) mod paths;
pub(crate) mod policy;
pub(crate) mod policy_source;
pub(crate) mod policy_table_router;
mod prompt;
mod provider_credentials;
pub(crate) mod providers;
pub(crate) mod reload;
pub(crate) mod remote_control;
pub(crate) mod request_checks;
mod runtime_crypto;
pub(crate) mod session_identity;
pub(crate) mod skills;
pub(crate) mod storage;
pub(crate) mod style;
mod tenancy;
#[cfg(feature = "mcp")]
pub(crate) mod tools;
mod tracing_filter;
pub(crate) mod trajectory;
pub(crate) mod update;

/// Crate version string, surfaced by `bitrouter --version`.
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Optional application telemetry transports.
pub(crate) mod telemetry;

#[path = "main/cli.rs"]
mod cli;

/// Run the bundled operator CLI using this process’s arguments and environment.
///
/// This owns the runtime, standard streams and process exit status. Embedding
/// hosts should use [`host::serve_with_extensions`] instead.
/// The bundled executable prepares Windows standard-handle inheritance before
/// calling this entry point.
pub fn run_cli() {
    cli::entry();
}

#[cfg(test)]
mod tests;
