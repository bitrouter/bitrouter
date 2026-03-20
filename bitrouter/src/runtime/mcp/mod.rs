//! MCP gateway runtime — connects to upstream MCP servers and aggregates tools.
//!
//! This module contains the runtime components that depend on `rmcp` and `tokio`.
//! The lightweight types (config, error, groups, param_filter, admin trait)
//! live in the `bitrouter-mcp` library crate.

pub mod gateway;
pub mod registry;
pub mod upstream;
