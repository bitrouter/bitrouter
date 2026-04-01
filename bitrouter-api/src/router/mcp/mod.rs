//! MCP protocol gateway routes.
//!
//! Provides Warp filters for the Model Context Protocol, proxying tool,
//! resource, prompt, subscription, logging and completion operations to
//! any [`McpServer`](bitrouter_core::api::mcp::gateway::McpServer) implementation.

pub mod admin;
mod filters;

#[cfg(test)]
mod tests;

pub use admin::mcp_admin_filter;
pub use filters::{mcp_bridge_filter, mcp_server_filter, mcp_server_filter_with_observe};
