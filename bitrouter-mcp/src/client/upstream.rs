use std::borrow::Cow;
use std::sync::Arc;

use rmcp::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientInfo, GetPromptRequestParams, GetPromptResult,
    Prompt, ReadResourceRequestParams, ReadResourceResult, Resource, ResourceTemplate, Tool,
};
use rmcp::service::{RoleClient, RunningService, ServiceExt as _};
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use tokio::sync::Notify;

use crate::config::{McpServerConfig, McpTransport, ToolFilter};
use crate::error::McpGatewayError;
use crate::param_filter::ParamRestrictions;

/// Handler that receives notifications from an upstream MCP server.
struct UpstreamNotificationHandler {
    tool_notify: Arc<Notify>,
    resource_notify: Arc<Notify>,
    prompt_notify: Arc<Notify>,
}

impl ClientHandler for UpstreamNotificationHandler {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }

    fn on_tool_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.tool_notify.notify_one();
        std::future::ready(())
    }

    fn on_resource_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.resource_notify.notify_one();
        std::future::ready(())
    }

    fn on_prompt_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.prompt_notify.notify_one();
        std::future::ready(())
    }
}

/// A live connection to a single upstream MCP server.
pub struct UpstreamConnection {
    name: String,
    service: RunningService<RoleClient, UpstreamNotificationHandler>,
    tools: Arc<tokio::sync::RwLock<Vec<Tool>>>,
    resources: Arc<tokio::sync::RwLock<Vec<Resource>>>,
    resource_templates: Arc<tokio::sync::RwLock<Vec<ResourceTemplate>>>,
    prompts: Arc<tokio::sync::RwLock<Vec<Prompt>>>,
    tool_filter: tokio::sync::RwLock<Option<ToolFilter>>,
    param_restrictions: tokio::sync::RwLock<ParamRestrictions>,
    tool_notify: Arc<Notify>,
    resource_notify: Arc<Notify>,
    prompt_notify: Arc<Notify>,
}

impl UpstreamConnection {
    /// Connect to an upstream MCP server.
    pub async fn connect(config: McpServerConfig) -> Result<Self, McpGatewayError> {
        config.validate()?;

        let tool_notify = Arc::new(Notify::new());
        let resource_notify = Arc::new(Notify::new());
        let prompt_notify = Arc::new(Notify::new());
        let handler = UpstreamNotificationHandler {
            tool_notify: Arc::clone(&tool_notify),
            resource_notify: Arc::clone(&resource_notify),
            prompt_notify: Arc::clone(&prompt_notify),
        };

        let map_connect_err = |e: rmcp::service::ClientInitializeError, name: &str| {
            McpGatewayError::UpstreamConnect {
                name: name.to_owned(),
                reason: e.to_string(),
            }
        };

        let service: RunningService<RoleClient, UpstreamNotificationHandler> = match &config
            .transport
        {
            McpTransport::Stdio { command, args, env } => {
                let mut cmd = tokio::process::Command::new(command);
                cmd.args(args);
                for (k, v) in env {
                    cmd.env(k, v);
                }
                let transport =
                    TokioChildProcess::new(cmd).map_err(|e| McpGatewayError::UpstreamConnect {
                        name: config.name.clone(),
                        reason: e.to_string(),
                    })?;
                handler
                    .serve(transport)
                    .await
                    .map_err(|e| map_connect_err(e, &config.name))?
            }
            McpTransport::Http { url, headers } => {
                let mut custom_headers = std::collections::HashMap::new();
                for (k, v) in headers {
                    let header_name: reqwest::header::HeaderName =
                        k.parse().map_err(|e: reqwest::header::InvalidHeaderName| {
                            McpGatewayError::UpstreamConnect {
                                name: config.name.clone(),
                                reason: format!("invalid header name '{k}': {e}"),
                            }
                        })?;
                    let header_value: reqwest::header::HeaderValue =
                        v.parse()
                            .map_err(|e: reqwest::header::InvalidHeaderValue| {
                                McpGatewayError::UpstreamConnect {
                                    name: config.name.clone(),
                                    reason: format!("invalid header value for '{k}': {e}"),
                                }
                            })?;
                    custom_headers.insert(header_name, header_value);
                }

                let transport_config = StreamableHttpClientTransportConfig {
                    uri: url.as_str().into(),
                    custom_headers,
                    ..Default::default()
                };
                let transport = rmcp::transport::StreamableHttpClientTransport::with_client(
                    reqwest::Client::default(),
                    transport_config,
                );
                handler
                    .serve(transport)
                    .await
                    .map_err(|e| map_connect_err(e, &config.name))?
            }
        };

        let initial_tools =
            service
                .list_all_tools()
                .await
                .map_err(|e| McpGatewayError::UpstreamConnect {
                    name: config.name.clone(),
                    reason: format!("failed to list tools: {e}"),
                })?;

        // Best-effort: fetch resources and prompts if the upstream supports them.
        let initial_resources = service.list_all_resources().await.unwrap_or_default();
        let initial_templates = service
            .list_all_resource_templates()
            .await
            .unwrap_or_default();
        let initial_prompts = service.list_all_prompts().await.unwrap_or_default();

        Ok(Self {
            name: config.name,
            service,
            tools: Arc::new(tokio::sync::RwLock::new(initial_tools)),
            resources: Arc::new(tokio::sync::RwLock::new(initial_resources)),
            resource_templates: Arc::new(tokio::sync::RwLock::new(initial_templates)),
            prompts: Arc::new(tokio::sync::RwLock::new(initial_prompts)),
            tool_filter: tokio::sync::RwLock::new(config.tool_filter),
            param_restrictions: tokio::sync::RwLock::new(config.param_restrictions),
            tool_notify,
            resource_notify,
            prompt_notify,
        })
    }

