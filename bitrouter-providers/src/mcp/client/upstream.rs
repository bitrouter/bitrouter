//! Transport-agnostic upstream MCP connection.
//!
//! Uses rmcp's client runtime for protocol handling. The [`ConnectedPeer`]
//! wrapper provides a type-erased interface to the rmcp `Peer<RoleClient>`.

use std::sync::Arc;

use rmcp::service::ServiceExt;
use tokio::sync::{Notify, RwLock};

use super::config::{McpServerConfig, McpServerTransport};
use bitrouter_core::errors::{BitrouterError, Result as BResult};
use bitrouter_core::tools::provider::ToolProvider;
use bitrouter_core::tools::result::{ToolCallResult, ToolContent};

use bitrouter_core::api::mcp::gateway::McpClientRequestHandler;
use bitrouter_core::api::mcp::types::McpGatewayError;
use bitrouter_core::api::mcp::types::{
    McpContent, McpGetPromptResult, McpPrompt, McpPromptArgument, McpResource, McpResourceContent,
    McpResourceTemplate, McpTool, McpToolCallResult,
};

use super::convert;
use super::transport::{
    BitrouterClientHandler, ConnectedPeer, NotifyHandles, build_http_transport,
};

/// A namespaced resource from an upstream, with its URI prefixed by server name.
pub struct NamespacedResource {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

/// A namespaced resource template from an upstream, with its URI template prefixed.
pub struct NamespacedResourceTemplate {
    pub uri_template: String,
    pub name: String,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

/// A namespaced prompt from an upstream, with its name prefixed by server name.
pub struct NamespacedPrompt {
    pub name: String,
    pub description: Option<String>,
    pub arguments: Vec<McpPromptArgument>,
}

/// A live connection to a single upstream MCP server.
///
/// Stores cached tool, resource, and prompt lists. Filter and parameter
/// restriction state is managed externally by [`DynamicToolRegistry`].
pub struct UpstreamConnection {
    name: String,
    peer: ConnectedPeer,
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
    ///
    /// If a `handler` is provided, the connection will handle server→client
    /// requests (sampling, elicitation) by dispatching to it.
    pub async fn connect(
        config: McpServerConfig,
        handler: Option<Arc<dyn McpClientRequestHandler>>,
    ) -> Result<Self, McpGatewayError> {
        config
            .validate()
            .map_err(|reason| McpGatewayError::InvalidConfig { reason })?;

        // Server names must not contain "__" because the MCP gateway uses it
        // as the wire-format separator between server and tool names.
        if config.name.contains("__") {
            return Err(McpGatewayError::InvalidConfig {
                reason: format!(
                    "server name '{}' must not contain '__' (reserved as wire-format separator)",
                    config.name
                ),
            });
        }

        let name = config.name.clone();
        let tool_notify = Arc::new(Notify::new());
        let resource_notify = Arc::new(Notify::new());
        let prompt_notify = Arc::new(Notify::new());

        let bridge = BitrouterClientHandler::new(
            name.clone(),
            NotifyHandles {
                tool: Arc::clone(&tool_notify),
                resource: Arc::clone(&resource_notify),
                prompt: Arc::clone(&prompt_notify),
            },
            handler,
        );

        let peer = match config.transport {
            McpServerTransport::Http {
                ref url,
                ref headers,
            } => {
                let transport = build_http_transport(url, headers, &name)?;
                let service = bridge.serve(transport).await.map_err(
                    |e: rmcp::service::ClientInitializeError| McpGatewayError::UpstreamConnect {
                        name: name.clone(),
                        reason: e.to_string(),
                    },
                )?;
                ConnectedPeer::from_service(service)
            }
            McpServerTransport::Stdio {
                ref command,
                ref args,
            } => {
                let mut cmd = tokio::process::Command::new(command);
                cmd.args(args);
                let transport = rmcp::transport::TokioChildProcess::new(cmd).map_err(|e| {
                    McpGatewayError::UpstreamConnect {
                        name: name.clone(),
                        reason: format!("failed to spawn stdio process: {e}"),
                    }
                })?;
                let service = bridge.serve(transport).await.map_err(
                    |e: rmcp::service::ClientInitializeError| McpGatewayError::UpstreamConnect {
                        name: name.clone(),
                        reason: e.to_string(),
                    },
                )?;
                ConnectedPeer::from_service(service)
            }
        };

        let initial_tools: Vec<McpTool> = peer
            .peer()
            .list_all_tools()
            .await
            .map_err(|e| McpGatewayError::UpstreamConnect {
                name: name.clone(),
                reason: format!("failed to list tools: {e}"),
            })?
            .into_iter()
            .map(convert::tool)
            .collect();

        // Best-effort: fetch resources and prompts concurrently.
        let (resources_result, templates_result, prompts_result) = tokio::join!(
            peer.peer().list_all_resources(),
            peer.peer().list_all_resource_templates(),
            peer.peer().list_all_prompts(),
        );

        let initial_resources: Vec<McpResource> = resources_result
            .unwrap_or_default()
            .into_iter()
            .map(convert::resource)
            .collect();

        let initial_templates: Vec<McpResourceTemplate> = templates_result
            .unwrap_or_default()
            .into_iter()
            .map(convert::resource_template)
            .collect();

        let initial_prompts: Vec<McpPrompt> = prompts_result
            .unwrap_or_default()
            .into_iter()
            .map(convert::prompt)
            .collect();

        Ok(Self {
            name: config.name,
            peer,
            tools: Arc::new(RwLock::new(initial_tools)),
            resources: Arc::new(RwLock::new(initial_resources)),
            resource_templates: Arc::new(RwLock::new(initial_templates)),
            prompts: Arc::new(RwLock::new(initial_prompts)),
            tool_notify,
            resource_notify,
            prompt_notify,
        })
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
        let fresh: Vec<McpTool> = self
            .peer
            .peer()
            .list_all_tools()
            .await
            .map_err(|e| convert::service_error(&self.name, e))?
            .into_iter()
            .map(convert::tool)
            .collect();
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
        let params = rmcp::model::CallToolRequestParams::new(tool_name.to_owned())
            .with_arguments(arguments.unwrap_or_default());
        let result = self
            .peer
            .peer()
            .call_tool(params)
            .await
            .map_err(|e| convert::service_error(&self.name, e))?;
        Ok(convert::call_tool_result(result))
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
    pub async fn namespaced_resources(&self) -> Vec<NamespacedResource> {
        let resources = self.resources.read().await;
        resources
            .iter()
            .map(|r| NamespacedResource {
                uri: format!("{}+{}", self.name, r.uri),
                name: r.name.clone(),
                description: r.description.clone(),
                mime_type: r.mime_type.clone(),
            })
            .collect()
    }

    /// Return all resource templates from this upstream, namespaced as `{name}+{uri_template}`.
    pub async fn namespaced_resource_templates(&self) -> Vec<NamespacedResourceTemplate> {
        let templates = self.resource_templates.read().await;
        templates
            .iter()
            .map(|t| NamespacedResourceTemplate {
                uri_template: format!("{}+{}", self.name, t.uri_template),
                name: t.name.clone(),
                description: t.description.clone(),
                mime_type: t.mime_type.clone(),
            })
            .collect()
    }

    /// Read a resource from this upstream using its original (un-prefixed) URI.
    pub async fn read_resource(
        &self,
        uri: &str,
    ) -> Result<Vec<McpResourceContent>, McpGatewayError> {
        let params = rmcp::model::ReadResourceRequestParams::new(uri);
        let result = self
            .peer
            .peer()
            .read_resource(params)
            .await
            .map_err(|e| convert::service_error(&self.name, e))?;
        Ok(result
            .contents
            .into_iter()
            .map(convert::resource_contents)
            .collect())
    }

    /// Re-fetch resources and templates from the upstream and update the caches.
    pub async fn refresh_resources(&self) -> Result<(), McpGatewayError> {
        let fresh_resources: Vec<McpResource> = self
            .peer
            .peer()
            .list_all_resources()
            .await
            .map_err(|e| convert::service_error(&self.name, e))?
            .into_iter()
            .map(convert::resource)
            .collect();
        let fresh_templates: Vec<McpResourceTemplate> = self
            .peer
            .peer()
            .list_all_resource_templates()
            .await
            .map_err(|e| convert::service_error(&self.name, e))?
            .into_iter()
            .map(convert::resource_template)
            .collect();
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
    pub async fn namespaced_prompts(&self) -> Vec<NamespacedPrompt> {
        let prompts = self.prompts.read().await;
        prompts
            .iter()
            .map(|p| NamespacedPrompt {
                name: format!("{}/{}", self.name, p.name),
                description: p.description.clone(),
                arguments: p.arguments.clone(),
            })
            .collect()
    }

    /// Get a prompt from this upstream using the original (un-prefixed) name.
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<std::collections::HashMap<String, String>>,
    ) -> Result<McpGetPromptResult, McpGatewayError> {
        let mut params = rmcp::model::GetPromptRequestParams::new(name);
        if let Some(args) = arguments {
            let json_obj: serde_json::Map<String, serde_json::Value> = args
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect();
            params.arguments = Some(json_obj);
        }
        let result = self
            .peer
            .peer()
            .get_prompt(params)
            .await
            .map_err(|e| convert::service_error(&self.name, e))?;
        Ok(convert::get_prompt_result(result))
    }

    /// Re-fetch prompts from the upstream and update the cache.
    pub async fn refresh_prompts(&self) -> Result<(), McpGatewayError> {
        let fresh: Vec<McpPrompt> = self
            .peer
            .peer()
            .list_all_prompts()
            .await
            .map_err(|e| convert::service_error(&self.name, e))?
            .into_iter()
            .map(convert::prompt)
            .collect();
        let mut cache = self.prompts.write().await;
        *cache = fresh;
        Ok(())
    }
}

// ── ToolProvider impl ──────────────────────────────────────────────

impl ToolProvider for UpstreamConnection {
    fn provider_name(&self) -> &str {
        &self.name
    }

    async fn call_tool(
        &self,
        tool_id: &str,
        arguments: serde_json::Value,
    ) -> BResult<ToolCallResult> {
        let args = match arguments {
            serde_json::Value::Object(map) => Some(map),
            serde_json::Value::Null => None,
            other => {
                return Err(BitrouterError::invalid_request(
                    Some(&self.name),
                    format!("tool arguments must be a JSON object, got {}", other),
                    None,
                ));
            }
        };

        let mcp_result = self
            .call_tool(tool_id, args)
            .await
            .map_err(|e| BitrouterError::transport(Some(&self.name), e.to_string()))?;

        Ok(mcp_result_to_tool_result(mcp_result))
    }
}

fn mcp_result_to_tool_result(mcp: McpToolCallResult) -> ToolCallResult {
    let content = mcp
        .content
        .into_iter()
        .map(|c| match c {
            McpContent::Text { text } => ToolContent::Text { text },
        })
        .collect();

    ToolCallResult {
        content,
        is_error: mcp.is_error.unwrap_or(false),
        metadata: None,
    }
}
