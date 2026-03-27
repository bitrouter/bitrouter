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

use futures_core::Stream;

use crate::routers::admin::ParamRestrictions;
use crate::routers::dynamic_tool::DynamicToolRegistry;
use crate::routers::registry::ToolRegistry;

/// A boxed stream that yields `()` for each change notification.
///
/// Returned by `subscribe_*_changes()` methods. Runtime-agnostic — implementors
/// can back this with any async channel or event source.
pub type ChangeStream = Pin<Box<dyn Stream<Item = ()> + Send + Sync>>;

use super::error::McpGatewayError;
use super::types::{
    CompleteParams, CompleteResult, LoggingLevel, McpGetPromptResult, McpPrompt, McpResource,
    McpResourceContent, McpResourceTemplate, McpTool, McpToolCallResult,
};

/// Trait for serving MCP tools to downstream clients.
///
/// Implementors provide tool listing, tool invocation, and change
/// notification subscription. The API layer's warp filters call these
/// methods to serve the MCP server protocol.
pub trait McpToolServer: Send + Sync {
    /// List available tools, optionally paginated via cursor.
    fn list_tools(
        &self,
        cursor: Option<&str>,
    ) -> impl Future<Output = (Vec<McpTool>, Option<String>)> + Send;

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
    fn subscribe_tool_changes(&self) -> ChangeStream;
}

/// Trait for serving MCP resources to downstream clients.
pub trait McpResourceServer: Send + Sync {
    /// List available resources, optionally paginated via cursor.
    fn list_resources(
        &self,
        cursor: Option<&str>,
    ) -> impl Future<Output = (Vec<McpResource>, Option<String>)> + Send;

    /// Read a namespaced resource by URI (e.g. `"github+file:///readme.md"`).
    fn read_resource(
        &self,
        uri: &str,
    ) -> impl Future<Output = Result<Vec<McpResourceContent>, McpGatewayError>> + Send;

    /// List available resource templates, optionally paginated via cursor.
    fn list_resource_templates(
        &self,
        cursor: Option<&str>,
    ) -> impl Future<Output = (Vec<McpResourceTemplate>, Option<String>)> + Send;

    /// Subscribe to resource list change notifications.
    fn subscribe_resource_changes(&self) -> ChangeStream;

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

/// Trait for serving MCP prompts to downstream clients.
pub trait McpPromptServer: Send + Sync {
    /// List available prompts, optionally paginated via cursor.
    fn list_prompts(
        &self,
        cursor: Option<&str>,
    ) -> impl Future<Output = (Vec<McpPrompt>, Option<String>)> + Send;

    /// Get a namespaced prompt by name (e.g. `"github/summarize"`).
    fn get_prompt(
        &self,
        name: &str,
        arguments: Option<std::collections::HashMap<String, String>>,
    ) -> impl Future<Output = Result<McpGetPromptResult, McpGatewayError>> + Send;

    /// Subscribe to prompt list change notifications.
    fn subscribe_prompt_changes(&self) -> ChangeStream;
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

// ── Blanket impls for Arc<T> ────────────────────────────────────────

impl<T: McpToolServer> McpToolServer for Arc<T> {
    async fn list_tools(&self, cursor: Option<&str>) -> (Vec<McpTool>, Option<String>) {
        (**self).list_tools(cursor).await
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<McpToolCallResult, McpGatewayError> {
        (**self).call_tool(name, arguments).await
    }

    fn subscribe_tool_changes(&self) -> ChangeStream {
        (**self).subscribe_tool_changes()
    }
}

impl<T: McpResourceServer> McpResourceServer for Arc<T> {
    async fn list_resources(&self, cursor: Option<&str>) -> (Vec<McpResource>, Option<String>) {
        (**self).list_resources(cursor).await
    }

    async fn read_resource(&self, uri: &str) -> Result<Vec<McpResourceContent>, McpGatewayError> {
        (**self).read_resource(uri).await
    }

    async fn list_resource_templates(
        &self,
        cursor: Option<&str>,
    ) -> (Vec<McpResourceTemplate>, Option<String>) {
        (**self).list_resource_templates(cursor).await
    }

    fn subscribe_resource_changes(&self) -> ChangeStream {
        (**self).subscribe_resource_changes()
    }

    async fn subscribe_resource(&self, uri: &str) -> Result<(), McpGatewayError> {
        (**self).subscribe_resource(uri).await
    }

    async fn unsubscribe_resource(&self, uri: &str) -> Result<(), McpGatewayError> {
        (**self).unsubscribe_resource(uri).await
    }
}

impl<T: McpPromptServer> McpPromptServer for Arc<T> {
    async fn list_prompts(&self, cursor: Option<&str>) -> (Vec<McpPrompt>, Option<String>) {
        (**self).list_prompts(cursor).await
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<std::collections::HashMap<String, String>>,
    ) -> Result<McpGetPromptResult, McpGatewayError> {
        (**self).get_prompt(name, arguments).await
    }

