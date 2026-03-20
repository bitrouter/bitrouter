use std::borrow::Cow;
use std::sync::Arc;

use rmcp::ClientHandler;
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientInfo, Tool};
use rmcp::service::{RoleClient, RunningService, ServiceExt as _};
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use tokio::sync::Notify;

use bitrouter_mcp::config::{McpServerConfig, McpTransport, ToolFilter};
use bitrouter_mcp::error::McpGatewayError;
use bitrouter_mcp::param_filter::ParamRestrictions;

/// Handler that receives tool-list-changed notifications from an upstream.
struct ToolChangeHandler {
    notify: Arc<Notify>,
}

impl ClientHandler for ToolChangeHandler {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }

    fn on_tool_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.notify.notify_one();
        std::future::ready(())
    }
}

/// A live connection to a single upstream MCP server.
pub struct UpstreamConnection {
    name: String,
    service: RunningService<RoleClient, ToolChangeHandler>,
    tools: Arc<tokio::sync::RwLock<Vec<Tool>>>,
    tool_filter: tokio::sync::RwLock<Option<ToolFilter>>,
    param_restrictions: tokio::sync::RwLock<ParamRestrictions>,
    notify: Arc<Notify>,
}

impl UpstreamConnection {
    /// Connect to an upstream MCP server.
    pub async fn connect(config: McpServerConfig) -> Result<Self, McpGatewayError> {
        config.validate()?;

        let notify = Arc::new(Notify::new());
        let handler = ToolChangeHandler {
            notify: Arc::clone(&notify),
        };

        let map_connect_err = |e: rmcp::service::ClientInitializeError, name: &str| {
            McpGatewayError::UpstreamConnect {
                name: name.to_owned(),
                reason: e.to_string(),
            }
        };

        let service: RunningService<RoleClient, ToolChangeHandler> = match &config.transport {
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

        Ok(Self {
            name: config.name,
            service,
            tools: Arc::new(tokio::sync::RwLock::new(initial_tools)),
            tool_filter: tokio::sync::RwLock::new(config.tool_filter),
            param_restrictions: tokio::sync::RwLock::new(config.param_restrictions),
            notify,
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

    /// Expose the notify handle for spawning background refresh tasks.
    pub fn tool_change_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.notify)
    }
}
