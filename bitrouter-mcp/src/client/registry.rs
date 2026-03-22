use std::collections::HashMap;
use std::sync::Arc;

use bitrouter_core::routers::dynamic_tool::DynamicToolRegistry;
use rmcp::model::{CallToolResult, PromptMessageContent, ResourceContents, Tool};
use tokio::sync::{Notify, RwLock, broadcast};

use crate::config::McpServerConfig;
use crate::error::McpGatewayError;
use crate::groups::McpAccessGroups;
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
/// This is the raw tool source — it provides unfiltered tools from all
/// upstreams. Filter and restriction management is handled by the
/// [`DynamicToolRegistry`] wrapper that wraps this registry.
pub struct ConfigMcpRegistry {
    upstreams: RwLock<HashMap<String, UpstreamConnection>>,
    groups: McpAccessGroups,
    /// Broadcast sender for notifying downstream MCP clients of tool list changes.
    tool_change_tx: broadcast::Sender<()>,
    /// Broadcast sender for notifying downstream MCP clients of resource list changes.
    resource_change_tx: broadcast::Sender<()>,
    /// Broadcast sender for notifying downstream MCP clients of prompt list changes.
    prompt_change_tx: broadcast::Sender<()>,
}

impl ConfigMcpRegistry {
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

    /// Merge all namespaced tools from all upstreams (unfiltered).
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

/// Implement core [`ToolRegistry`](bitrouter_core::routers::registry::ToolRegistry)
/// for public discovery (`GET /v1/tools`).
///
/// Returns all tools from all upstreams, unfiltered. Filtering is applied
/// by the [`DynamicToolRegistry`] wrapper.
impl bitrouter_core::routers::registry::ToolRegistry for ConfigMcpRegistry {
    async fn list_tools(&self) -> Vec<bitrouter_core::routers::registry::ToolEntry> {
        McpToolServer::list_tools(self)
            .await
            .into_iter()
            .map(Into::into)
            .collect()
    }
}

// ── McpToolServer impl (raw, on ConfigMcpRegistry) ──────────────────

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

/// `McpToolServer` for `Arc<ConfigMcpRegistry>` — delegates to inner.
impl McpToolServer for Arc<ConfigMcpRegistry> {
    async fn list_tools(&self) -> Vec<McpTool> {
        <ConfigMcpRegistry as McpToolServer>::list_tools(self).await
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<McpToolCallResult, McpGatewayError> {
        <ConfigMcpRegistry as McpToolServer>::call_tool(self, name, arguments).await
    }

    fn subscribe_tool_changes(&self) -> broadcast::Receiver<()> {
        <ConfigMcpRegistry as McpToolServer>::subscribe_tool_changes(self)
    }
}

/// `McpResourceServer` for `Arc<ConfigMcpRegistry>` — delegates to inner.
impl McpResourceServer for Arc<ConfigMcpRegistry> {
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

/// `McpPromptServer` for `Arc<ConfigMcpRegistry>` — delegates to inner.
impl McpPromptServer for Arc<ConfigMcpRegistry> {
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

/// Raw [`McpToolServer`] impl on `ConfigMcpRegistry` — returns all tools
/// unfiltered and calls without restriction enforcement.
impl McpToolServer for ConfigMcpRegistry {
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

// ── Protocol trait impls on DynamicToolRegistry<ConfigMcpRegistry> ───
//
// These impls live here (in the MCP crate) because the protocol traits
// (`McpToolServer`, `McpResourceServer`, `McpPromptServer`) are defined
// in this crate. The `DynamicToolRegistry` type is from core. Orphan
// rules are satisfied because the traits are local.

/// [`McpToolServer`] for the wrapped registry — applies param restrictions
/// at call time and delegates tool listing through the filtered wrapper.
impl McpToolServer for DynamicToolRegistry<Arc<ConfigMcpRegistry>> {
    async fn list_tools(&self) -> Vec<McpTool> {
        // Use the filtered core ToolRegistry output, then convert back to McpTool.
        let core_tools =
            <Self as bitrouter_core::routers::registry::ToolRegistry>::list_tools(self).await;
        core_tools
            .into_iter()
            .map(|t| McpTool {
                name: t.id,
                description: t.description,
                input_schema: t.input_schema.unwrap_or_default(),
            })
            .collect()
    }

    async fn call_tool(
        &self,
        name: &str,
        mut arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<McpToolCallResult, McpGatewayError> {
        // Extract server name from namespaced tool call.
        let (server_name, tool_name) = parse_namespaced(name)?;

        // Enforce parameter restrictions from the wrapper's state.
        if let Some(restrictions) = self.get_param_restrictions(server_name) {
            restrictions
                .check(tool_name, &mut arguments)
                .map_err(|e| match e {
                    bitrouter_core::errors::BitrouterError::InvalidRequest { message, .. } => {
                        McpGatewayError::ParamDenied {
                            tool: name.to_owned(),
                            param: message,
                        }
                    }
                    other => McpGatewayError::UpstreamCall {
                        name: name.to_owned(),
                        reason: other.to_string(),
                    },
                })?;
        }

        // Delegate actual call to inner ConfigMcpRegistry.
        self.inner().call_tool(name, arguments).await
    }

    fn subscribe_tool_changes(&self) -> broadcast::Receiver<()> {
        self.inner().subscribe_tool_changes()
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

/// [`McpResourceServer`] — delegates to inner `ConfigMcpRegistry`.
impl McpResourceServer for DynamicToolRegistry<Arc<ConfigMcpRegistry>> {
    async fn list_resources(&self) -> Vec<McpResource> {
        <ConfigMcpRegistry as McpResourceServer>::list_resources(self.inner()).await
    }

    async fn read_resource(&self, uri: &str) -> Result<Vec<McpResourceContent>, McpGatewayError> {
        <ConfigMcpRegistry as McpResourceServer>::read_resource(self.inner(), uri).await
    }

    async fn list_resource_templates(&self) -> Vec<McpResourceTemplate> {
        <ConfigMcpRegistry as McpResourceServer>::list_resource_templates(self.inner()).await
    }

    fn subscribe_resource_changes(&self) -> broadcast::Receiver<()> {
        <ConfigMcpRegistry as McpResourceServer>::subscribe_resource_changes(self.inner())
    }
}

/// [`McpResourceServer`] impl on raw `ConfigMcpRegistry`.
impl McpResourceServer for ConfigMcpRegistry {
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

/// [`McpPromptServer`] — delegates to inner `ConfigMcpRegistry`.
impl McpPromptServer for DynamicToolRegistry<Arc<ConfigMcpRegistry>> {
    async fn list_prompts(&self) -> Vec<McpPrompt> {
        <ConfigMcpRegistry as McpPromptServer>::list_prompts(self.inner()).await
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<std::collections::HashMap<String, String>>,
    ) -> Result<McpGetPromptResult, McpGatewayError> {
        <ConfigMcpRegistry as McpPromptServer>::get_prompt(self.inner(), name, arguments).await
    }

    fn subscribe_prompt_changes(&self) -> broadcast::Receiver<()> {
        <ConfigMcpRegistry as McpPromptServer>::subscribe_prompt_changes(self.inner())
    }
}

/// [`McpPromptServer`] impl on raw `ConfigMcpRegistry`.
impl McpPromptServer for ConfigMcpRegistry {
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
