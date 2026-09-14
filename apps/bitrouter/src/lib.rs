//! # bitrouter (library)
//!
//! Assembly layer: turns a [`bitrouter_sdk::config::Config`] into a running
//! [`bitrouter_sdk::App`], and carries the management-command logic. This is
//! the home of v0's `load_builtin_plugins` equivalent.
//!
//! Assembly sits **above** the SDK and the plugins (`plugins → sdk`, sdk never
//! depends back) — see. The `bin` target (`main.rs`) is the CLI
//! entry point and a thin shell over this lib.

#![forbid(unsafe_code)]

pub mod acp_cli;
pub mod acp_runtime;
pub mod acp_trajectory;
pub mod actions;
pub mod adequacy;
pub mod administration_target;
pub mod agent_registry;
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
pub mod dashboard;
pub mod db;
pub mod error_report;
pub mod eval;
pub mod evolution;
pub mod gateways;
pub mod harness;
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
pub mod result_contract;
pub mod session_identity;
pub mod skills;
pub mod skills_catalog;
pub mod spawn;
pub mod style;
pub mod tools;
pub mod trajectory;
pub mod update;
pub mod workflow_state;

pub use assemble::{Assembled, build_app, build_app_with_path, merge_registry_into};

/// Crate version string, surfaced by `bro --version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
