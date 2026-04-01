use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Notify, RwLock, broadcast};

use super::config::McpServerConfig;

use bitrouter_core::api::mcp::gateway::{
    McpClientRequestHandler, McpCompletionServer, McpLoggingServer, McpPromptServer,
    McpResourceServer, McpSubscriptionServer, McpToolServer,
};
use bitrouter_core::api::mcp::types::McpGatewayError;
use bitrouter_core::api::mcp::types::{
    CompleteParams, CompleteResult, Completion, LoggingLevel, McpGetPromptResult, McpPrompt,
    McpResource, McpResourceContent, McpResourceTemplate, McpTool, McpToolCallResult,
};

use super::upstream::UpstreamConnection;
use crate::util::RefreshGuard;

/// Aggregates multiple upstream MCP connections and routes tool calls.
///
/// This is the raw tool source — it provides unfiltered tools from all
/// upstreams. Filter and restriction management is handled by the
/// [`DynamicToolRegistry`] wrapper that wraps this registry.
pub struct ConfigMcpRegistry {
    upstreams: RwLock<HashMap<String, Arc<UpstreamConnection>>>,
    /// Broadcast sender for notifying downstream MCP clients of tool list changes.
    tool_change_tx: broadcast::Sender<()>,
    /// Broadcast sender for notifying downstream MCP clients of resource list changes.
    resource_change_tx: broadcast::Sender<()>,
    /// Broadcast sender for notifying downstream MCP clients of prompt list changes.
    prompt_change_tx: broadcast::Sender<()>,
}

impl ConfigMcpRegistry {
    /// Build a registry from pre-built, Arc-wrapped upstream connections.
    ///
    /// This is the preferred constructor when connections are created externally
    /// (e.g. to share them with bridge endpoints).
    pub fn from_connections(connections: HashMap<String, Arc<UpstreamConnection>>) -> Self {
        let (tool_change_tx, _) = broadcast::channel(16);
        let (resource_change_tx, _) = broadcast::channel(16);
        let (prompt_change_tx, _) = broadcast::channel(16);
        Self {
            upstreams: RwLock::new(connections),
            tool_change_tx,
            resource_change_tx,
            prompt_change_tx,
        }
    }

    /// Connect to all configured upstreams. Fails on first error or duplicate name.
    ///
    /// If a `handler` is provided, all connections will handle server→client
    /// requests (sampling, elicitation) by dispatching to it.
    pub async fn from_configs(
        configs: Vec<McpServerConfig>,
        handler: Option<Arc<dyn McpClientRequestHandler>>,
    ) -> Result<Self, McpGatewayError> {
        // Check for duplicate names
        let mut seen = std::collections::HashSet::new();
        for config in &configs {
            if !seen.insert(&config.name) {
                return Err(McpGatewayError::InvalidConfig {
                    reason: format!("duplicate server name '{}'", config.name),
                });
            }
        }

        let mut connections = HashMap::with_capacity(configs.len());
        for config in configs {
            let name = config.name.clone();
            tracing::info!(upstream = %name, "connecting to upstream");
            let conn = UpstreamConnection::connect(config, handler.clone()).await?;
            connections.insert(name, Arc::new(conn));
        }

        Ok(Self::from_connections(connections))
    }