    fn subscribe_prompt_changes(&self) -> ChangeStream {
        (**self).subscribe_prompt_changes()
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

// ── Blanket impls for DynamicToolRegistry<T> ────────────────────────
//
// These impls live here (where the traits are defined) to satisfy orphan
// rules.  The `DynamicToolRegistry` type is defined in `bitrouter-core`.

/// Split a namespaced name `server/item` on the first `/`.
fn parse_namespaced(name: &str) -> Result<(&str, &str), McpGatewayError> {
    name.split_once('/')
        .ok_or_else(|| McpGatewayError::ToolNotFound {
            name: name.to_owned(),
        })
}

impl<T: McpToolServer + ToolRegistry + Send + Sync> McpToolServer for DynamicToolRegistry<T> {
    async fn list_tools(&self, _cursor: Option<&str>) -> (Vec<McpTool>, Option<String>) {
        let core_tools = <Self as ToolRegistry>::list_tools(self).await;
        let tools = core_tools
            .into_iter()
            .map(|t| McpTool {
                name: t.id,
                description: t.description,
                input_schema: t.input_schema.unwrap_or_default(),
            })
            .collect();
        (tools, None)
    }

    async fn call_tool(
        &self,
        name: &str,
        mut arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<McpToolCallResult, McpGatewayError> {
        let (server_name, tool_name) = parse_namespaced(name)?;

        if let Some(restrictions) = self.get_param_restrictions(server_name) {
            enforce_param_restrictions(name, tool_name, &mut arguments, &restrictions)?;
        }

        self.inner().call_tool(name, arguments).await
    }

    fn subscribe_tool_changes(&self) -> ChangeStream {
        self.inner().subscribe_tool_changes()
    }
}

impl<T: McpResourceServer + ToolRegistry + Send + Sync> McpResourceServer
    for DynamicToolRegistry<T>
{
    async fn list_resources(&self, cursor: Option<&str>) -> (Vec<McpResource>, Option<String>) {
        self.inner().list_resources(cursor).await
    }

    async fn read_resource(&self, uri: &str) -> Result<Vec<McpResourceContent>, McpGatewayError> {
        self.inner().read_resource(uri).await
    }

    async fn list_resource_templates(
        &self,
        cursor: Option<&str>,
    ) -> (Vec<McpResourceTemplate>, Option<String>) {
        self.inner().list_resource_templates(cursor).await
    }

    fn subscribe_resource_changes(&self) -> ChangeStream {
        self.inner().subscribe_resource_changes()
    }

    async fn subscribe_resource(&self, uri: &str) -> Result<(), McpGatewayError> {
        self.inner().subscribe_resource(uri).await
    }

    async fn unsubscribe_resource(&self, uri: &str) -> Result<(), McpGatewayError> {
        self.inner().unsubscribe_resource(uri).await
    }
}

impl<T: McpPromptServer + ToolRegistry + Send + Sync> McpPromptServer for DynamicToolRegistry<T> {
    async fn list_prompts(&self, cursor: Option<&str>) -> (Vec<McpPrompt>, Option<String>) {
        self.inner().list_prompts(cursor).await
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<std::collections::HashMap<String, String>>,
    ) -> Result<McpGetPromptResult, McpGatewayError> {
        self.inner().get_prompt(name, arguments).await
    }

    fn subscribe_prompt_changes(&self) -> ChangeStream {
        self.inner().subscribe_prompt_changes()
    }
}

impl<T: McpLoggingServer + ToolRegistry + Send + Sync> McpLoggingServer for DynamicToolRegistry<T> {
    async fn set_logging_level(&self, level: LoggingLevel) -> Result<(), McpGatewayError> {
        self.inner().set_logging_level(level).await
    }
}

impl<T: McpCompletionServer + ToolRegistry + Send + Sync> McpCompletionServer
    for DynamicToolRegistry<T>
{
    async fn complete(&self, params: CompleteParams) -> Result<CompleteResult, McpGatewayError> {
        self.inner().complete(params).await
    }
}

/// Enforce parameter restrictions at call time.
fn enforce_param_restrictions(
    full_name: &str,
    tool_name: &str,
    arguments: &mut Option<serde_json::Map<String, serde_json::Value>>,
    restrictions: &ParamRestrictions,
) -> Result<(), McpGatewayError> {
    restrictions
        .check(tool_name, arguments)
        .map_err(|e| match e {
            crate::errors::BitrouterError::InvalidRequest { message, .. } => {
                McpGatewayError::ParamDenied {
                    tool: full_name.to_owned(),
                    param: message,
                }
            }
            other => McpGatewayError::UpstreamCall {
                name: full_name.to_owned(),
                reason: other.to_string(),
            },
        })
}
