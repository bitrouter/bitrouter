//! MCP server traits.
//!
//! Provides [`McpToolServer`], [`McpResourceServer`], and [`McpPromptServer`]
//! for serving aggregated MCP capabilities to downstream clients.

use std::future::Future;

use tokio::sync::broadcast;

use crate::error::McpGatewayError;
use crate::types::{
    CompleteParams, CompleteResult, LoggingLevel, McpGetPromptResult, McpPrompt, McpResource,
    McpResourceContent, McpResourceTemplate, McpTool, McpToolCallResult,
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

/// Trait for handling per-resource subscriptions.
///
/// When a client subscribes to a resource URI, the server should send
/// `notifications/resources/updated` over the SSE channel when the
/// resource changes.
pub trait McpSubscriptionServer: Send + Sync {
    /// Subscribe to updates for a specific resource URI.
    fn subscribe_resource(
        &self,
        uri: &str,
    ) -> impl Future<Output = Result<(), McpGatewayError>> + Send;

    /// Unsubscribe from updates for a specific resource URI.
    fn unsubscribe_resource(
        &self,
        uri: &str,
    ) -> impl Future<Output = Result<(), McpGatewayError>> + Send;
}

/// Trait for managing the server's logging level.
pub trait McpLoggingServer: Send + Sync {
    /// Set the logging level for the server.
    fn set_logging_level(
        &self,
        level: LoggingLevel,
    ) -> impl Future<Output = Result<(), McpGatewayError>> + Send;
}

/// Trait for providing argument auto-completion.
pub trait McpCompletionServer: Send + Sync {
    /// Provide completion suggestions for a prompt or resource argument.
    fn complete(
        &self,
        params: CompleteParams,
    ) -> impl Future<Output = Result<CompleteResult, McpGatewayError>> + Send;
}
