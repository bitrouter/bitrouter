//! MCP client runtime — connects to upstream MCP servers and aggregates capabilities.
//!
//! This module requires the `client` feature and depends on `rmcp` and `tokio`.
//! The lightweight types (config, error, groups, param_filter, admin trait)
//! live in the parent crate without this feature.

pub mod registry;
pub mod upstream;
