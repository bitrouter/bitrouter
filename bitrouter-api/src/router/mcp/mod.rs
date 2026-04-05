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
#[cfg(any(feature = "mpp-tempo", feature = "mpp-solana"))]
pub use filters::mcp_server_filter_with_payment_gate;
pub use filters::{mcp_bridge_filter, mcp_server_filter, mcp_server_filter_with_observe};
