//! Discovery and execution registry types and traits for tools.
//!
//! These are the core abstractions powering tool discovery (`GET /v1/tools`)
//! and protocol-neutral tool execution. `ToolEntry` is protocol-agnostic —
//! conversion from protocol-specific types (MCP tools, A2A agent skills)
//! happens in the respective `api/` convert modules.

use std::future::Future;

use crate::errors::Result;

use super::definition::ToolDefinition;
use super::result::ToolCallResult;

/// A single tool available through the router, with its full definition.
///
/// Unifies MCP tools (structured, schema-driven) and A2A skills
/// (unstructured, tag-driven) into a common discovery type.
#[derive(Debug, Clone)]
pub struct ToolEntry {
    /// Namespaced tool identifier (e.g. `"github/search"`).
    pub id: String,
    /// The server or agent that provides this tool.
    pub provider: String,
    /// Protocol-neutral tool definition.
    pub definition: ToolDefinition,
}

/// Read-only registry for discovering tools available across all sources.
///
/// Implemented by all tool sources — both callable (MCP, A2A) and
/// discovery-only (filesystem skills).
pub trait ToolRegistry: Send + Sync {
    /// Lists all tools available through the router.
    fn list_tools(&self) -> impl Future<Output = Vec<ToolEntry>> + Send;
}

impl<T: ToolRegistry> ToolRegistry for std::sync::Arc<T> {
    async fn list_tools(&self) -> Vec<ToolEntry> {
        (**self).list_tools().await
    }
}

/// Protocol-neutral aggregated tool gateway — list and call.
///
/// Extends [`ToolRegistry`] with `call_tool` for sources that support
/// execution (MCP servers, A2A agents). Discovery-only sources (e.g.
/// filesystem skills) implement [`ToolRegistry`] alone.
///
/// This is the tool equivalent of [`LanguageModelRouter`] — a canonical
/// aggregation point that protocol-specific adapters (`McpToolServer`,
/// A2A gateway) delegate to.
///
/// [`LanguageModelRouter`]: crate::routers::router::LanguageModelRouter
pub trait ToolGateway: ToolRegistry {
    /// Invoke a namespaced tool (e.g. `"github/search"`) and return the result.
    fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> impl Future<Output = Result<ToolCallResult>> + Send;
}

impl<T: ToolGateway> ToolGateway for std::sync::Arc<T> {
    async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> Result<ToolCallResult> {
        (**self).call_tool(name, arguments).await
    }
}

/// Combines two [`ToolRegistry`] implementations into one.
///
/// `list_tools()` returns entries from both registries (primary first).
pub struct CompositeToolRegistry<A, B> {
    primary: A,
    secondary: B,
}

impl<A, B> CompositeToolRegistry<A, B> {
    pub fn new(primary: A, secondary: B) -> Self {
        Self { primary, secondary }
    }
}

impl<A: ToolRegistry, B: ToolRegistry> ToolRegistry for CompositeToolRegistry<A, B> {
    async fn list_tools(&self) -> Vec<ToolEntry> {
        let mut tools = self.primary.list_tools().await;
        tools.extend(self.secondary.list_tools().await);
        tools
    }
}
