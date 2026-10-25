//! # bitrouter (library)
//!
//! Assembly layer: turns a [`crate::config::Config`] into a running
//! [`crate::gateway::assembly::App`], and carries the management-command logic.
//! The external-agent ACP controller, client and process transport live in
//! `crate::harness::acp`; this app assembles their product adapters.
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

#[cfg(feature = "harness")]
pub mod acp_cli;
pub mod acp_trajectory;
pub mod actions;
pub mod administration_target;
#[cfg(feature = "harness")]
pub mod agent_api;
#[cfg(feature = "harness")]
pub mod agent_registry;
#[cfg(feature = "harness")]
pub mod agent_sessions;
#[cfg(feature = "harness")]
pub mod agent_store;
#[cfg(feature = "harness")]
pub mod agents;
pub mod artifact;
pub mod assemble;
pub mod auth;
pub mod claude_code;
pub mod client;
pub mod cloud;
mod codex_router;
pub mod commands;
pub mod config;
#[cfg(feature = "harness")]
pub mod config_synthesis;
#[cfg(feature = "harness")]
pub mod conformance;
pub mod connected_agents;
pub mod contexts;
pub mod continuation;
pub mod control_output;
#[cfg(feature = "core")]
pub mod core;
pub mod daemon;
pub mod daemon_handoff;
pub mod daemon_locator;
pub mod db;
pub mod error_report;
pub mod gateway;
#[cfg(feature = "harness")]
pub mod gateways;
#[cfg(feature = "harness")]
pub mod harness;
pub mod host;
#[cfg(feature = "harness")]
mod local_cli;
#[cfg(feature = "harness")]
pub mod managed_store;
pub mod managed_task;
pub mod mcp_config;
pub mod mcp_registry;
pub mod metering;
#[cfg(feature = "harness")]
pub mod native_acp;
pub mod thread_cli;

pub mod onboarding;
#[cfg(feature = "core")]
pub mod orchestrator_api;
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
pub mod router_migration;
mod runtime_crypto;
pub mod session_diagnostics;
pub mod session_identity;
pub mod skills;
#[cfg(feature = "harness")]
pub mod spawn;
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

/// Crate version string, surfaced by `bro --version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Build fingerprint of the local handoff and migration contract.
pub const HANDOFF_BUILD_ID: &str = env!("BITROUTER_HANDOFF_BUILD_ID");

/// Optional application telemetry transports.
pub mod telemetry;
