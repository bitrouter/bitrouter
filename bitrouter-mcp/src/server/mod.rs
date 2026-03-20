//! MCP server protocol types and traits.
//!
//! Provides JSON-RPC 2.0 envelope types, MCP protocol messages, tool
//! definitions, and the [`McpToolServer`] trait for serving aggregated
//! tools to downstream MCP clients.
//!
//! These types are `rmcp`-free — they are pure serde structs that match
//! the MCP wire format, allowing `bitrouter-api` to serve the protocol
//! without depending on `rmcp`.

pub mod error_codes;
pub mod jsonrpc;
pub mod protocol;
pub mod types;

use std::future::Future;

use tokio::sync::broadcast;

use crate::error::McpGatewayError;
use protocol::McpGetPromptResult;
use types::{
    McpPrompt, McpResource, McpResourceContent, McpResourceTemplate, McpTool, McpToolCallResult,
};

/// Trait for serving MCP tools to downstream clients.
///
/// Implementors provide tool listing, tool invocation, and change
/// notification subscription. The API layer's warp filters call these
/// methods to serve the MCP server protocol.
pub trait McpToolServer: Send + Sync {
    /// List all available tools with full JSON Schema input definitions.
    fn list_tools(&self) -> impl Future<Output = Vec<McpTool>> + Send;

    /// Invoke a namespaced tool (e.g. `"github/search"`) by name.
    fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> impl Future<Output = Result<McpToolCallResult, McpGatewayError>> + Send;

    /// Subscribe to tool list change notifications.
    ///
    /// Returns a broadcast receiver that yields `()` each time the
    /// aggregated tool list changes (e.g. an upstream added or removed
    /// a tool).
    fn subscribe_tool_changes(&self) -> broadcast::Receiver<()>;
}

/// Trait for serving MCP resources to downstream clients.
pub trait McpResourceServer: Send + Sync {
    /// List all available resources across all upstreams.
    fn list_resources(&self) -> impl Future<Output = Vec<McpResource>> + Send;

    /// Read a namespaced resource by URI (e.g. `"github+file:///readme.md"`).
    fn read_resource(
        &self,
        uri: &str,
    ) -> impl Future<Output = Result<Vec<McpResourceContent>, McpGatewayError>> + Send;

    /// List all available resource templates across all upstreams.
    fn list_resource_templates(&self) -> impl Future<Output = Vec<McpResourceTemplate>> + Send;

    /// Subscribe to resource list change notifications.
    fn subscribe_resource_changes(&self) -> broadcast::Receiver<()>;
}

/// Trait for serving MCP prompts to downstream clients.
pub trait McpPromptServer: Send + Sync {
    /// List all available prompts across all upstreams.
    fn list_prompts(&self) -> impl Future<Output = Vec<McpPrompt>> + Send;

    /// Get a namespaced prompt by name (e.g. `"github/summarize"`).
    fn get_prompt(
        &self,
        name: &str,
        arguments: Option<std::collections::HashMap<String, String>>,
    ) -> impl Future<Output = Result<McpGetPromptResult, McpGatewayError>> + Send;

    /// Subscribe to prompt list change notifications.
    fn subscribe_prompt_changes(&self) -> broadcast::Receiver<()>;
}
