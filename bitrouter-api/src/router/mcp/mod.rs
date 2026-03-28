//! MCP protocol gateway routes.
//!
//! Provides Warp filters for the Model Context Protocol, proxying tool,
//! resource, prompt, subscription, logging and completion operations to
//! any [`McpServer`](types::McpServer) implementation.

mod filters;
#[cfg(test)]
mod tests;
mod types;

pub use filters::{mcp_bridge_filter, mcp_server_filter, mcp_server_filter_with_observe};