    /// Spawn background tasks that listen for upstream change notifications
    /// and refresh tool, resource, and prompt caches accordingly.
    ///
    /// Returns a [`RefreshGuard`] that aborts all tasks when dropped.
    pub async fn spawn_refresh_listeners(self: &Arc<Self>) -> RefreshGuard {
        let mut handles = Vec::new();

        for (name, notify) in self.tool_change_notifiers().await {
            let reg = Arc::clone(self);
            handles.push(tokio::spawn(async move {
                loop {
                    notify.notified().await;
                    tracing::info!(upstream = %name, "tool list changed, refreshing");
                    if let Err(e) = reg.refresh_upstream(&name).await {
                        tracing::warn!(upstream = %name, error = %e, "failed to refresh tools");
                    } else {
                        reg.notify_downstream_tool_change();
                    }
                }
            }));
        }

        for (name, notify) in self.resource_change_notifiers().await {
            let reg = Arc::clone(self);
            handles.push(tokio::spawn(async move {
                loop {
                    notify.notified().await;
                    tracing::info!(upstream = %name, "resource list changed, refreshing");
                    if let Err(e) = reg.refresh_upstream_resources(&name).await {
                        tracing::warn!(upstream = %name, error = %e, "failed to refresh resources");
                    } else {
                        reg.notify_downstream_resource_change();
                    }
                }
            }));
        }

        for (name, notify) in self.prompt_change_notifiers().await {
            let reg = Arc::clone(self);
            handles.push(tokio::spawn(async move {
                loop {
                    notify.notified().await;
                    tracing::info!(upstream = %name, "prompt list changed, refreshing");
                    if let Err(e) = reg.refresh_upstream_prompts(&name).await {
                        tracing::warn!(upstream = %name, error = %e, "failed to refresh prompts");
                    } else {
                        reg.notify_downstream_prompt_change();
                    }
                }
            }));
        }

        RefreshGuard::from_handles(handles)
    }

    /// Merge all namespaced tools from all upstreams (unfiltered).
    pub async fn aggregated_tools(&self) -> Vec<McpTool> {
        let upstreams = self.upstreams.read().await;
        let mut all = Vec::new();
        for upstream in upstreams.values() {
            all.extend(upstream.namespaced_tools().await);
        }
        all
    }

