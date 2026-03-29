//! MCP Streamable HTTP client.
//!
//! Speaks JSON-RPC 2.0 over the MCP Streamable HTTP transport to any
//! MCP-compliant server. Handles session management, content-type
//! negotiation (JSON vs SSE responses), and cursor-based pagination.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};

use tokio::sync::RwLock;

use bitrouter_core::api::mcp::error::McpGatewayError;
use bitrouter_core::api::mcp::types::{
    CallToolParams, CallToolResult, ClientCapabilities, ClientInfo, GetPromptParams,
    InitializeParams, InitializeResult, JsonRpcId, JsonRpcNotification, JsonRpcRequest,
    JsonRpcResponse, ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult,
    ListToolsParams, ListToolsResult, McpGetPromptResult, McpPrompt, McpResource,
    McpResourceContent, McpResourceTemplate, McpTool, McpToolCallResult, ReadResourceParams,
    ReadResourceResult,
};

/// MCP protocol version this client advertises.
const PROTOCOL_VERSION: &str = "2025-03-26";

/// Process-global monotonic request ID counter shared by all MCP HTTP clients.
///
/// Upstream MCP servers treat request IDs as opaque and do not correlate
/// them across connections, so sharing a single counter is harmless and
/// simplifies debugging (IDs are globally unique within a process).
static REQUEST_ID: AtomicI64 = AtomicI64::new(1);

/// MCP Streamable HTTP session state.
struct McpSession {
    session_id: Option<String>,
    protocol_version: Option<String>,
}

/// MCP Streamable HTTP client.
///
/// Connects to an upstream MCP server over HTTP, implementing the
/// [Streamable HTTP transport](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports#streamable-http).
pub struct McpHttpClient {
    http: reqwest::Client,
    url: String,
    name: String,
    session: RwLock<McpSession>,
}

impl McpHttpClient {
    /// Build a new client for the given MCP endpoint URL.
    ///
    /// Custom headers (e.g. `Authorization`) are set as default headers on
    /// the underlying reqwest client.
    pub fn new(
        name: impl Into<String>,
        url: impl Into<String>,
        headers: &HashMap<String, String>,
    ) -> Result<Self, McpGatewayError> {
        let name = name.into();
        let mut header_map = reqwest::header::HeaderMap::new();
        for (k, v) in headers {
            let header_name: reqwest::header::HeaderName =
                k.parse().map_err(|e| McpGatewayError::UpstreamConnect {
                    name: name.clone(),
                    reason: format!("invalid header name '{k}': {e}"),
                })?;
            let header_value: reqwest::header::HeaderValue =
                v.parse().map_err(|e| McpGatewayError::UpstreamConnect {
                    name: name.clone(),
                    reason: format!("invalid header value for '{k}': {e}"),
                })?;
            header_map.insert(header_name, header_value);
        }

        let http = reqwest::Client::builder()
            .default_headers(header_map)
            .build()
            .map_err(|e| McpGatewayError::UpstreamConnect {
                name: name.clone(),
                reason: format!("failed to build HTTP client: {e}"),
            })?;

        Ok(Self {
            http,
            url: url.into(),
            name,
            session: RwLock::new(McpSession {
                session_id: None,
                protocol_version: None,
            }),
        })
    }

    // ── Internal JSON-RPC helpers ──────────────────────────────────

    /// Send a JSON-RPC request and return the result value.
    async fn rpc_call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpGatewayError> {
        let (value, _headers) = self.rpc_call_with_headers(method, params).await?;
        Ok(value)
    }

    /// Send a JSON-RPC request and return both the result value and
    /// response headers (needed during initialization for session ID).
    async fn rpc_call_with_headers(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(serde_json::Value, reqwest::header::HeaderMap), McpGatewayError> {
        let id = JsonRpcId::Number(REQUEST_ID.fetch_add(1, Ordering::Relaxed));
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: id.clone(),
            method: method.to_owned(),
            params: Some(params),
        };

        let session = self.session.read().await;
        let mut builder = self
            .http
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");

        if let Some(ref sid) = session.session_id {
            builder = builder.header("Mcp-Session-Id", sid);
        }
        if let Some(ref version) = session.protocol_version {
            builder = builder.header("MCP-Protocol-Version", version);
        }
        drop(session);

        let response =
            builder
                .json(&request)
                .send()
                .await
                .map_err(|e| McpGatewayError::HttpTransport {
                    name: self.name.clone(),
                    reason: format!("failed to send {method} request: {e}"),
                })?;

        let status = response.status();
        if status.as_u16() == 404 {
            return Err(McpGatewayError::SessionExpired {
                name: self.name.clone(),
            });
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(McpGatewayError::HttpTransport {
                name: self.name.clone(),
                reason: format!("HTTP {status} for {method}: {body}"),
            });
        }

        let response_headers = response.headers().clone();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();

        let rpc_response = if content_type.contains("text/event-stream") {
            let body = response
                .text()
                .await
                .map_err(|e| McpGatewayError::HttpTransport {
                    name: self.name.clone(),
                    reason: format!("failed to read SSE body for {method}: {e}"),
                })?;
            parse_sse_response(&body, &self.name, method)?
        } else {
            response.json::<JsonRpcResponse>().await.map_err(|e| {
                McpGatewayError::HttpTransport {
                    name: self.name.clone(),
                    reason: format!("failed to parse JSON response for {method}: {e}"),
                }
            })?
        };

        // Extract result or propagate JSON-RPC error.
        if let Some(error) = rpc_response.error {
            return Err(McpGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: format!("{method} error ({}): {}", error.code, error.message),
            });
        }

