//! Transport-agnostic upstream MCP connection.
//!
//! Uses [`McpTransport`](crate::mcp::transports::McpTransport) implementations
//! via [`TransportKind`](crate::mcp::transports::TransportKind) for static dispatch.

use std::sync::Arc;

use tokio::sync::{Notify, RwLock};

use bitrouter_core::routers::upstream::{ToolServerConfig, ToolServerTransport};

use crate::mcp::transports::McpTransport;
use crate::mcp::transports::TransportKind;
use bitrouter_core::api::mcp::error::McpGatewayError;
use bitrouter_core::api::mcp::types::{
    McpGetPromptResult, McpPrompt, McpPromptArgument, McpResource, McpResourceContent,
    McpResourceTemplate, McpTool, McpToolCallResult,
};

/// A live connection to a single upstream MCP server.
///
/// Stores cached tool, resource, and prompt lists. Filter and parameter
/// restriction state is managed externally by [`DynamicToolRegistry`].
pub struct UpstreamConnection {
    name: String,
    transport: TransportKind,
    tools: Arc<RwLock<Vec<McpTool>>>,
    resources: Arc<RwLock<Vec<McpResource>>>,
    resource_templates: Arc<RwLock<Vec<McpResourceTemplate>>>,
    prompts: Arc<RwLock<Vec<McpPrompt>>>,
    tool_notify: Arc<Notify>,
    resource_notify: Arc<Notify>,
    prompt_notify: Arc<Notify>,
}

impl UpstreamConnection {
    /// Connect to an upstream MCP server.
    pub async fn connect(config: ToolServerConfig) -> Result<Self, McpGatewayError> {
        config
            .validate()
            .map_err(|reason| McpGatewayError::InvalidConfig { reason })?;

        let name = config.name.clone();
        let tool_notify = Arc::new(Notify::new());
        let resource_notify = Arc::new(Notify::new());
        let prompt_notify = Arc::new(Notify::new());

        match config.transport {
            ToolServerTransport::Http {
                ref url,
                ref headers,
            } => {
                let client = crate::mcp::transports::http::McpHttpClient::new(
                    name.clone(),
                    url.clone(),
                    headers,
                )?;
                client
                    .initialize()
                    .await
                    .map_err(|e| McpGatewayError::UpstreamConnect {
                        name: name.clone(),
                        reason: e.to_string(),
                    })?;

                let initial_tools =
                    client
                        .list_tools()
                        .await
                        .map_err(|e| McpGatewayError::UpstreamConnect {
                            name: name.clone(),
                            reason: format!("failed to list tools: {e}"),
                        })?;

                // Best-effort: fetch resources and prompts if the upstream supports them.
                let initial_resources = client.list_resources().await.unwrap_or_default();
                let initial_templates = client.list_resource_templates().await.unwrap_or_default();
                let initial_prompts = client.list_prompts().await.unwrap_or_default();

                Ok(Self {
                    name: config.name,
                    transport: TransportKind::Http(client),
                    tools: Arc::new(RwLock::new(initial_tools)),
                    resources: Arc::new(RwLock::new(initial_resources)),
                    resource_templates: Arc::new(RwLock::new(initial_templates)),
                    prompts: Arc::new(RwLock::new(initial_prompts)),
                    tool_notify,
                    resource_notify,
                    prompt_notify,
                })
            }
            #[cfg(feature = "mcp-stdio")]
            ToolServerTransport::Stdio {
                ref command,
                ref args,
                ref env,
            } => {
                let conn = crate::mcp::transports::stdio::StdioConnection::connect(
                    name.clone(),
                    command.clone(),
                    args.clone(),
                    env.clone(),
                )
                .await?;

                let initial_tools =
                    conn.list_tools()
                        .await
                        .map_err(|e| McpGatewayError::UpstreamConnect {
                            name: name.clone(),
                            reason: format!("failed to list tools: {e}"),
                        })?;

                let initial_resources = conn.list_resources().await.unwrap_or_default();
                let initial_templates = conn.list_resource_templates().await.unwrap_or_default();
                let initial_prompts = conn.list_prompts().await.unwrap_or_default();

                let tn = conn.tool_change_notify();
                let rn = conn.resource_change_notify();
                let pn = conn.prompt_change_notify();

                Ok(Self {
                    name: config.name,
                    transport: TransportKind::Stdio(conn),
                    tools: Arc::new(RwLock::new(initial_tools)),
                    resources: Arc::new(RwLock::new(initial_resources)),
                    resource_templates: Arc::new(RwLock::new(initial_templates)),
                    prompts: Arc::new(RwLock::new(initial_prompts)),
                    tool_notify: tn,
                    resource_notify: rn,
                    prompt_notify: pn,
                })
            }
            #[cfg(not(feature = "mcp-stdio"))]
            ToolServerTransport::Stdio { .. } => Err(McpGatewayError::InvalidConfig {
                reason: format!(
                    "server '{}': stdio transport requires the 'mcp-stdio' feature",
                    config.name
                ),
            }),
        }
    }