    /// Return all tools from this upstream, filtered and namespaced as `{name}/{tool_name}`.
    pub async fn namespaced_tools(&self) -> Vec<Tool> {
        let tools = self.tools.read().await;
        let filter = self.tool_filter.read().await;
        tools
            .iter()
            .filter(|t| filter.as_ref().is_none_or(|f| f.accepts(&t.name)))
            .map(|t| {
                let prefixed_name = format!("{}/{}", self.name, t.name);
                let mut cloned = t.clone();
                cloned.name = Cow::Owned(prefixed_name);
                cloned
            })
            .collect()
    }

    /// Re-fetch the tool list from the upstream and update the cache.
    pub async fn refresh_tools(&self) -> Result<(), McpGatewayError> {
        let fresh =
            self.service
                .list_all_tools()
                .await
                .map_err(|e| McpGatewayError::UpstreamCall {
                    name: self.name.clone(),
                    reason: format!("failed to refresh tools: {e}"),
                })?;
        let mut cache = self.tools.write().await;
        *cache = fresh;
        Ok(())
    }

    /// Forward a tool call to this upstream using the original (un-prefixed) tool name.
    ///
    /// Parameter restrictions are enforced before forwarding the call.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        mut arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<CallToolResult, McpGatewayError> {
        // Enforce parameter restrictions before forwarding
        self.param_restrictions
            .read()
            .await
            .check(tool_name, &mut arguments)?;