    /// Route a namespaced tool call to the correct upstream.
    pub async fn route_call(
        &self,
        prefixed_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<McpToolCallResult, McpGatewayError> {
        let (server_name, tool_name) = parse_namespaced(prefixed_name)?;
        let upstreams = self.upstreams.read().await;
        let upstream = upstreams
            .get(server_name)
            .ok_or_else(|| McpGatewayError::ToolNotFound {
                name: prefixed_name.to_owned(),
            })?;
        upstream.call_tool(tool_name, arguments).await
    }

    /// Refresh the tool cache for a specific upstream.
    pub async fn refresh_upstream(&self, name: &str) -> Result<(), McpGatewayError> {
        let upstreams = self.upstreams.read().await;
        let upstream = upstreams
            .get(name)
            .ok_or_else(|| McpGatewayError::UpstreamClosed {
                name: name.to_owned(),
            })?;
        upstream.refresh_tools().await
    }

    /// Return notify handles for all upstreams, for spawning background listeners.
    pub async fn tool_change_notifiers(&self) -> Vec<(String, Arc<Notify>)> {
        let upstreams = self.upstreams.read().await;
        upstreams
            .iter()
            .map(|(name, conn)| (name.clone(), conn.tool_change_notify()))
            .collect()
    }

    /// Notify downstream MCP clients that the tool list has changed.
    ///
    /// Best-effort: does nothing if there are no active subscribers.
    pub fn notify_downstream_tool_change(&self) {
        let _ = self.tool_change_tx.send(());
    }

    /// Notify downstream MCP clients that the resource list has changed.
    pub fn notify_downstream_resource_change(&self) {
        let _ = self.resource_change_tx.send(());
    }

    /// Return resource-change notify handles for all upstreams.
    pub async fn resource_change_notifiers(&self) -> Vec<(String, Arc<Notify>)> {
        let upstreams = self.upstreams.read().await;
        upstreams
            .iter()
            .map(|(name, conn)| (name.clone(), conn.resource_change_notify()))
            .collect()
    }

    /// Notify downstream MCP clients that the prompt list has changed.
    pub fn notify_downstream_prompt_change(&self) {
        let _ = self.prompt_change_tx.send(());
    }

    /// Return prompt-change notify handles for all upstreams.
    pub async fn prompt_change_notifiers(&self) -> Vec<(String, Arc<Notify>)> {
        let upstreams = self.upstreams.read().await;
        upstreams
            .iter()
            .map(|(name, conn)| (name.clone(), conn.prompt_change_notify()))
            .collect()
    }

    /// Refresh the prompt cache for a specific upstream.
    pub async fn refresh_upstream_prompts(&self, name: &str) -> Result<(), McpGatewayError> {
        let upstreams = self.upstreams.read().await;
        let upstream = upstreams
            .get(name)
            .ok_or_else(|| McpGatewayError::UpstreamClosed {
                name: name.to_owned(),
            })?;
        upstream.refresh_prompts().await
    }

    /// Refresh the resource cache for a specific upstream.
    pub async fn refresh_upstream_resources(&self, name: &str) -> Result<(), McpGatewayError> {
        let upstreams = self.upstreams.read().await;
        let upstream = upstreams
            .get(name)
            .ok_or_else(|| McpGatewayError::UpstreamClosed {
                name: name.to_owned(),
            })?;
        upstream.refresh_resources().await
    }
}

// ── Core ToolRegistry impl ──────────────────────────────────────────

impl bitrouter_core::routers::registry::ToolRegistry for ConfigMcpRegistry {
    async fn list_tools(&self) -> Vec<bitrouter_core::routers::registry::ToolEntry> {
        self.aggregated_tools()
            .await
            .into_iter()
            .map(Into::into)
            .collect()
    }
}

/// Raw [`McpToolServer`] impl on `ConfigMcpRegistry`.
///
/// Delegates to [`ToolRegistry`] for list/call, keeps subscribe as
/// MCP-specific (broadcast channel).
impl McpToolServer for ConfigMcpRegistry {
    async fn list_tools(&self) -> Vec<McpTool> {
        self.aggregated_tools().await
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<McpToolCallResult, McpGatewayError> {
        self.route_call(name, arguments).await
    }

    fn subscribe_tool_changes(&self) -> broadcast::Receiver<()> {
        self.tool_change_tx.subscribe()
    }
}

/// [`McpResourceServer`] impl on raw `ConfigMcpRegistry`.
impl McpResourceServer for ConfigMcpRegistry {
    async fn list_resources(&self) -> Vec<McpResource> {
        let upstreams = self.upstreams.read().await;
        let mut all = Vec::new();
        for upstream in upstreams.values() {
            for r in upstream.namespaced_resources().await {
                all.push(McpResource {
                    uri: r.uri,
                    name: r.name,
                    description: r.description,
                    mime_type: r.mime_type,
                });
            }
        }
        all
    }

    async fn read_resource(&self, uri: &str) -> Result<Vec<McpResourceContent>, McpGatewayError> {
        let (server_name, original_uri) = parse_namespaced_uri(uri)?;
        let upstreams = self.upstreams.read().await;
        let upstream =
            upstreams
                .get(server_name)
                .ok_or_else(|| McpGatewayError::ResourceNotFound {
                    uri: uri.to_owned(),
                })?;
        upstream.read_resource(original_uri).await
    }

    async fn list_resource_templates(&self) -> Vec<McpResourceTemplate> {
        let upstreams = self.upstreams.read().await;
        let mut all = Vec::new();
        for upstream in upstreams.values() {
            for t in upstream.namespaced_resource_templates().await {
                all.push(McpResourceTemplate {
                    uri_template: t.uri_template,
                    name: t.name,
                    description: t.description,
                    mime_type: t.mime_type,
                });
            }
        }
        all
    }

