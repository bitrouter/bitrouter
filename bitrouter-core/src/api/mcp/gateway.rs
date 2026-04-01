//! MCP server traits and blanket implementations.
//!
//! Provides [`McpToolServer`], [`McpResourceServer`], and [`McpPromptServer`]
//! for serving aggregated MCP capabilities to downstream clients.
//!
//! Blanket implementations for `Arc<T>` and `DynamicToolRegistry<T>` are
//! provided here (where the traits are defined) to satisfy the orphan rules
//! when concrete types live in downstream crates like `bitrouter-providers`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::broadcast;

use crate::routers::dynamic::DynamicRoutingTable;
use crate::routers::registry::ToolRegistry;

use super::types::McpGatewayError;
use super::types::{
    CompleteParams, CompleteResult, CreateMessageParams, CreateMessageResult,
    ElicitationCreateParams, ElicitationCreateResult, JsonRpcError, LoggingLevel,
    McpGetPromptResult, McpPrompt, McpResource, McpResourceContent, McpResourceTemplate, McpTool,
    McpToolCallResult,
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

/// Combined trait for an MCP server that supports all capabilities.
pub trait McpServer:
    McpToolServer
    + McpResourceServer
    + McpPromptServer
    + McpSubscriptionServer
    + McpLoggingServer
    + McpCompletionServer
{
}
impl<
    T: McpToolServer
        + McpResourceServer
        + McpPromptServer
        + McpSubscriptionServer
        + McpLoggingServer
        + McpCompletionServer,
> McpServer for T
{
}

// ── Tool call dispatch ─────────────────────────────────────────────

/// Dyn-safe handler for dispatching `tools/call` requests.
///
/// Decouples tool execution from the `McpToolServer` trait so that
/// `tools/call` can be routed through the [`ToolRouter`](crate::routers::router::ToolRouter)
/// dispatch chain — independent of MCP server capabilities like resources
/// and prompts.
///
/// Uses `Pin<Box<dyn Future>>` for dyn-compatibility, since concrete
/// implementations live in downstream crates.
pub trait ToolCallHandler: Send + Sync {
    /// Dispatch a `tools/call` request by namespaced tool name.
    ///
    /// `name` is the internal namespaced name (e.g. `"github/search"`),
    /// already translated from wire format.
    fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Pin<Box<dyn Future<Output = Result<McpToolCallResult, McpGatewayError>> + Send + '_>>;
}

impl<T: ToolCallHandler> ToolCallHandler for Arc<T> {
    fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Pin<Box<dyn Future<Output = Result<McpToolCallResult, McpGatewayError>> + Send + '_>> {
        (**self).call_tool(name, arguments)
    }
}

// ── Client-side handler for server→client requests ──────────────────

/// Handler for server→client requests (sampling, elicitation).
///
/// When an upstream MCP server sends a request to the client, the
/// transport layer dispatches it to this handler. Implementations
/// provide the application-level logic (e.g. routing to an LLM for
/// sampling, or collecting user input for elicitation).
///
/// Returns `Pin<Box<dyn Future>>` for dyn-compatibility, since concrete
/// handler types live in downstream crates (e.g. the binary crate).
pub trait McpClientRequestHandler: Send + Sync {
    /// Handle a `sampling/createMessage` request from an upstream server.
    ///
    /// `server_name` identifies which upstream MCP server is making the request.
    fn handle_sampling(
        &self,
        server_name: &str,
        params: CreateMessageParams,
    ) -> Pin<Box<dyn Future<Output = Result<CreateMessageResult, JsonRpcError>> + Send + '_>>;

    /// Handle an `elicitation/create` request from an upstream server.
    ///
    /// `server_name` identifies which upstream MCP server is making the request.
    fn handle_elicitation(
        &self,
        server_name: &str,
        params: ElicitationCreateParams,
    ) -> Pin<Box<dyn Future<Output = Result<ElicitationCreateResult, JsonRpcError>> + Send + '_>>;
}

// ── Blanket impls for Arc<T> ────────────────────────────────────────

impl<T: McpToolServer> McpToolServer for Arc<T> {
    async fn list_tools(&self) -> Vec<McpTool> {
        (**self).list_tools().await
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<McpToolCallResult, McpGatewayError> {
        (**self).call_tool(name, arguments).await
    }

    fn subscribe_tool_changes(&self) -> broadcast::Receiver<()> {
        (**self).subscribe_tool_changes()
    }
}

impl<T: McpResourceServer> McpResourceServer for Arc<T> {
    async fn list_resources(&self) -> Vec<McpResource> {
        (**self).list_resources().await
    }

    async fn read_resource(&self, uri: &str) -> Result<Vec<McpResourceContent>, McpGatewayError> {
        (**self).read_resource(uri).await
    }