        let mut params = CallToolRequestParams::new(tool_name.to_owned());
        if let Some(args) = arguments {
            params = params.with_arguments(args);
        }
        self.service
            .call_tool(params)
            .await
            .map_err(|e| McpGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: e.to_string(),
            })
    }

    /// Update the tool filter at runtime.
    pub async fn set_filter(&self, filter: Option<ToolFilter>) {
        let mut current = self.tool_filter.write().await;
        *current = filter;
    }

    /// Return a clone of the current tool filter.
    pub async fn filter(&self) -> Option<ToolFilter> {
        self.tool_filter.read().await.clone()
    }

    /// Update the parameter restrictions at runtime.
    pub async fn set_param_restrictions(&self, restrictions: ParamRestrictions) {
        let mut current = self.param_restrictions.write().await;
        *current = restrictions;
    }

    /// Return a clone of the current parameter restrictions.
    pub async fn param_restrictions(&self) -> ParamRestrictions {
        self.param_restrictions.read().await.clone()
    }

    /// Return the number of tools exposed by this upstream (after filtering).
    pub async fn tool_count(&self) -> usize {
        let tools = self.tools.read().await;
        let filter = self.tool_filter.read().await;
        match filter.as_ref() {
            Some(f) => tools.iter().filter(|t| f.accepts(&t.name)).count(),
            None => tools.len(),
        }
    }

    /// Expose the tool-change notify handle for spawning background refresh tasks.
    pub fn tool_change_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.tool_notify)
    }

    /// Expose the resource-change notify handle for spawning background refresh tasks.
    pub fn resource_change_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.resource_notify)
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
                let prefixed_uri = format!("{}+{}", self.name, r.raw.uri);
                (
                    prefixed_uri,
                    r.raw.name.clone(),
                    r.raw.description.clone(),
                    r.raw.mime_type.clone(),
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
                let prefixed = format!("{}+{}", self.name, t.raw.uri_template);
                (
                    prefixed,
                    t.raw.name.clone(),
                    t.raw.description.clone(),
                    t.raw.mime_type.clone(),
                )
            })
            .collect()
    }

    /// Read a resource from this upstream using its original (un-prefixed) URI.
    pub async fn read_resource(&self, uri: &str) -> Result<ReadResourceResult, McpGatewayError> {
        let params = ReadResourceRequestParams::new(uri.to_owned());
        self.service
            .read_resource(params)
            .await
            .map_err(|e| McpGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: e.to_string(),
            })
    }

    /// Expose the prompt-change notify handle for spawning background refresh tasks.
    pub fn prompt_change_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.prompt_notify)
    }

    // ── Prompt methods ──────────────────────────────────────────────

    /// Return all prompts from this upstream, namespaced as `{name}/{prompt_name}`.
    pub async fn namespaced_prompts(
        &self,
    ) -> Vec<(String, Option<String>, Vec<rmcp::model::PromptArgument>)> {
        let prompts = self.prompts.read().await;
        prompts
            .iter()
            .map(|p| {
                let prefixed_name = format!("{}/{}", self.name, p.name);
                let args = p.arguments.clone().unwrap_or_default();
                (prefixed_name, p.description.clone(), args)
            })
            .collect()
    }

    /// Get a prompt from this upstream using the original (un-prefixed) name.
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<std::collections::HashMap<String, String>>,
    ) -> Result<GetPromptResult, McpGatewayError> {
        let mut params = GetPromptRequestParams::new(name.to_owned());
        if let Some(args) = arguments {
            let map: serde_json::Map<String, serde_json::Value> = args
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect();
            params.arguments = Some(map);
        }
        self.service
            .get_prompt(params)
            .await
            .map_err(|e| McpGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: e.to_string(),
            })
    }

    /// Re-fetch prompts from the upstream and update the cache.
    pub async fn refresh_prompts(&self) -> Result<(), McpGatewayError> {
        let fresh =
            self.service
                .list_all_prompts()
                .await
                .map_err(|e| McpGatewayError::UpstreamCall {
                    name: self.name.clone(),
                    reason: format!("failed to refresh prompts: {e}"),
                })?;
        let mut cache = self.prompts.write().await;
        *cache = fresh;
        Ok(())
    }

    /// Re-fetch resources and templates from the upstream and update the caches.
    pub async fn refresh_resources(&self) -> Result<(), McpGatewayError> {
        let fresh_resources =
            self.service
                .list_all_resources()
                .await
                .map_err(|e| McpGatewayError::UpstreamCall {
                    name: self.name.clone(),
                    reason: format!("failed to refresh resources: {e}"),
                })?;
        let fresh_templates = self
            .service
            .list_all_resource_templates()
            .await
            .map_err(|e| McpGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: format!("failed to refresh resource templates: {e}"),
            })?;
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
}
