//! Stdio transport for upstream MCP connections via `rmcp`.
//!
//! This module is feature-gated behind `mcp-stdio` and provides
//! child-process MCP connections. It converts `rmcp` types to the
//! crate's own `types` module types at the boundary.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, GetPromptRequestParams, PromptMessageContent,
    ReadResourceRequestParams, ResourceContents, Tool,
};
use rmcp::service::{RoleClient, RunningService, ServiceExt as _};
use rmcp::transport::TokioChildProcess;
use tokio::sync::Notify;

use bitrouter_core::api::mcp::error::McpGatewayError;
use bitrouter_core::api::mcp::types::{
    InitializeResult, McpContent, McpGetPromptResult, McpPrompt, McpPromptArgument,
    McpPromptContent, McpPromptMessage, McpResource, McpResourceContent, McpResourceTemplate,
    McpRole, McpTool, McpToolCallResult, ServerCapabilities, ServerInfo, ToolsCapability,
};

/// Handler that receives notifications from an upstream MCP server.
struct StdioNotificationHandler {
    tool_notify: Arc<Notify>,
    resource_notify: Arc<Notify>,
    prompt_notify: Arc<Notify>,
}

impl ClientHandler for StdioNotificationHandler {
    fn get_info(&self) -> rmcp::model::ClientInfo {
        rmcp::model::ClientInfo::default()
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

/// A live stdio connection to a single upstream MCP server via `rmcp`.
pub struct StdioConnection {
    service: RunningService<RoleClient, StdioNotificationHandler>,
    tool_notify: Arc<Notify>,
    resource_notify: Arc<Notify>,
    prompt_notify: Arc<Notify>,
}

impl StdioConnection {
    /// Spawn a child process and connect via stdio.
    pub async fn connect(
        name: String,
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    ) -> Result<Self, McpGatewayError> {
        let tool_notify = Arc::new(Notify::new());
        let resource_notify = Arc::new(Notify::new());
        let prompt_notify = Arc::new(Notify::new());
        let handler = StdioNotificationHandler {
            tool_notify: Arc::clone(&tool_notify),
            resource_notify: Arc::clone(&resource_notify),
            prompt_notify: Arc::clone(&prompt_notify),
        };

        let mut cmd = tokio::process::Command::new(&command);
        cmd.args(&args);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let transport =
            TokioChildProcess::new(cmd).map_err(|e| McpGatewayError::UpstreamConnect {
                name: name.clone(),
                reason: e.to_string(),
            })?;

        let service =
            handler
                .serve(transport)
                .await
                .map_err(|e| McpGatewayError::UpstreamConnect {
                    name,
                    reason: e.to_string(),
                })?;

        Ok(Self {
            service,
            tool_notify,
            resource_notify,
            prompt_notify,
        })
    }

    // ── Notify handles ─────────────────────────────────────────────

    pub fn tool_change_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.tool_notify)
    }

    pub fn resource_change_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.resource_notify)
    }

    pub fn prompt_change_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.prompt_notify)
    }

    // ── Internal ───────────────────────────────────────────────────

    fn call_error(&self, reason: String) -> McpGatewayError {
        McpGatewayError::UpstreamCall {
            name: String::new(),
            reason,
        }
    }
}

// ── McpTransport impl ────────────────────────────────────────

impl super::McpTransport for StdioConnection {
    async fn initialize(&self) -> Result<InitializeResult, McpGatewayError> {
        // rmcp handles initialization internally via `serve()`.
        // Return a synthetic result reflecting the connection state.
        let server_info = self.service.peer_info();
        Ok(InitializeResult {
            protocol_version: "2025-03-26".to_owned(),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability {
                    list_changed: Some(true),
                }),
                ..Default::default()
            },
            server_info: ServerInfo {
                name: server_info
                    .as_ref()
                    .map(|i| i.server_info.name.clone())
                    .unwrap_or_default(),
                version: server_info.as_ref().map(|i| i.server_info.version.clone()),
            },
            instructions: None,
        })
    }

    async fn terminate(&self) {
        // rmcp handles cleanup on drop; nothing to do here.
    }

    async fn list_tools(&self) -> Result<Vec<McpTool>, McpGatewayError> {
        let tools = self
            .service
            .list_all_tools()
            .await
            .map_err(|e| self.call_error(format!("failed to list tools: {e}")))?;
        Ok(tools.iter().map(rmcp_tool_to_mcp_tool).collect())
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<McpToolCallResult, McpGatewayError> {
        let mut params = CallToolRequestParams::new(tool_name.to_owned());
        if let Some(args) = arguments {
            params = params.with_arguments(args);
        }
        let result = self
            .service
            .call_tool(params)
            .await
            .map_err(|e| self.call_error(e.to_string()))?;
        Ok(rmcp_result_to_mcp_result(&result))
    }

    async fn list_resources(&self) -> Result<Vec<McpResource>, McpGatewayError> {
        let resources = self.service.list_all_resources().await.unwrap_or_default();
        Ok(resources
            .iter()
            .map(|r| McpResource {
                uri: r.raw.uri.clone(),
                name: r.raw.name.clone(),
                description: r.raw.description.clone(),
                mime_type: r.raw.mime_type.clone(),
            })
            .collect())
    }

    async fn read_resource(&self, uri: &str) -> Result<Vec<McpResourceContent>, McpGatewayError> {
        let params = ReadResourceRequestParams::new(uri.to_owned());
        let result = self
            .service
            .read_resource(params)
            .await
            .map_err(|e| self.call_error(e.to_string()))?;
        Ok(result
            .contents
            .iter()
            .map(rmcp_resource_contents_to_mcp)
            .collect())
    }

    async fn list_resource_templates(&self) -> Result<Vec<McpResourceTemplate>, McpGatewayError> {
        let templates = self
            .service
            .list_all_resource_templates()
            .await
            .unwrap_or_default();
        Ok(templates
            .iter()
            .map(|t| McpResourceTemplate {
                uri_template: t.raw.uri_template.clone(),
                name: t.raw.name.clone(),
                description: t.raw.description.clone(),
                mime_type: t.raw.mime_type.clone(),
            })
            .collect())
    }

    async fn list_prompts(&self) -> Result<Vec<McpPrompt>, McpGatewayError> {
        let prompts = self.service.list_all_prompts().await.unwrap_or_default();
        Ok(prompts
            .iter()
            .map(|p| {
                let arguments = p
                    .arguments
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|a| McpPromptArgument {
                        name: a.name,
                        description: a.description,
                        required: a.required,
                    })
                    .collect();
                McpPrompt {
                    name: p.name.to_string(),
                    description: p.description.clone(),
                    arguments,
                }
            })
            .collect())
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<HashMap<String, String>>,
    ) -> Result<McpGetPromptResult, McpGatewayError> {
        let mut params = GetPromptRequestParams::new(name.to_owned());
        if let Some(args) = arguments {
            let map: serde_json::Map<String, serde_json::Value> = args
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect();
            params.arguments = Some(map);
        }
        let result = self
            .service
            .get_prompt(params)
            .await
            .map_err(|e| self.call_error(e.to_string()))?;
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
}

// ── rmcp -> McpTool conversion functions ────────────────────────────

fn rmcp_tool_to_mcp_tool(tool: &Tool) -> McpTool {
    McpTool {
        name: tool.name.to_string(),
        description: tool.description.as_deref().map(str::to_owned),
        input_schema: serde_json::to_value(&*tool.input_schema).unwrap_or_default(),
    }
}

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