    fn subscribe_resource_changes(&self) -> broadcast::Receiver<()> {
        self.resource_change_tx.subscribe()
    }
}

/// [`McpPromptServer`] impl on raw `ConfigMcpRegistry`.
impl McpPromptServer for ConfigMcpRegistry {
    async fn list_prompts(&self) -> Vec<McpPrompt> {
        let upstreams = self.upstreams.read().await;
        let mut all = Vec::new();
        for upstream in upstreams.values() {
            for p in upstream.namespaced_prompts().await {
                all.push(McpPrompt {
                    name: p.name,
                    description: p.description,
                    arguments: p.arguments,
                });
            }
        }
        all
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<std::collections::HashMap<String, String>>,
    ) -> Result<McpGetPromptResult, McpGatewayError> {
        let (server_name, prompt_name) = parse_namespaced(name)?;
        let upstreams = self.upstreams.read().await;
        let upstream =
            upstreams
                .get(server_name)
                .ok_or_else(|| McpGatewayError::PromptNotFound {
                    name: name.to_owned(),
                })?;
        upstream.get_prompt(prompt_name, arguments).await
    }

    fn subscribe_prompt_changes(&self) -> broadcast::Receiver<()> {
        self.prompt_change_tx.subscribe()
    }
}

/// [`McpSubscriptionServer`] impl on raw `ConfigMcpRegistry`.
///
/// Resource subscriptions are accepted but currently no-ops — the upstream
/// refresh listeners already detect list-level changes. Per-resource
/// granularity can be added later.
impl McpSubscriptionServer for ConfigMcpRegistry {
    async fn subscribe_resource(&self, _uri: &str) -> Result<(), McpGatewayError> {
        Ok(())
    }

    async fn unsubscribe_resource(&self, _uri: &str) -> Result<(), McpGatewayError> {
        Ok(())
    }
}

/// [`McpLoggingServer`] impl on raw `ConfigMcpRegistry`.
impl McpLoggingServer for ConfigMcpRegistry {
    async fn set_logging_level(&self, _level: LoggingLevel) -> Result<(), McpGatewayError> {
        Ok(())
    }
}

/// [`McpCompletionServer`] impl on raw `ConfigMcpRegistry`.
///
/// Returns empty completions — upstreams do not yet expose completion support.
impl McpCompletionServer for ConfigMcpRegistry {
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

/// Split a namespaced URI `server+scheme:///path` on the first `+`.
pub fn parse_namespaced_uri(uri: &str) -> Result<(&str, &str), McpGatewayError> {
    uri.split_once('+')
        .ok_or_else(|| McpGatewayError::ResourceNotFound {
            uri: uri.to_owned(),
        })
}

/// Split a namespaced name `server/item` on the first `/`.
pub fn parse_namespaced(name: &str) -> Result<(&str, &str), McpGatewayError> {
    name.split_once('/')
        .ok_or_else(|| McpGatewayError::ToolNotFound {
            name: name.to_owned(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_splits_on_first_slash() {
        let (server, tool) = parse_namespaced("myserver/mytool").expect("valid");
        assert_eq!(server, "myserver");
        assert_eq!(tool, "mytool");
    }

    #[test]
    fn parse_preserves_slashes_in_tool_name() {
        let (server, tool) = parse_namespaced("srv/path/to/tool").expect("valid");
        assert_eq!(server, "srv");
        assert_eq!(tool, "path/to/tool");
    }

    #[test]
    fn parse_errors_on_no_slash() {
        let result = parse_namespaced("noslash");
        assert!(result.is_err());
    }

    #[test]
    fn parse_uri_splits_on_first_plus() {
        let (server, uri) = parse_namespaced_uri("github+file:///readme.md").expect("valid");
        assert_eq!(server, "github");
        assert_eq!(uri, "file:///readme.md");
    }

    #[test]
    fn parse_uri_preserves_plus_in_original() {
        let (server, uri) = parse_namespaced_uri("srv+file:///path+extra").expect("valid");
        assert_eq!(server, "srv");
        assert_eq!(uri, "file:///path+extra");
    }

    #[test]
    fn parse_uri_errors_on_no_plus() {
        let result = parse_namespaced_uri("noplus");
        assert!(result.is_err());
    }
}
