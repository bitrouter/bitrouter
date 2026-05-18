//! [`Executor`] backed by the official [rmcp](https://github.com/modelcontextprotocol/rust-sdk)
//! client.
//!
//! Dispatches `tools/list`, `tools/call`, `resources/list`, `resources/read`,
//! `resources/templates/list`, `prompts/list`, and `prompts/get` to typed rmcp
//! peer methods. Unknown methods come back as JSON-RPC "Method not found"
//! (`-32601`). The MCP spec method catalogue is at
//! <https://modelcontextprotocol.io/specification/2025-06-18>.
//!
//! Connections are pooled per server-name and lazily initialised. The first
//! request to each server triggers the MCP `initialize` handshake (handled
//! transparently by rmcp's `serve()`); subsequent requests reuse the same
//! [`RunningService`]. There is no idle eviction in v1.0 — the pool grows to
//! the number of distinct servers reached.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use rmcp::ServiceExt;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, ClientInfo, GetPromptRequestParams, Implementation,
    ReadResourceRequestParams,
};
use rmcp::service::{Peer, RoleClient, RunningService};
use tokio::sync::Mutex;

use super::transport::McpTransport;
use super::{Executor, McpRequest, McpResponse, McpTarget};
use crate::error::{BitrouterError, Result};

/// Minimal [`ClientHandler`] — we don't yet expose server→client sampling /
/// elicitation back to the pipeline caller. The default trait impls reject
/// those requests with `MethodNotFound`, which is the correct behaviour for
/// a transparent router that hasn't been asked to support them yet.
#[derive(Debug, Clone, Default)]
struct BitrouterMcpClient;

impl ClientHandler for BitrouterMcpClient {
    fn get_info(&self) -> ClientInfo {
        let mut info = ClientInfo::default();
        info.client_info = Implementation::new("bitrouter", env!("CARGO_PKG_VERSION"));
        info
    }
}

/// Pooled rmcp client used by [`RmcpExecutor`].
type Pool = Mutex<HashMap<String, Arc<RunningService<RoleClient, BitrouterMcpClient>>>>;

/// [`Executor`] that forwards [`McpRequest`]s to upstream MCP servers via
/// rmcp.
#[derive(Default)]
pub struct RmcpExecutor {
    pool: Pool,
}

impl RmcpExecutor {
    /// Fresh executor with an empty connection pool.
    pub fn new() -> Self {
        Self::default()
    }

    async fn peer_for(
        &self,
        target: &McpTarget,
    ) -> Result<Arc<RunningService<RoleClient, BitrouterMcpClient>>> {
        // Fast path: already connected.
        if let Some(existing) = self.pool.lock().await.get(&target.server_name).cloned() {
            return Ok(existing);
        }
        // Slow path: dial. We drop the lock across the network round-trip so
        // a slow `initialize` against one server can't block lookups for
        // another. If two requests race to dial the same server, both will
        // dial; the second one's value silently replaces the first in the
        // pool — fine because either RunningService is correct.
        let service = connect(target).await?;
        let arc = Arc::new(service);
        self.pool
            .lock()
            .await
            .insert(target.server_name.clone(), Arc::clone(&arc));
        Ok(arc)
    }
}

async fn connect(target: &McpTarget) -> Result<RunningService<RoleClient, BitrouterMcpClient>> {
    match &target.transport {
        McpTransport::Http { url, headers } => {
            // Streamable HTTP transport per the MCP spec
            // <https://modelcontextprotocol.io/specification/2025-06-18/basic/transports#streamable-http>.
            //
            // We construct the transport via `from_config`, which uses rmcp's
            // internally-bundled reqwest client. That keeps rmcp's reqwest
            // version independent of the workspace reqwest used by the
            // language_model executor.
            use http::{HeaderName, HeaderValue};
            let mut header_map: std::collections::HashMap<HeaderName, HeaderValue> =
                std::collections::HashMap::new();
            for (k, v) in headers {
                let name: HeaderName = k.parse().map_err(|e| {
                    BitrouterError::internal(format!(
                        "mcp '{}': invalid header name '{k}': {e}",
                        target.server_name
                    ))
                })?;
                let value: HeaderValue = v.parse().map_err(|e| {
                    BitrouterError::internal(format!(
                        "mcp '{}': invalid header value for '{k}': {e}",
                        target.server_name
                    ))
                })?;
                header_map.insert(name, value);
            }
            let cfg =
                rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
                    url.clone(),
                )
                .custom_headers(header_map);
            let transport = rmcp::transport::StreamableHttpClientTransport::from_config(cfg);
            BitrouterMcpClient
                .serve(transport)
                .await
                .map_err(|e| upstream(&target.server_name, format!("HTTP connect: {e}")))
        }
        McpTransport::Stdio { command, args, env } => {
            // stdio child-process transport per the MCP spec
            // <https://modelcontextprotocol.io/specification/2025-06-18/basic/transports#stdio>.
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args);
            for (k, v) in env {
                cmd.env(k, v);
            }
            let transport = rmcp::transport::TokioChildProcess::new(cmd)
                .map_err(|e| upstream(&target.server_name, format!("spawning '{command}': {e}")))?;
            BitrouterMcpClient
                .serve(transport)
                .await
                .map_err(|e| upstream(&target.server_name, format!("stdio connect: {e}")))
        }
    }
}

fn upstream(server: &str, msg: impl Into<String>) -> BitrouterError {
    BitrouterError::Upstream {
        status: 502,
        message: format!("mcp '{server}': {}", msg.into()),
    }
}

