//! Re-exports of MCP protocol types and traits used by the API handlers.
//!
//! Centralizes imports from `bitrouter_mcp` so handler modules use short
//! paths (`super::types::*`) instead of reaching into the library crate.

// ── Traits ──────────────────────────────────────────────────────────

pub use bitrouter_mcp::server::McpPromptServer;
pub use bitrouter_mcp::server::McpResourceServer;
pub use bitrouter_mcp::server::McpToolServer;

/// Combined trait for an MCP server that supports all capabilities.
pub trait McpServer: McpToolServer + McpResourceServer + McpPromptServer {}
impl<T: McpToolServer + McpResourceServer + McpPromptServer> McpServer for T {}

// ── Error ───────────────────────────────────────────────────────────

pub use bitrouter_mcp::error::McpGatewayError;

// ── JSON-RPC envelope ───────────────────────────────────────────────

pub use bitrouter_mcp::types::error_codes;
pub use bitrouter_mcp::types::{JsonRpcId, JsonRpcMessage, JsonRpcResponse};

// ── Protocol types (request params / response results) ──────────────

pub use bitrouter_mcp::types::{
    CallToolParams, GetPromptParams, InitializeResult, ListPromptsResult,
    ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, PromptsCapability,
    ReadResourceParams, ReadResourceResult, ResourcesCapability, ServerCapabilities, ServerInfo,
    ToolsCapability,
};
