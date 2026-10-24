//! # bitrouter (library)
//!
//! Assembly layer: turns a [`bitrouter_sdk::config::Config`] into a running
//! [`bitrouter_sdk::App`], and carries the management-command logic.
//! The external-agent ACP controller, client and process transport live in
//! `bitrouter_orchestrator::acp`; this app assembles their product adapters.
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
pub mod agent_api;
pub mod agent_local;
pub mod agent_registry;
pub mod agent_sessions;
pub mod agent_store;
pub mod agents;
pub mod assemble;
pub mod auth;
mod bundled_registry;
pub mod catalog;
pub mod claude_code;
pub mod cloud;
mod codex_router;
pub mod commands;
pub mod config_synthesis;
pub mod conformance;
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
pub mod evolution;
pub mod gateways;
pub mod harness;
pub mod host;
mod local_cli;
pub mod managed_store;
pub mod managed_task;
pub mod mcp_config;
pub mod mcp_registry;
pub mod metering;
pub mod native_acp;
pub mod onboarding;
pub mod optimization;
pub mod orchestrator_api;
pub mod output;
pub mod paths;
pub mod policy;
pub mod policy_compile;
pub mod policy_lock;
pub mod policy_table_router;
mod prompt;
mod provider_credentials;
pub mod providers;
pub mod reload;
pub mod remote_control;
pub mod request_checks;
pub mod result_contract;
pub mod router_migration;
pub mod session_diagnostics;
pub mod session_identity;
pub mod skills;
pub mod spawn;
pub mod style;
pub mod supervisor;
pub mod thread_cli;
pub mod tools;
mod tracing_filter;
pub mod trajectory;
pub mod update;
pub mod upgrade;
pub mod upgrade_preflight;
pub mod workflow_state;

pub use assemble::{Assembled, build_app, build_app_with_path, merge_registry_into};

/// Crate version string, surfaced by `bro --version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Build fingerprint of the local handoff and migration contract.
pub const HANDOFF_BUILD_ID: &str = env!("BITROUTER_HANDOFF_BUILD_ID");

#[cfg(test)]
mod session_boundary_tests {
    #[test]
    fn retained_headless_services_do_not_use_terminal_business_types() {
        for (name, source) in [
            ("acp_cli", include_str!("acp_cli.rs")),
            ("supervisor", include_str!("supervisor.rs")),
            ("agent_sessions", include_str!("agent_sessions.rs")),
            ("session actions", include_str!("actions/session.rs")),
            ("command reports", include_str!("actions/commands.rs")),
            (
                "session diagnostics",
                include_str!("session_diagnostics.rs"),
            ),
        ] {
            for dependency in ["bitrouter_tui::", "ratatui::", "crossterm::"] {
                assert!(
                    !source.contains(dependency),
                    "{name} depends on terminal presentation through {dependency}"
                );
            }
        }
    }
}