fn bad_params(server: &str, method: &str, msg: impl std::fmt::Display) -> BitrouterError {
    BitrouterError::bad_request(format!("mcp '{server}' {method}: {msg}"))
}

#[async_trait]
impl Executor for RmcpExecutor {
    async fn execute(&self, target: &McpTarget, request: &McpRequest) -> Result<McpResponse> {
        let peer = self.peer_for(target).await?.peer().clone();
        let result = dispatch(&peer, target, request).await?;
        Ok(McpResponse {
            request_id: request.request_id.clone(),
            result,
        })
    }
}

async fn dispatch(
    peer: &Peer<RoleClient>,
    target: &McpTarget,
    request: &McpRequest,
) -> Result<serde_json::Value> {
    let server = target.server_name.as_str();
    let method = request.method.as_str();
    match method {
        "tools/list" => {
            let tools = peer
                .list_all_tools()
                .await
                .map_err(|e| upstream(server, format!("tools/list: {e}")))?;
            Ok(serde_json::json!({ "tools": tools }))
        }
        "tools/call" => {
            let params: CallToolRequestParams = serde_json::from_value(request.params.clone())
                .map_err(|e| bad_params(server, method, e))?;
            let result = peer
                .call_tool(params)
                .await
                .map_err(|e| upstream(server, format!("tools/call: {e}")))?;
            serde_json::to_value(&result).map_err(|e| {
                BitrouterError::internal(format!("mcp '{server}' tools/call serialise: {e}"))
            })
        }
        "resources/list" => {
            let resources = peer
                .list_all_resources()
                .await
                .map_err(|e| upstream(server, format!("resources/list: {e}")))?;
            Ok(serde_json::json!({ "resources": resources }))
        }
        "resources/read" => {
            let params: ReadResourceRequestParams = serde_json::from_value(request.params.clone())
                .map_err(|e| bad_params(server, method, e))?;
            let result = peer
                .read_resource(params)
                .await
                .map_err(|e| upstream(server, format!("resources/read: {e}")))?;
            serde_json::to_value(&result).map_err(|e| {
                BitrouterError::internal(format!("mcp '{server}' resources/read serialise: {e}"))
            })
        }
        "resources/templates/list" => {
            let templates = peer
                .list_all_resource_templates()
                .await
                .map_err(|e| upstream(server, format!("resources/templates/list: {e}")))?;
            Ok(serde_json::json!({ "resourceTemplates": templates }))
        }
        "prompts/list" => {
            let prompts = peer
                .list_all_prompts()
                .await
                .map_err(|e| upstream(server, format!("prompts/list: {e}")))?;
            Ok(serde_json::json!({ "prompts": prompts }))
        }
        "prompts/get" => {
            let params: GetPromptRequestParams = serde_json::from_value(request.params.clone())
                .map_err(|e| bad_params(server, method, e))?;
            let result = peer
                .get_prompt(params)
                .await
                .map_err(|e| upstream(server, format!("prompts/get: {e}")))?;
            serde_json::to_value(&result).map_err(|e| {
                BitrouterError::internal(format!("mcp '{server}' prompts/get serialise: {e}"))
            })
        }
        // The spec catalogue is closed for v0 of the protocol; if the inbound
        // client invents one, surface it as a JSON-RPC "Method not found".
        other => Err(BitrouterError::NotFound(format!(
            "mcp '{server}': method '{other}' not supported by v1.0 RmcpExecutor"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caller::{CallerContext, PaymentMethod};
    use std::collections::HashMap;

    fn req(server: &str, method: &str) -> McpRequest {
        McpRequest::new(
            server,
            method,
            serde_json::json!({}),
            CallerContext::new("k", "u", PaymentMethod::None),
        )
    }

    #[test]
    fn executor_constructs_with_empty_pool() {
        let _ = RmcpExecutor::new();
    }

    /// `/bin/false` exits immediately with status 1 — rmcp's `serve()` sees
    /// EOF before the `initialize` handshake completes and surfaces a
    /// transport error. We assert the executor maps that to a 502 Upstream,
    /// not a panic or a 500.
    #[tokio::test]
    async fn stdio_connect_failure_surfaces_as_502_upstream() {
        let exec = RmcpExecutor::new();
        let target = McpTarget {
            server_name: "ghost".into(),
            transport: McpTransport::Stdio {
                command: "/bin/false".into(),
                args: vec![],
                env: HashMap::new(),
            },
        };
        let err = exec
            .execute(&target, &req("ghost", "tools/list"))
            .await
            .unwrap_err();
        assert_eq!(err.status(), 502, "unexpected error: {err}");
        assert!(
            err.to_string().contains("mcp 'ghost'"),
            "error should be server-tagged: {err}"
        );
    }

    /// Stdio with a command that does not exist — the spawn itself fails.
    /// Same 502 mapping.
    #[tokio::test]
    async fn stdio_spawn_failure_surfaces_as_502_upstream() {
        let exec = RmcpExecutor::new();
        let target = McpTarget {
            server_name: "ghost".into(),
            transport: McpTransport::Stdio {
                command: "/definitely/does/not/exist/bitrouter-mcp-test".into(),
                args: vec![],
                env: HashMap::new(),
            },
        };
        let err = exec
            .execute(&target, &req("ghost", "tools/list"))
            .await
            .unwrap_err();
        assert_eq!(err.status(), 502, "unexpected error: {err}");
    }
}