    async fn list_resource_templates(&self) -> Vec<McpResourceTemplate> {
        (**self).list_resource_templates().await
    }

    fn subscribe_resource_changes(&self) -> broadcast::Receiver<()> {
        (**self).subscribe_resource_changes()
    }
}

impl<T: McpPromptServer> McpPromptServer for Arc<T> {
    async fn list_prompts(&self) -> Vec<McpPrompt> {
        (**self).list_prompts().await
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<std::collections::HashMap<String, String>>,
    ) -> Result<McpGetPromptResult, McpGatewayError> {
        (**self).get_prompt(name, arguments).await
    }

    fn subscribe_prompt_changes(&self) -> broadcast::Receiver<()> {
        (**self).subscribe_prompt_changes()
    }
}

impl<T: McpSubscriptionServer> McpSubscriptionServer for Arc<T> {
    async fn subscribe_resource(&self, uri: &str) -> Result<(), McpGatewayError> {
        (**self).subscribe_resource(uri).await
    }

    async fn unsubscribe_resource(&self, uri: &str) -> Result<(), McpGatewayError> {
        (**self).unsubscribe_resource(uri).await
    }
}

impl<T: McpLoggingServer> McpLoggingServer for Arc<T> {
    async fn set_logging_level(&self, level: LoggingLevel) -> Result<(), McpGatewayError> {
        (**self).set_logging_level(level).await
    }
}

impl<T: McpCompletionServer> McpCompletionServer for Arc<T> {
    async fn complete(&self, params: CompleteParams) -> Result<CompleteResult, McpGatewayError> {
        (**self).complete(params).await
    }
}

// ── Blanket impls for DynamicRoutingTable<T> ────────────────────────
//
// These impls live here (where the traits are defined) to satisfy orphan
// rules.  The `DynamicRoutingTable` type is defined in `bitrouter-core`.

impl<T: McpToolServer + ToolRegistry + Send + Sync> McpToolServer for DynamicRoutingTable<T> {
    async fn list_tools(&self) -> Vec<McpTool> {
        let core_tools = <Self as ToolRegistry>::list_tools(self).await;
        core_tools.into_iter().map(McpTool::from).collect()
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<McpToolCallResult, McpGatewayError> {
        // Delegate to the inner McpToolServer — restriction enforcement
        // is handled at the MCP filter level via ToolCallHandler.
        self.read_inner().call_tool(name, arguments).await
    }

    fn subscribe_tool_changes(&self) -> broadcast::Receiver<()> {
        self.read_inner().subscribe_tool_changes()
    }
}

impl<T: McpResourceServer + ToolRegistry + Send + Sync> McpResourceServer
    for DynamicRoutingTable<T>
{
    async fn list_resources(&self) -> Vec<McpResource> {
        self.read_inner().list_resources().await
    }

    async fn read_resource(&self, uri: &str) -> Result<Vec<McpResourceContent>, McpGatewayError> {
        self.read_inner().read_resource(uri).await
    }

    async fn list_resource_templates(&self) -> Vec<McpResourceTemplate> {
        self.read_inner().list_resource_templates().await
    }

    fn subscribe_resource_changes(&self) -> broadcast::Receiver<()> {
        self.read_inner().subscribe_resource_changes()
    }
}

impl<T: McpPromptServer + ToolRegistry + Send + Sync> McpPromptServer for DynamicRoutingTable<T> {
    async fn list_prompts(&self) -> Vec<McpPrompt> {
        self.read_inner().list_prompts().await
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<std::collections::HashMap<String, String>>,
    ) -> Result<McpGetPromptResult, McpGatewayError> {
        self.read_inner().get_prompt(name, arguments).await
    }

    fn subscribe_prompt_changes(&self) -> broadcast::Receiver<()> {
        self.read_inner().subscribe_prompt_changes()
    }
}

impl<T: McpSubscriptionServer + ToolRegistry + Send + Sync> McpSubscriptionServer
    for DynamicRoutingTable<T>
{
    async fn subscribe_resource(&self, uri: &str) -> Result<(), McpGatewayError> {
        self.read_inner().subscribe_resource(uri).await
    }

    async fn unsubscribe_resource(&self, uri: &str) -> Result<(), McpGatewayError> {
        self.read_inner().unsubscribe_resource(uri).await
    }
}

impl<T: McpLoggingServer + ToolRegistry + Send + Sync> McpLoggingServer for DynamicRoutingTable<T> {
    async fn set_logging_level(&self, level: LoggingLevel) -> Result<(), McpGatewayError> {
        self.read_inner().set_logging_level(level).await
    }
}

impl<T: McpCompletionServer + ToolRegistry + Send + Sync> McpCompletionServer
    for DynamicRoutingTable<T>
{
    async fn complete(&self, params: CompleteParams) -> Result<CompleteResult, McpGatewayError> {
        self.read_inner().complete(params).await
    }
}
