//! # bitrouter (library)
//!
//! Assembly layer: turns a [`bitrouter_sdk::config::Config`] into a running
//! [`bitrouter_sdk::App`], and carries the management-command logic.
//!
//! Foreground custom hosts register typed capabilities through
//! [`bitrouter_sdk::extension::ExtensionApi`] and [`host::serve_with_extensions`],
//! sharing the product's complete service and management lifecycle. Embeddings
//! that own their listeners can use [`assemble::build_app_with_extensions`].
//! Router configuration explicitly binds capabilities; host assembly retains
//! execution and receipt ownership.
//! The default binary does not link concrete extension implementations. The SDK
//! never depends back on this product host; `main.rs` is the CLI entry point.

#![forbid(unsafe_code)]

pub mod acp_cli;
pub mod acp_runtime;
pub mod acp_trajectory;
pub mod actions;
pub mod adequacy;
pub mod administration_target;
pub mod agent_registry;
pub mod agent_sessions;
pub mod agents;
pub mod assemble;
pub mod auth;
mod bundled_registry;
pub mod chat;
pub mod claude_code;
pub mod cloud;
mod codex_router;
pub mod commands;
pub mod config_synthesis;
pub mod conformance;
pub mod contexts;
pub mod continuation;
pub mod daemon;
pub mod daemon_locator;
pub mod dashboard;
pub mod db;
pub mod error_report;
pub mod eval;
pub mod evaluation_http;
pub mod evolution;
pub mod gateways;
pub mod harness;
pub mod host;
mod local_cli;
pub mod mcp_registry;
pub mod metering;
pub mod onboarding;
pub mod optimization;
pub mod output;
pub mod paths;
pub mod policy;
pub mod policy_compile;
pub mod policy_lock;
pub mod policy_table_router;
mod prompt;
pub mod reload;
pub mod remote_control;
pub mod request_checks;
pub mod result_contract;
pub mod router_migration;
pub mod session_identity;
pub mod skills;
pub mod spawn;
pub mod style;
pub mod supervisor;
pub mod tools;
mod tracing_filter;
pub mod trajectory;
pub mod update;
pub mod workflow_state;

pub use assemble::{Assembled, build_app, build_app_with_path, merge_registry_into};

/// Crate version string, surfaced by `bro --version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
