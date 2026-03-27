//! 1:1 stdio-to-HTTP bridge for a single upstream MCP server.
//!
//! [`SingleServerBridge`] wraps an [`UpstreamConnection`] and re-exposes its
//! tools, resources, and prompts under their original names (no namespace
//! prefix), so that external MCP clients can address the server directly via
//! `POST /mcp/{name}` and `GET /mcp/{name}/sse`.
//!
//! The bridge shares the same [`UpstreamConnection`] as the aggregated
//! [`ConfigMcpRegistry`](crate::mcp::client::registry::ConfigMcpRegistry), so
//! only one child process is spawned per stdio server.  Change notifications
//! are forwarded from the registry's downstream broadcast channels to the
//! bridge's own broadcast channels.

use std::sync::Arc;

use tokio::sync::broadcast;

use super::registry::RefreshGuard;
use super::upstream::UpstreamConnection;
use bitrouter_core::api::mcp::error::McpGatewayError;
use bitrouter_core::api::mcp::gateway::{
    ChangeStream, McpCompletionServer, McpLoggingServer, McpPromptServer, McpResourceServer,
    McpToolServer,
};
use bitrouter_core::api::mcp::types::{
    CompleteParams, CompleteResult, Completion, LoggingLevel, McpGetPromptResult, McpPrompt,
    McpResource, McpResourceContent, McpResourceTemplate, McpTool, McpToolCallResult,
};
use tokio_stream::StreamExt;

/// A bridge that re-exposes a single upstream [`UpstreamConnection`] as an
/// independent MCP server without name-prefixing.
///
/// Build with [`SingleServerBridge::new`], then pass the resulting
/// `Arc<SingleServerBridge>` to `mcp_bridge_filter`.
pub struct SingleServerBridge {
    upstream: Arc<UpstreamConnection>,
    tool_change_tx: broadcast::Sender<()>,
    resource_change_tx: broadcast::Sender<()>,
    prompt_change_tx: broadcast::Sender<()>,
}

impl SingleServerBridge {
    /// Create a bridge from a shared upstream connection.
    ///
    /// The `upstream_tool_rx`, `upstream_resource_rx`, and `upstream_prompt_rx`
    /// parameters should be subscriptions from the aggregated registry's
    /// downstream broadcast channels (i.e. `registry.subscribe_tool_changes()`
    /// etc.).  When the registry notifies downstream clients of a cache refresh,
    /// the bridge forwards the notification to its own subscribers.
    ///
    /// Returns the bridge and a [`RefreshGuard`] that keeps the background
    /// forwarding tasks alive.  Drop the guard to stop background activity.
    pub fn new(
        upstream: Arc<UpstreamConnection>,
        upstream_tool_rx: ChangeStream,
        upstream_resource_rx: ChangeStream,
        upstream_prompt_rx: ChangeStream,
    ) -> (Arc<Self>, RefreshGuard) {
        let (tool_tx, _) = broadcast::channel(16);
        let (resource_tx, _) = broadcast::channel(16);
        let (prompt_tx, _) = broadcast::channel(16);

        let bridge = Arc::new(Self {
            upstream,
            tool_change_tx: tool_tx,
            resource_change_tx: resource_tx,
            prompt_change_tx: prompt_tx,
        });

        let guard = bridge.spawn_forward_listeners(
            upstream_tool_rx,
            upstream_resource_rx,
            upstream_prompt_rx,
        );
        (bridge, guard)
    }

    /// Spawn background tasks that forward registry notifications to bridge subscribers.
    fn spawn_forward_listeners(
        self: &Arc<Self>,
        mut tool_rx: ChangeStream,
        mut resource_rx: ChangeStream,
        mut prompt_rx: ChangeStream,
    ) -> RefreshGuard {
        let mut handles = Vec::new();

        // Forward tool-change notifications.
        let tx = self.tool_change_tx.clone();
        handles.push(tokio::spawn(async move {
            while tool_rx.next().await.is_some() {
                let _ = tx.send(());
            }
        }));

        // Forward resource-change notifications.
        let tx = self.resource_change_tx.clone();
        handles.push(tokio::spawn(async move {
            while resource_rx.next().await.is_some() {
                let _ = tx.send(());
            }
        }));

        // Forward prompt-change notifications.
        let tx = self.prompt_change_tx.clone();
        handles.push(tokio::spawn(async move {
            while prompt_rx.next().await.is_some() {
                let _ = tx.send(());
            }
        }));

        RefreshGuard::from_handles(handles)
    }
}

impl McpToolServer for SingleServerBridge {
    async fn list_tools(&self, _cursor: Option<&str>) -> (Vec<McpTool>, Option<String>) {
        (self.upstream.raw_tools().await, None)
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<McpToolCallResult, McpGatewayError> {
        self.upstream.call_tool(name, arguments).await
    }

    fn subscribe_tool_changes(&self) -> ChangeStream {
        Box::pin(
            tokio_stream::wrappers::BroadcastStream::new(self.tool_change_tx.subscribe())
                .filter_map(|r| r.ok()),
        )
    }
}

impl McpResourceServer for SingleServerBridge {
    async fn list_resources(&self, _cursor: Option<&str>) -> (Vec<McpResource>, Option<String>) {
        (self.upstream.raw_resources().await, None)
    }

    async fn read_resource(&self, uri: &str) -> Result<Vec<McpResourceContent>, McpGatewayError> {
        self.upstream.read_resource(uri).await
    }

    async fn list_resource_templates(
        &self,
        _cursor: Option<&str>,
    ) -> (Vec<McpResourceTemplate>, Option<String>) {
        (self.upstream.raw_resource_templates().await, None)
    }

    fn subscribe_resource_changes(&self) -> ChangeStream {
        Box::pin(
            tokio_stream::wrappers::BroadcastStream::new(self.resource_change_tx.subscribe())
                .filter_map(|r| r.ok()),
        )
    }

    async fn subscribe_resource(&self, _uri: &str) -> Result<(), McpGatewayError> {
        Ok(())
    }

    async fn unsubscribe_resource(&self, _uri: &str) -> Result<(), McpGatewayError> {
        Ok(())
    }
}

impl McpPromptServer for SingleServerBridge {
    async fn list_prompts(&self, _cursor: Option<&str>) -> (Vec<McpPrompt>, Option<String>) {
        (self.upstream.raw_prompts().await, None)
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<std::collections::HashMap<String, String>>,
    ) -> Result<McpGetPromptResult, McpGatewayError> {
        self.upstream.get_prompt(name, arguments).await
    }

    fn subscribe_prompt_changes(&self) -> ChangeStream {
        Box::pin(
            tokio_stream::wrappers::BroadcastStream::new(self.prompt_change_tx.subscribe())
                .filter_map(|r| r.ok()),
        )
    }
}

impl McpLoggingServer for SingleServerBridge {
    async fn set_logging_level(&self, _level: LoggingLevel) -> Result<(), McpGatewayError> {
        Ok(())
    }
}

/// Returns empty completions — upstreams do not yet expose completion support
/// through the bridge.
impl McpCompletionServer for SingleServerBridge {
    async fn complete(&self, _params: CompleteParams) -> Result<CompleteResult, McpGatewayError> {
        Ok(CompleteResult {
            completion: Completion {
                values: Vec::new(),
                has_more: None,
                total: None,
            },
        })
    }
}
