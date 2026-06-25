//! # bitrouter (library)
//!
//! Assembly layer: turns a [`bitrouter_sdk::config::Config`] into a running
//! [`bitrouter_sdk::App`], and carries the management-command logic. This is
//! the home of v0's `load_builtin_plugins` equivalent.
//!
//! Assembly sits **above** the SDK and the plugins (`plugins → sdk`, sdk never
//! depends back) — see. The `bin` target (`main.rs`) is the CLI/TUI
//! entry point and a thin shell over this lib.

#![forbid(unsafe_code)]

pub mod agent_proxy;
pub mod agents;
pub mod assemble;
pub mod auth;
pub mod claude_code;
pub mod cloud;
pub mod commands;
pub mod daemon;
pub mod db;
pub mod error_report;
pub mod metering;
pub mod output;
pub mod paths;
pub mod policy;
pub mod reload;
pub mod skills;
pub mod spawn;
pub mod style;
pub mod tools;
pub mod update;

pub use assemble::{Assembled, build_app, build_app_with_path, merge_registry_into};

/// Crate version string, surfaced by `bitrouter --version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