        let result = rpc_response.result.unwrap_or(serde_json::Value::Null);
        Ok((result, response_headers))
    }

    /// Send a JSON-RPC notification (no `id`, expects 202 Accepted).
    async fn rpc_notify(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), McpGatewayError> {
        let notification = JsonRpcNotification {
            jsonrpc: "2.0".to_owned(),
            method: method.to_owned(),
            params,
        };

        let session = self.session.read().await;
        let mut builder = self
            .http
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");

        if let Some(ref sid) = session.session_id {
            builder = builder.header("Mcp-Session-Id", sid);
        }
        if let Some(ref version) = session.protocol_version {
            builder = builder.header("MCP-Protocol-Version", version);
        }
        drop(session);

        let response = builder.json(&notification).send().await.map_err(|e| {
            McpGatewayError::HttpTransport {
                name: self.name.clone(),
                reason: format!("failed to send notification {method}: {e}"),
            }
        })?;

        let status = response.status();
        if !status.is_success() && status.as_u16() != 202 {
            let body = response.text().await.unwrap_or_default();
            return Err(McpGatewayError::HttpTransport {
                name: self.name.clone(),
                reason: format!("notification {method} returned HTTP {status}: {body}"),
            });
        }

        Ok(())
    }

    /// Build an `UpstreamCall` error scoped to this connection.
    fn call_error(&self, reason: String) -> McpGatewayError {
        McpGatewayError::UpstreamCall {
            name: self.name.clone(),
            reason,
        }
    }
}

// ── McpTransport impl ────────────────────────────────────────

impl super::McpTransport for McpHttpClient {
    async fn initialize(&self) -> Result<InitializeResult, McpGatewayError> {
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION.to_owned(),
            capabilities: ClientCapabilities::default(),
            client_info: ClientInfo {
                name: "bitrouter".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            },
        };
        let params_value =
            serde_json::to_value(&params).map_err(|e| McpGatewayError::UpstreamConnect {
                name: self.name.clone(),
                reason: format!("failed to serialize initialize params: {e}"),
            })?;

        let (result_value, response_headers) = self
            .rpc_call_with_headers("initialize", params_value)
            .await?;

