//! MCP transport trait and implementations.
//!
//! Defines [`McpTransport`] -- the wire-level interface for communicating
//! with upstream MCP servers. Implementations handle protocol framing,
//! session management, and type conversion.

use std::collections::HashMap;
use std::future::Future;

use bitrouter_core::api::mcp::error::McpGatewayError;
use bitrouter_core::api::mcp::types::{
    InitializeResult, McpGetPromptResult, McpPrompt, McpResource, McpResourceContent,
    McpResourceTemplate, McpTool, McpToolCallResult,
};

/// Wire-level transport for communicating with an upstream MCP server.
///
/// Implementors handle protocol framing (JSON-RPC, stdio, etc.) and
/// type conversion. Higher-level concerns like caching, namespacing,
/// and access control are managed by [`UpstreamConnection`](crate::mcp::client::upstream::UpstreamConnection).
pub trait McpTransport: Send + Sync {
    /// Perform the MCP `initialize` handshake.
    fn initialize(&self) -> impl Future<Output = Result<InitializeResult, McpGatewayError>> + Send;

    /// Terminate the MCP session.
    fn terminate(&self) -> impl Future<Output = ()> + Send;

    /// List all tools from the upstream, handling pagination.
    fn list_tools(&self) -> impl Future<Output = Result<Vec<McpTool>, McpGatewayError>> + Send;

    /// Invoke a tool by name.
    fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> impl Future<Output = Result<McpToolCallResult, McpGatewayError>> + Send;

    /// List all resources from the upstream.
    fn list_resources(
        &self,
    ) -> impl Future<Output = Result<Vec<McpResource>, McpGatewayError>> + Send;

    /// Read a single resource by URI.
    fn read_resource(
        &self,
        uri: &str,
    ) -> impl Future<Output = Result<Vec<McpResourceContent>, McpGatewayError>> + Send;

    /// List all resource templates from the upstream.
    fn list_resource_templates(
        &self,
    ) -> impl Future<Output = Result<Vec<McpResourceTemplate>, McpGatewayError>> + Send;

    /// List all prompts from the upstream.
    fn list_prompts(&self) -> impl Future<Output = Result<Vec<McpPrompt>, McpGatewayError>> + Send;

    /// Get a prompt by name.
    fn get_prompt(
        &self,
        name: &str,
        arguments: Option<HashMap<String, String>>,
    ) -> impl Future<Output = Result<McpGetPromptResult, McpGatewayError>> + Send;
}

pub mod http;

#[cfg(feature = "mcp-stdio")]
pub mod stdio;

/// Internal transport dispatch enum.
///
/// Since [`McpTransport`] uses RPITIT and is not object-safe,
/// this enum provides static dispatch across transport backends.
pub(crate) enum TransportKind {
    Http(http::McpHttpClient),
    #[cfg(feature = "mcp-stdio")]
    Stdio(stdio::StdioConnection),
}

impl McpTransport for TransportKind {
    async fn initialize(&self) -> Result<InitializeResult, McpGatewayError> {
        match self {
            Self::Http(c) => c.initialize().await,
            #[cfg(feature = "mcp-stdio")]
            Self::Stdio(c) => c.initialize().await,
        }
    }

    async fn terminate(&self) {
        match self {
            Self::Http(c) => c.terminate().await,
            #[cfg(feature = "mcp-stdio")]
            Self::Stdio(c) => c.terminate().await,
        }
    }

    async fn list_tools(&self) -> Result<Vec<McpTool>, McpGatewayError> {
        match self {
            Self::Http(c) => c.list_tools().await,
            #[cfg(feature = "mcp-stdio")]
            Self::Stdio(c) => c.list_tools().await,
        }
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<McpToolCallResult, McpGatewayError> {
        match self {
            Self::Http(c) => c.call_tool(name, arguments).await,
            #[cfg(feature = "mcp-stdio")]
            Self::Stdio(c) => c.call_tool(name, arguments).await,
        }
    }

    async fn list_resources(&self) -> Result<Vec<McpResource>, McpGatewayError> {
        match self {
            Self::Http(c) => c.list_resources().await,
            #[cfg(feature = "mcp-stdio")]
            Self::Stdio(c) => c.list_resources().await,
        }
    }

    async fn read_resource(&self, uri: &str) -> Result<Vec<McpResourceContent>, McpGatewayError> {
        match self {
            Self::Http(c) => c.read_resource(uri).await,
            #[cfg(feature = "mcp-stdio")]
            Self::Stdio(c) => c.read_resource(uri).await,
        }
    }

    async fn list_resource_templates(&self) -> Result<Vec<McpResourceTemplate>, McpGatewayError> {
        match self {
            Self::Http(c) => c.list_resource_templates().await,
            #[cfg(feature = "mcp-stdio")]
            Self::Stdio(c) => c.list_resource_templates().await,
        }
    }

    async fn list_prompts(&self) -> Result<Vec<McpPrompt>, McpGatewayError> {
        match self {
            Self::Http(c) => c.list_prompts().await,
            #[cfg(feature = "mcp-stdio")]
            Self::Stdio(c) => c.list_prompts().await,
        }
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<HashMap<String, String>>,
    ) -> Result<McpGetPromptResult, McpGatewayError> {
        match self {
            Self::Http(c) => c.get_prompt(name, arguments).await,
            #[cfg(feature = "mcp-stdio")]
            Self::Stdio(c) => c.get_prompt(name, arguments).await,
        }
    }
}
