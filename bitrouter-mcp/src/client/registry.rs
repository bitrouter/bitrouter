use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{CallToolResult, PromptMessageContent, ResourceContents, Tool};
use tokio::sync::{Notify, RwLock, broadcast};

use crate::admin::{AdminToolRegistry, ToolEntry, UpstreamInfo};
use crate::config::{McpServerConfig, ToolFilter};
use crate::error::McpGatewayError;
use crate::groups::McpAccessGroups;
use crate::param_filter::ParamRestrictions;
use crate::server::protocol::McpGetPromptResult;
use crate::server::types::{
    McpContent, McpPrompt, McpPromptArgument, McpPromptContent, McpPromptMessage, McpResource,
    McpResourceContent, McpResourceTemplate, McpRole, McpTool, McpToolCallResult,
};
use crate::server::{McpPromptServer, McpResourceServer, McpToolServer};

use super::upstream::UpstreamConnection;

/// Guard that aborts background refresh tasks on drop.
pub struct RefreshGuard {
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
    }
}

/// Aggregates multiple upstream MCP connections and routes tool calls.
///
/// The inner map is wrapped in [`RwLock`] to support runtime mutation of
/// filters without restarting the gateway.
pub struct UpstreamRegistry {
    upstreams: RwLock<HashMap<String, UpstreamConnection>>,
    groups: McpAccessGroups,
    /// Broadcast sender for notifying downstream MCP clients of tool list changes.
    tool_change_tx: broadcast::Sender<()>,
    /// Broadcast sender for notifying downstream MCP clients of resource list changes.
    resource_change_tx: broadcast::Sender<()>,
    /// Broadcast sender for notifying downstream MCP clients of prompt list changes.
    prompt_change_tx: broadcast::Sender<()>,
}

impl UpstreamRegistry {
    /// Connect to all configured upstreams. Fails on first error or duplicate name.
    pub async fn from_configs(
        configs: Vec<McpServerConfig>,
        groups: McpAccessGroups,
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

        let mut upstreams = HashMap::with_capacity(configs.len());
        for config in configs {
            let name = config.name.clone();
            tracing::info!(upstream = %name, "connecting to upstream");
            let conn = UpstreamConnection::connect(config).await?;
            upstreams.insert(name, conn);
        }

        let (tool_change_tx, _) = broadcast::channel(16);
        let (resource_change_tx, _) = broadcast::channel(16);
        let (prompt_change_tx, _) = broadcast::channel(16);
        Ok(Self {
            upstreams: RwLock::new(upstreams),
            groups,
            tool_change_tx,
            resource_change_tx,
            prompt_change_tx,
        })
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

        RefreshGuard { handles }
    }

    /// Return the access groups.
    pub fn groups(&self) -> &McpAccessGroups {
        &self.groups
    }

