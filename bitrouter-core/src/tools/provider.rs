use dynosaur::dynosaur;

use crate::errors::Result;

use super::result::ToolCallResult;

/// A tool provider that can execute tool invocations.
///
/// This is the tool equivalent of [`LanguageModel`] — each implementation
/// represents a single upstream tool server or agent (e.g. one MCP server,
/// one A2A agent). The provider name is stored on the instance, not passed
/// per-request.
///
/// Protocol-specific details (MCP sessions, A2A task lifecycle, HTTP
/// transport) are handled internally by each implementation.
#[dynosaur(pub DynToolProvider = dyn(box) ToolProvider)]
pub trait ToolProvider: Send + Sync {
    /// The provider name, e.g. `"github-mcp"`, `"my-agent"`.
    fn provider_name(&self) -> &str;

    /// Invokes a tool and returns the result.
    ///
    /// `tool_id` is the bare tool name as known to this provider (not
    /// namespaced). `arguments` carries the tool input as a JSON object.
    fn call_tool(
        &self,
        tool_id: &str,
        arguments: serde_json::Value,
    ) -> impl Future<Output = Result<ToolCallResult>> + Send;
}