        // Capture session ID from response headers.
        if let Some(session_id) = response_headers
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            let mut session = self.session.write().await;
            session.session_id = Some(session_id.to_owned());
        }

        let init_result: InitializeResult =
            serde_json::from_value(result_value).map_err(|e| McpGatewayError::UpstreamConnect {
                name: self.name.clone(),
                reason: format!("failed to parse initialize result: {e}"),
            })?;

        // Store negotiated protocol version.
        {
            let mut session = self.session.write().await;
            session.protocol_version = Some(init_result.protocol_version.clone());
        }

        // Send initialized notification.
        self.rpc_notify("notifications/initialized", None).await?;

        Ok(init_result)
    }

    async fn terminate(&self) {
        let session = self.session.read().await;
        let mut builder = self.http.delete(&self.url);
        if let Some(ref sid) = session.session_id {
            builder = builder.header("Mcp-Session-Id", sid);
        }
        // Best-effort: ignore errors on teardown.
        let _ = builder.send().await;
    }

    async fn list_tools(&self) -> Result<Vec<McpTool>, McpGatewayError> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = ListToolsParams {
                cursor: cursor.clone(),
            };
            let value = self
                .rpc_call(
                    "tools/list",
                    serde_json::to_value(&params).map_err(|e| {
                        self.call_error(format!("failed to serialize tools/list params: {e}"))
                    })?,
                )
                .await?;
            let result: ListToolsResult = serde_json::from_value(value)
                .map_err(|e| self.call_error(format!("failed to parse tools/list result: {e}")))?;
            all.extend(result.tools);
            cursor = result.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(all)
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<McpToolCallResult, McpGatewayError> {
        let params = CallToolParams {
            name: name.to_owned(),
            arguments,
        };
        let value = self
            .rpc_call(
                "tools/call",
                serde_json::to_value(&params).map_err(|e| {
                    self.call_error(format!("failed to serialize tools/call params: {e}"))
                })?,
            )
            .await?;
        let result: CallToolResult = serde_json::from_value(value)
            .map_err(|e| self.call_error(format!("failed to parse tools/call result: {e}")))?;
        Ok(result)
    }

    async fn list_resources(&self) -> Result<Vec<McpResource>, McpGatewayError> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = serde_json::json!({ "cursor": cursor });
            let value = self.rpc_call("resources/list", params).await?;
            let result: ListResourcesResult = serde_json::from_value(value).map_err(|e| {
                self.call_error(format!("failed to parse resources/list result: {e}"))
            })?;
            all.extend(result.resources);
            cursor = result.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(all)
    }

    async fn read_resource(&self, uri: &str) -> Result<Vec<McpResourceContent>, McpGatewayError> {
        let params = ReadResourceParams {
            uri: uri.to_owned(),
        };
        let value = self
            .rpc_call(
                "resources/read",
                serde_json::to_value(&params).map_err(|e| {
                    self.call_error(format!("failed to serialize resources/read params: {e}"))
                })?,
            )
            .await?;
        let result: ReadResourceResult = serde_json::from_value(value)
            .map_err(|e| self.call_error(format!("failed to parse resources/read result: {e}")))?;
        Ok(result.contents)
    }

    async fn list_resource_templates(&self) -> Result<Vec<McpResourceTemplate>, McpGatewayError> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = serde_json::json!({ "cursor": cursor });
            let value = self.rpc_call("resources/templates/list", params).await?;
            let result: ListResourceTemplatesResult =
                serde_json::from_value(value).map_err(|e| {
                    self.call_error(format!(
                        "failed to parse resources/templates/list result: {e}"
                    ))
                })?;
            all.extend(result.resource_templates);
            cursor = result.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(all)
    }

    async fn list_prompts(&self) -> Result<Vec<McpPrompt>, McpGatewayError> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = serde_json::json!({ "cursor": cursor });
            let value = self.rpc_call("prompts/list", params).await?;
            let result: ListPromptsResult = serde_json::from_value(value).map_err(|e| {
                self.call_error(format!("failed to parse prompts/list result: {e}"))
            })?;
            all.extend(result.prompts);
            cursor = result.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(all)
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<HashMap<String, String>>,
    ) -> Result<McpGetPromptResult, McpGatewayError> {
        let params = GetPromptParams {
            name: name.to_owned(),
            arguments,
        };
        let value = self
            .rpc_call(
                "prompts/get",
                serde_json::to_value(&params).map_err(|e| {
                    self.call_error(format!("failed to serialize prompts/get params: {e}"))
                })?,
            )
            .await?;
        let result: McpGetPromptResult = serde_json::from_value(value)
            .map_err(|e| self.call_error(format!("failed to parse prompts/get result: {e}")))?;
        Ok(result)
    }
}

/// Parse an SSE response body to extract the JSON-RPC response.
///
/// MCP Streamable HTTP servers may return `text/event-stream` for any
/// JSON-RPC request. The response is the last `data:` event containing
/// a valid `JsonRpcResponse`.
fn parse_sse_response(
    body: &str,
    name: &str,
    method: &str,
) -> Result<JsonRpcResponse, McpGatewayError> {
    let mut last_data = String::new();
    let mut current_data = String::new();

    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim_start();
            if !current_data.is_empty() {
                current_data.push('\n');
            }
            current_data.push_str(data);
        } else if line.is_empty() {
            // Empty line = event boundary.
            if !current_data.is_empty() {
                last_data = std::mem::take(&mut current_data);
            }
        }
        // Ignore `event:`, `id:`, `retry:` fields.
    }

    // Handle case where stream ends without trailing blank line.
    if !current_data.is_empty() {
        last_data = current_data;
    }

    if last_data.is_empty() {
        return Err(McpGatewayError::HttpTransport {
            name: name.to_owned(),
            reason: format!("SSE response for {method} contained no data events"),
        });
    }

    serde_json::from_str::<JsonRpcResponse>(&last_data).map_err(|e| {
        McpGatewayError::HttpTransport {
            name: name.to_owned(),
            reason: format!("failed to parse SSE data as JSON-RPC for {method}: {e}"),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sse_simple_json_response() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n";
        let resp = parse_sse_response(body, "test", "tools/list").expect("should parse");
        assert_eq!(resp.id, JsonRpcId::Number(1));
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn parse_sse_multiple_events_returns_last() {
        let body = "\
            data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\
            \n\
            data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\
            \n";
        let resp = parse_sse_response(body, "test", "test").expect("should parse");
        assert_eq!(resp.id, JsonRpcId::Number(1));
    }

    #[test]
    fn parse_sse_with_event_and_id_fields() {
        let body = "\
            event: message\n\
            id: evt-1\n\
            data: {\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{}}\n\
            \n";
        let resp = parse_sse_response(body, "test", "test").expect("should parse");
        assert_eq!(resp.id, JsonRpcId::Number(5));
    }

    #[test]
    fn parse_sse_no_trailing_newline() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}";
        let resp = parse_sse_response(body, "test", "test").expect("should parse");
        assert_eq!(resp.id, JsonRpcId::Number(1));
    }

    #[test]
    fn parse_sse_empty_body_errors() {
        let result = parse_sse_response("", "test", "test");
        assert!(result.is_err());
    }

    #[test]
    fn request_id_increments() {
        let id1 = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let id2 = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        assert!(id2 > id1);
    }
}