    /// Return all tools with their original names (no server prefix).
    pub async fn raw_tools(&self) -> Vec<McpTool> {
        self.tools.read().await.clone()
    }

    /// Return all resources with their original URIs (no server prefix).
    pub async fn raw_resources(&self) -> Vec<McpResource> {
        self.resources.read().await.clone()
    }

    /// Return all resource templates with their original URI templates (no prefix).
    pub async fn raw_resource_templates(&self) -> Vec<McpResourceTemplate> {
        self.resource_templates.read().await.clone()
    }

    /// Return all prompts with their original names (no server prefix).
    pub async fn raw_prompts(&self) -> Vec<McpPrompt> {
        self.prompts.read().await.clone()
    }

    /// Return all tools from this upstream, namespaced as `{name}/{tool_name}`.
    pub async fn namespaced_tools(&self) -> Vec<McpTool> {
        let tools = self.tools.read().await;
        tools
            .iter()
            .map(|t| McpTool {
                name: format!("{}/{}", self.name, t.name),
                description: t.description.clone(),
                input_schema: t.input_schema.clone(),
            })
            .collect()
    }

    /// Re-fetch the tool list from the upstream and update the cache.
    pub async fn refresh_tools(&self) -> Result<(), McpGatewayError> {
        let fresh = self.transport.list_tools().await?;
        let mut cache = self.tools.write().await;
        *cache = fresh;
        Ok(())
    }

    /// Forward a tool call to this upstream using the original (un-prefixed) tool name.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<McpToolCallResult, McpGatewayError> {
        self.transport.call_tool(tool_name, arguments).await
    }

    /// Return the total number of tools on this upstream (unfiltered).
    pub async fn tool_count(&self) -> usize {
        self.tools.read().await.len()
    }

    /// Expose the tool-change notify handle for spawning background refresh tasks.
    pub fn tool_change_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.tool_notify)
    }

    /// Expose the resource-change notify handle for spawning background refresh tasks.
    pub fn resource_change_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.resource_notify)
    }

    /// Expose the prompt-change notify handle for spawning background refresh tasks.
    pub fn prompt_change_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.prompt_notify)
    }

    // ── Resource methods ────────────────────────────────────────────

    /// Return all resources from this upstream, namespaced as `{name}+{uri}`.
    pub async fn namespaced_resources(
        &self,
    ) -> Vec<(String, String, Option<String>, Option<String>)> {
        let resources = self.resources.read().await;
        resources
            .iter()
            .map(|r| {
                let prefixed_uri = format!("{}+{}", self.name, r.uri);
                (
                    prefixed_uri,
                    r.name.clone(),
                    r.description.clone(),
                    r.mime_type.clone(),
                )
            })
            .collect()
    }

    /// Return all resource templates from this upstream, namespaced as `{name}+{uri_template}`.
    pub async fn namespaced_resource_templates(
        &self,
    ) -> Vec<(String, String, Option<String>, Option<String>)> {
        let templates = self.resource_templates.read().await;
        templates
            .iter()
            .map(|t| {
                let prefixed = format!("{}+{}", self.name, t.uri_template);
                (
                    prefixed,
                    t.name.clone(),
                    t.description.clone(),
                    t.mime_type.clone(),
                )
            })
            .collect()
    }

    /// Read a resource from this upstream using its original (un-prefixed) URI.
    pub async fn read_resource(
        &self,
        uri: &str,
    ) -> Result<Vec<McpResourceContent>, McpGatewayError> {
        self.transport.read_resource(uri).await
    }

    /// Re-fetch resources and templates from the upstream and update the caches.
    pub async fn refresh_resources(&self) -> Result<(), McpGatewayError> {
        let fresh_resources = self.transport.list_resources().await?;
        let fresh_templates = self.transport.list_resource_templates().await?;
        {
            let mut cache = self.resources.write().await;
            *cache = fresh_resources;
        }
        {
            let mut cache = self.resource_templates.write().await;
            *cache = fresh_templates;
        }
        Ok(())
    }

    // ── Prompt methods ──────────────────────────────────────────────

    /// Return all prompts from this upstream, namespaced as `{name}/{prompt_name}`.
    pub async fn namespaced_prompts(
        &self,
    ) -> Vec<(String, Option<String>, Vec<McpPromptArgument>)> {
        let prompts = self.prompts.read().await;
        prompts
            .iter()
            .map(|p| {
                let prefixed_name = format!("{}/{}", self.name, p.name);
                (prefixed_name, p.description.clone(), p.arguments.clone())
            })
            .collect()
    }

    /// Get a prompt from this upstream using the original (un-prefixed) name.
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<std::collections::HashMap<String, String>>,
    ) -> Result<McpGetPromptResult, McpGatewayError> {
        self.transport.get_prompt(name, arguments).await
    }

    /// Re-fetch prompts from the upstream and update the cache.
    pub async fn refresh_prompts(&self) -> Result<(), McpGatewayError> {
        let fresh = self.transport.list_prompts().await?;
        let mut cache = self.prompts.write().await;
        *cache = fresh;
        Ok(())
    }
}
