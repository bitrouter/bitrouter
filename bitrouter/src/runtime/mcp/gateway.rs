use std::collections::HashMap;
use std::sync::Arc;

use rmcp::ServerHandler;
use rmcp::model::{
    CallToolResult, ErrorCode, ErrorData, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};

use bitrouter_core::observe::{ToolCallEvent, ToolObserveCallback};

use bitrouter_mcp::config::{McpServerConfig, ToolCostConfig};
use bitrouter_mcp::error::McpGatewayError;
use bitrouter_mcp::groups::McpAccessGroups;

use super::registry::UpstreamRegistry;

/// MCP gateway that aggregates tools from multiple upstream MCP servers.
pub struct McpGateway {
    registry: Arc<UpstreamRegistry>,
    observer: Option<Arc<dyn ToolObserveCallback>>,
    cost_configs: HashMap<String, ToolCostConfig>,
    refresh_tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl McpGateway {
    /// Build a gateway from config, connecting to all upstreams eagerly.
    ///
    /// Spawns background tasks that listen for tool-list-changed notifications
    /// and refresh the corresponding upstream's tool cache.
    pub async fn new(
        configs: Vec<McpServerConfig>,
        groups: McpAccessGroups,
    ) -> Result<Self, McpGatewayError> {
        // Build cost config map from server configs before consuming them.
        let cost_configs: HashMap<String, ToolCostConfig> = configs
            .iter()
            .map(|c| (c.name.clone(), c.cost.clone()))
            .collect();

        let registry = Arc::new(UpstreamRegistry::from_configs(configs, groups).await?);

        // Spawn background refresh listeners
        let mut refresh_tasks = Vec::new();
        for (name, notify) in registry.tool_change_notifiers().await {
            let reg = Arc::clone(&registry);
            let handle = tokio::spawn(async move {
                loop {
                    notify.notified().await;
                    tracing::info!(upstream = %name, "tool list changed, refreshing");
                    if let Err(e) = reg.refresh_upstream(&name).await {
                        tracing::warn!(upstream = %name, error = %e, "failed to refresh tools");
                    } else {
                        reg.notify_downstream_change();
                    }
                }
            });
            refresh_tasks.push(handle);
        }

        Ok(Self {
            registry,
            observer: None,
            cost_configs,
            refresh_tasks,
        })
    }

    /// Attach a tool call observer for cost tracking.
    pub fn with_observer(mut self, observer: Arc<dyn ToolObserveCallback>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Access the underlying registry as a shared pointer.
    pub fn registry_arc(&self) -> &Arc<UpstreamRegistry> {
        &self.registry
    }
}

impl Drop for McpGateway {
    fn drop(&mut self) {
        for handle in &self.refresh_tasks {
            handle.abort();
        }
    }
}

/// Convert a gateway error into an MCP `ErrorData` for the wire.
fn gateway_error_to_error_data(err: McpGatewayError) -> ErrorData {
    match &err {
        McpGatewayError::ToolNotFound { .. } => {
            ErrorData::new(ErrorCode::METHOD_NOT_FOUND, err.to_string(), None)
        }
        McpGatewayError::InvalidConfig { .. } | McpGatewayError::ParamDenied { .. } => {
            ErrorData::invalid_params(err.to_string(), None)
        }
        _ => ErrorData::internal_error(err.to_string(), None),
    }
}

impl ServerHandler for McpGateway {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_instructions(
            "BitRouter MCP Gateway — aggregated tools from multiple upstream MCP servers"
                .to_owned(),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self.registry.aggregated_tools().await;
        Ok(ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let start = tokio::time::Instant::now();
        let result = self
            .registry
            .route_call(&request.name, request.arguments)
            .await;
        let latency_ms = start.elapsed().as_millis() as u64;

        // Fire observer if present
        if let Some(observer) = &self.observer {
            let (server, tool) = match request.name.split_once('/') {
                Some(pair) => pair,
                None => {
                    tracing::warn!(tool = %request.name, "tool name missing namespace separator");
                    ("unknown", request.name.as_ref())
                }
            };

            let cost = self
                .cost_configs
                .get(server)
                .map(|c| c.cost_for(tool))
                .unwrap_or(0.0);

            let (success, error_message) = match &result {
                Ok(_) => (true, None),
                Err(e) => (false, Some(e.to_string())),
            };

            let event = ToolCallEvent {
                account_id: None, // caller identity not yet threaded through MCP
                server: server.to_owned(),
                tool: tool.to_owned(),
                cost,
                latency_ms,
                success,
                error_message,
            };
            observer.on_tool_call(event).await;
        }

        result.map_err(gateway_error_to_error_data)
    }
}
