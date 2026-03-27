//! Re-exports of MCP protocol types and traits used by the API handlers.
//!
//! Centralizes imports from `bitrouter_mcp` so handler modules use short
//! paths (`super::types::*`) instead of reaching into the library crate.

// ── Traits ──────────────────────────────────────────────────────────

pub use bitrouter_core::api::mcp::gateway::McpCompletionServer;
pub use bitrouter_core::api::mcp::gateway::McpLoggingServer;
pub use bitrouter_core::api::mcp::gateway::McpPromptServer;
pub use bitrouter_core::api::mcp::gateway::McpResourceServer;
pub use bitrouter_core::api::mcp::gateway::McpToolServer;

/// Combined trait for an MCP server that supports all capabilities.
pub trait McpServer:
    McpToolServer + McpResourceServer + McpPromptServer + McpLoggingServer + McpCompletionServer
{
}
impl<
    T: McpToolServer + McpResourceServer + McpPromptServer + McpLoggingServer + McpCompletionServer,
> McpServer for T
{
}

// ── Error ───────────────────────────────────────────────────────────

pub use bitrouter_core::api::mcp::error::McpGatewayError;

// ── JSON-RPC envelope ───────────────────────────────────────────────

pub use bitrouter_core::api::mcp::types::error_codes;
pub use bitrouter_core::api::mcp::types::{JsonRpcId, JsonRpcMessage, JsonRpcResponse};

// ── Protocol types (request params / response results) ──────────────

pub use bitrouter_core::api::mcp::types::{
    CallToolParams, CompletionsCapability, GetPromptParams, InitializeResult, ListPromptsResult,
    ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, LoggingCapability,
    PromptsCapability, ReadResourceParams, ReadResourceResult, ResourcesCapability,
    ServerCapabilities, ServerInfo, ToolsCapability,
};