    /// Merge all namespaced tools from all upstreams.
    pub async fn aggregated_tools(&self) -> Vec<Tool> {
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
    ) -> Result<CallToolResult, McpGatewayError> {
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

    /// Update the tool filter for a running upstream.
    pub async fn update_filter(
        &self,
        server_name: &str,
        filter: Option<ToolFilter>,
    ) -> Result<(), McpGatewayError> {
        let upstreams = self.upstreams.read().await;
        let upstream = upstreams
            .get(server_name)
            .ok_or_else(|| McpGatewayError::ToolNotFound {
                name: server_name.to_owned(),
            })?;
        upstream.set_filter(filter).await;
        Ok(())
    }

    /// Update the parameter restrictions for a running upstream.
    pub async fn update_param_restrictions(
        &self,
        server_name: &str,
        restrictions: ParamRestrictions,
    ) -> Result<(), McpGatewayError> {
        let upstreams = self.upstreams.read().await;
        let upstream = upstreams
            .get(server_name)
            .ok_or_else(|| McpGatewayError::ToolNotFound {
                name: server_name.to_owned(),
            })?;
        upstream.set_param_restrictions(restrictions).await;
        Ok(())
    }

    /// List all upstream servers with their tool counts and current filters.
    pub async fn list_upstreams(&self) -> Vec<UpstreamInfo> {
        let upstreams = self.upstreams.read().await;
        let mut infos = Vec::with_capacity(upstreams.len());
        for (name, conn) in upstreams.iter() {
            let restrictions = conn.param_restrictions().await;
            let has_restrictions = !restrictions.rules.is_empty();
            infos.push(UpstreamInfo {
                name: name.clone(),
                tool_count: conn.tool_count().await,
                filter: conn.filter().await,
                param_restrictions: if has_restrictions {
                    Some(restrictions)
                } else {
                    None
                },
            });
        }
        infos
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

/// Convert an `rmcp` [`Tool`] into an `rmcp`-free [`McpTool`].
fn rmcp_tool_to_mcp_tool(tool: &Tool) -> McpTool {
    McpTool {
        name: tool.name.to_string(),
        description: tool.description.as_deref().map(str::to_owned),
        input_schema: serde_json::to_value(&*tool.input_schema).unwrap_or_default(),
    }
}

/// Convert an `rmcp` [`CallToolResult`] into an `rmcp`-free [`McpToolCallResult`].
fn rmcp_result_to_mcp_result(result: &CallToolResult) -> McpToolCallResult {
    // Serialize the rmcp content to JSON, then extract text fields.
    // This handles all content types generically.
    let content: Vec<McpContent> = result
        .content
        .iter()
        .filter_map(|c| {
            let value = serde_json::to_value(c).ok()?;
            let text = value.get("text")?.as_str()?;
            Some(McpContent::Text {
                text: text.to_owned(),
            })
        })
        .collect();

    McpToolCallResult {
        content,
        is_error: result.is_error,
    }
}

/// Implement [`McpToolServer`] for the runtime registry.
impl McpToolServer for UpstreamRegistry {
    async fn list_tools(&self) -> Vec<McpTool> {
        let rmcp_tools = self.aggregated_tools().await;
        rmcp_tools.iter().map(rmcp_tool_to_mcp_tool).collect()
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<McpToolCallResult, McpGatewayError> {
        let result = self.route_call(name, arguments).await?;
        Ok(rmcp_result_to_mcp_result(&result))
    }

    fn subscribe_tool_changes(&self) -> broadcast::Receiver<()> {
        self.tool_change_tx.subscribe()
    }
}

/// Implement [`AdminToolRegistry`] for the runtime registry.
impl AdminToolRegistry for UpstreamRegistry {
    async fn list_tools(&self) -> Vec<ToolEntry> {
        let tools = self.aggregated_tools().await;
        tools
            .into_iter()
            .map(|t| {
                let (server, _tool) = t
                    .name
                    .split_once('/')
                    .unwrap_or(("unknown", t.name.as_ref()));
                ToolEntry {
                    name: t.name.to_string(),
                    server: server.to_owned(),
                    description: t.description.as_deref().unwrap_or_default().to_owned(),
                    source: "config",
                }
            })
            .collect()
    }

    async fn list_upstreams(&self) -> Vec<UpstreamInfo> {
        self.list_upstreams().await
    }

    async fn update_filter(
        &self,
        server: &str,
        filter: Option<ToolFilter>,
    ) -> Result<(), McpGatewayError> {
        self.update_filter(server, filter).await
    }

    async fn list_groups(&self) -> HashMap<String, Vec<String>> {
        self.groups().as_map().clone()
    }

    async fn update_param_restrictions(
        &self,
        server: &str,
        restrictions: ParamRestrictions,
    ) -> Result<(), McpGatewayError> {
        self.update_param_restrictions(server, restrictions).await
    }
}

/// Convert an `rmcp` [`ResourceContents`] into an `rmcp`-free [`McpResourceContent`].
fn rmcp_resource_contents_to_mcp(rc: &ResourceContents) -> McpResourceContent {
    match rc {
        ResourceContents::TextResourceContents {
            uri,
            mime_type,
            text,
            ..
        } => McpResourceContent {
            uri: uri.clone(),
            mime_type: mime_type.clone(),
            text: Some(text.clone()),
            blob: None,
        },
        ResourceContents::BlobResourceContents {
            uri,
            mime_type,
            blob,
            ..
        } => McpResourceContent {
            uri: uri.clone(),
            mime_type: mime_type.clone(),
            text: None,
            blob: Some(blob.clone()),
        },
    }
}

/// Implement [`McpResourceServer`] for the runtime registry.
impl McpResourceServer for UpstreamRegistry {
    async fn list_resources(&self) -> Vec<McpResource> {
        let upstreams = self.upstreams.read().await;
        let mut all = Vec::new();
        for upstream in upstreams.values() {
            for (uri, name, description, mime_type) in upstream.namespaced_resources().await {
                all.push(McpResource {
                    uri,
                    name,
                    description,
                    mime_type,
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
        let result = upstream.read_resource(original_uri).await?;
        Ok(result
            .contents
            .iter()
            .map(rmcp_resource_contents_to_mcp)
            .collect())
    }

    async fn list_resource_templates(&self) -> Vec<McpResourceTemplate> {
        let upstreams = self.upstreams.read().await;
        let mut all = Vec::new();
        for upstream in upstreams.values() {
            for (uri_template, name, description, mime_type) in
                upstream.namespaced_resource_templates().await
            {
                all.push(McpResourceTemplate {
                    uri_template,
                    name,
                    description,
                    mime_type,
                });
            }
        }
        all
    }

    fn subscribe_resource_changes(&self) -> broadcast::Receiver<()> {
        self.resource_change_tx.subscribe()
    }
}

/// Convert an `rmcp` [`PromptMessageContent`] into an `rmcp`-free [`McpPromptContent`].
fn rmcp_prompt_content_to_mcp(content: &PromptMessageContent) -> McpPromptContent {
    match content {
        PromptMessageContent::Text { text } => McpPromptContent::Text { text: text.clone() },
        PromptMessageContent::Resource { resource } => {
            let rc = &resource.resource;
            McpPromptContent::Resource {
                resource: rmcp_resource_contents_to_mcp(rc),
            }
        }
        // For image and resource_link types, serialize to text as a fallback.
        other => {
            let text = serde_json::to_string(other).unwrap_or_default();
            McpPromptContent::Text { text }
        }
    }
}

/// Implement [`McpPromptServer`] for the runtime registry.
impl McpPromptServer for UpstreamRegistry {
    async fn list_prompts(&self) -> Vec<McpPrompt> {
        let upstreams = self.upstreams.read().await;
        let mut all = Vec::new();
        for upstream in upstreams.values() {
            for (name, description, args) in upstream.namespaced_prompts().await {
                let arguments = args
                    .into_iter()
                    .map(|a| McpPromptArgument {
                        name: a.name,
                        description: a.description,
                        required: a.required,
                    })
                    .collect();
                all.push(McpPrompt {
                    name,
                    description,
                    arguments,
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
        let result = upstream.get_prompt(prompt_name, arguments).await?;
        let messages = result
            .messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    rmcp::model::PromptMessageRole::User => McpRole::User,
                    rmcp::model::PromptMessageRole::Assistant => McpRole::Assistant,
                };
                McpPromptMessage {
                    role,
                    content: rmcp_prompt_content_to_mcp(&m.content),
                }
            })
            .collect();
        Ok(McpGetPromptResult {
            description: result.description,
            messages,
        })
    }

    fn subscribe_prompt_changes(&self) -> broadcast::Receiver<()> {
        self.prompt_change_tx.subscribe()
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
