//! MCP Streamable HTTP client.
//!
//! Speaks JSON-RPC 2.0 over the MCP Streamable HTTP transport to any
//! MCP-compliant server. Handles session management, content-type
//! negotiation (JSON vs SSE responses), cursor-based pagination, and
//! server-to-client request dispatch (sampling, elicitation).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use tokio::sync::{Mutex, Notify, RwLock, oneshot};

use bitrouter_core::api::mcp::gateway::McpClientRequestHandler;
use bitrouter_core::api::mcp::types::McpGatewayError;
use bitrouter_core::api::mcp::types::{
    CallToolParams, CallToolResult, ClientCapabilities, ClientInfo, CreateMessageParams,
    ElicitationCapability, ElicitationCreateParams, GetPromptParams, InitializeParams,
    InitializeResult, JsonRpcId, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult, ListToolsParams,
    ListToolsResult, McpGetPromptResult, McpPrompt, McpResource, McpResourceContent,
    McpResourceTemplate, McpTool, McpToolCallResult, ReadResourceParams, ReadResourceResult,
    SamplingCapability, SseJsonRpcMessage,
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

/// Notification handles for signaling list changes to the upstream connection.
pub struct NotifyHandles {
    pub tool: Arc<Notify>,
    pub resource: Arc<Notify>,
    pub prompt: Arc<Notify>,
}

/// MCP Streamable HTTP client.
///
/// Connects to an upstream MCP server over HTTP, implementing the
/// [Streamable HTTP transport](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports#streamable-http).
///
/// Supports both client-to-server requests and server-to-client requests
/// (sampling, elicitation) via SSE streams.
pub struct McpHttpClient {
    http: reqwest::Client,
    url: String,
    name: String,
    session: Arc<RwLock<McpSession>>,
    /// Handler for server→client requests (sampling, elicitation).
    handler: Option<Arc<dyn McpClientRequestHandler>>,
    /// Pending request map: maps request IDs to oneshot senders waiting for the response.
    pending: Arc<RwLock<HashMap<JsonRpcId, oneshot::Sender<JsonRpcResponse>>>>,
    /// Notification handles for tool/resource/prompt list changes.
    notify: Option<NotifyHandles>,
    /// Background SSE listener task handle.
    sse_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl McpHttpClient {
    /// Build a new client for the given MCP endpoint URL.
    ///
    /// Custom headers (e.g. `Authorization`) are set as default headers on
    /// the underlying reqwest client. The optional `handler` enables
    /// server-to-client request dispatch (sampling/elicitation).
    pub fn new(
        name: impl Into<String>,
        url: impl Into<String>,
        headers: &HashMap<String, String>,
        handler: Option<Arc<dyn McpClientRequestHandler>>,
        notify: Option<NotifyHandles>,
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
            session: Arc::new(RwLock::new(McpSession {
                session_id: None,
                protocol_version: None,
            })),
            handler,
            pending: Arc::new(RwLock::new(HashMap::new())),
            notify,
            sse_task: Mutex::new(None),
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

        // Register a pending receiver so the background SSE listener can
        // deliver the response if it arrives on the SSE stream.
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.write().await;
            pending.insert(id.clone(), tx);
        }

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
            self.remove_pending(&id).await;
            return Err(McpGatewayError::SessionExpired {
                name: self.name.clone(),
            });
        }
        if !status.is_success() {
            self.remove_pending(&id).await;
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

        if content_type.contains("text/event-stream") {
            // SSE response — process all events. The response to our request
            // may be interspersed with server→client requests and notifications.
            let body = match response.text().await {
                Ok(b) => b,
                Err(e) => {
                    self.remove_pending(&id).await;
                    return Err(McpGatewayError::HttpTransport {
                        name: self.name.clone(),
                        reason: format!("failed to read SSE body for {method}: {e}"),
                    });
                }
            };
            self.process_sse_events(&body).await;
        } else {
            // Plain JSON response — resolve the pending request directly.
            let rpc_response = match response.json::<JsonRpcResponse>().await {
                Ok(r) => r,
                Err(e) => {
                    self.remove_pending(&id).await;
                    return Err(McpGatewayError::HttpTransport {
                        name: self.name.clone(),
                        reason: format!("failed to parse JSON response for {method}: {e}"),
                    });
                }
            };
            self.resolve_pending(rpc_response).await;
        }

        // Wait for the response via the oneshot channel.
        let rpc_response = rx.await.map_err(|_| McpGatewayError::HttpTransport {
            name: self.name.clone(),
            reason: format!("{method} response channel closed (server may have disconnected)"),
        })?;

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

    // ── SSE processing ────────────────────────────────────────────

    /// Process all SSE events from a response body.
    ///
    /// Each event is parsed as an [`SseJsonRpcMessage`] and dispatched:
    /// - Response → resolve the matching pending request
    /// - Request → dispatch to handler, POST response back
    /// - Notification → fire the matching notify handle
    async fn process_sse_events(&self, body: &str) {
        for event_data in parse_sse_events(body) {
            match serde_json::from_str::<SseJsonRpcMessage>(&event_data) {
                Ok(msg) => self.dispatch_sse_message(msg).await,
                Err(e) => {
                    tracing::warn!(
                        upstream = %self.name,
                        error = %e,
                        "failed to parse SSE event"
                    );
                }
            }
        }
    }

    /// Dispatch a single SSE message to the appropriate handler.
    async fn dispatch_sse_message(&self, msg: SseJsonRpcMessage) {
        match msg {
            SseJsonRpcMessage::Response(resp) => {
                self.resolve_pending(resp).await;
            }
            SseJsonRpcMessage::Request(req) => {
                let response =
                    dispatch_server_request(&self.name, self.handler.as_deref(), req).await;
                self.post_json_rpc_response(&response).await;
            }
            SseJsonRpcMessage::Notification(notif) => {
                self.handle_notification(&notif);
            }
        }
    }

    /// Resolve a pending request by sending the response through its oneshot channel.
    async fn resolve_pending(&self, response: JsonRpcResponse) {
        let mut pending = self.pending.write().await;
        if let Some(tx) = pending.remove(&response.id) {
            let _ = tx.send(response);
        } else {
            tracing::debug!(
                upstream = %self.name,
                id = ?response.id,
                "received response for unknown request ID"
            );
        }
    }

    /// Remove a pending request entry (e.g. on HTTP error before the oneshot is used).
    async fn remove_pending(&self, id: &JsonRpcId) {
        let mut pending = self.pending.write().await;
        pending.remove(id);
    }

    /// Handle a notification from the server.
    fn handle_notification(&self, notif: &JsonRpcNotification) {
        if let Some(ref handles) = self.notify {
            match notif.method.as_str() {
                "notifications/tools/list_changed" => handles.tool.notify_one(),
                "notifications/resources/list_changed" => handles.resource.notify_one(),
                "notifications/prompts/list_changed" => handles.prompt.notify_one(),
                _ => {
                    tracing::trace!(
                        upstream = %self.name,
                        method = %notif.method,
                        "ignoring unhandled notification"
                    );
                }
            }
        }
    }

    /// POST a JSON-RPC response back to the server (for server→client requests).
    async fn post_json_rpc_response(&self, response: &JsonRpcResponse) {
        let session = self.session.read().await;
        let mut builder = self
            .http
            .post(&self.url)
            .header("Content-Type", "application/json");

        if let Some(ref sid) = session.session_id {
            builder = builder.header("Mcp-Session-Id", sid);
        }
        if let Some(ref version) = session.protocol_version {
            builder = builder.header("MCP-Protocol-Version", version);
        }
        drop(session);

        match builder.json(response).send().await {
            Ok(resp) if resp.status().is_success() || resp.status().as_u16() == 202 => {}
            Ok(resp) => {
                tracing::warn!(
                    upstream = %self.name,
                    status = %resp.status(),
                    "unexpected status when posting response to server"
                );
            }
            Err(e) => {
                tracing::warn!(
                    upstream = %self.name,
                    error = %e,
                    "failed to post response to server"
                );
            }
        }
    }

    // ── Background SSE listener ───────────────────────────────────

    /// Spawn a background task that opens a persistent GET SSE stream
    /// to receive server-initiated messages.
    async fn spawn_sse_listener(&self) {
        let session = self.session.read().await;
        let Some(ref session_id) = session.session_id else {
            // Stateless server — no session ID means no persistent SSE stream.
            return;
        };

        let http = self.http.clone();
        let url = self.url.clone();
        let session_id = session_id.clone();
        let protocol_version = session.protocol_version.clone();
        drop(session);

        let ctx = SseListenerContext {
            name: self.name.clone(),
            pending: Arc::clone(&self.pending),
            handler: self.handler.clone(),
            notify: self.notify.as_ref().map(|h| NotifyHandles {
                tool: Arc::clone(&h.tool),
                resource: Arc::clone(&h.resource),
                prompt: Arc::clone(&h.prompt),
            }),
            http: self.http.clone(),
            url: self.url.clone(),
            session: Arc::clone(&self.session),
        };

        let handle = tokio::spawn(async move {
            let mut builder = http
                .get(&url)
                .header("Accept", "text/event-stream")
                .header("Mcp-Session-Id", &session_id);

            if let Some(ref version) = protocol_version {
                builder = builder.header("MCP-Protocol-Version", version);
            }

            let response = match builder.send().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!(
                        upstream = %ctx.name,
                        error = %e,
                        "failed to open SSE stream (server may not support GET streaming)"
                    );
                    return;
                }
            };

            if !response.status().is_success() {
                tracing::debug!(
                    upstream = %ctx.name,
                    status = %response.status(),
                    "SSE GET stream returned non-success status"
                );
                return;
            }

            // Stream the response body incrementally, parsing SSE events
            // as they arrive rather than buffering the entire stream.
            use tokio_stream::StreamExt;

            let mut byte_stream = response.bytes_stream();
            let mut current_data = String::new();
            let mut leftover = String::new();

            loop {
                let chunk = byte_stream.next().await;

                let bytes = match chunk {
                    Some(Ok(b)) => b,
                    Some(Err(e)) => {
                        tracing::debug!(
                            upstream = %ctx.name,
                            error = %e,
                            "SSE stream read error"
                        );
                        break;
                    }
                    None => {
                        // Stream ended — process any remaining buffered data.
                        if !current_data.is_empty()
                            && let Ok(msg) =
                                serde_json::from_str::<SseJsonRpcMessage>(&current_data)
                        {
                            ctx.dispatch(msg).await;
                        }
                        break;
                    }
                };

                let text = match std::str::from_utf8(&bytes) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                leftover.push_str(text);

                while let Some(newline_pos) = leftover.find('\n') {
                    let line: String = leftover[..newline_pos].trim_end_matches('\r').to_owned();
                    leftover = leftover[newline_pos + 1..].to_owned();

                    if let Some(data) = line.strip_prefix("data:") {
                        let data = data.trim_start();
                        if !current_data.is_empty() {
                            current_data.push('\n');
                        }
                        current_data.push_str(data);
                    } else if line.is_empty() && !current_data.is_empty() {
                        let event_data = std::mem::take(&mut current_data);
                        match serde_json::from_str::<SseJsonRpcMessage>(&event_data) {
                            Ok(msg) => ctx.dispatch(msg).await,
                            Err(e) => {
                                tracing::warn!(
                                    upstream = %ctx.name,
                                    error = %e,
                                    "failed to parse SSE event from GET stream"
                                );
                            }
                        }
                    }
                }
            }
        });

        let mut task = self.sse_task.lock().await;
        *task = Some(handle);
    }
}

/// Dispatch a server→client request to the handler and build a response.
///
/// Extracted as a free function so it can be called from both the
/// `McpHttpClient` methods and the background SSE listener task.
async fn dispatch_server_request(
    server_name: &str,
    handler: Option<&dyn McpClientRequestHandler>,
    request: JsonRpcRequest,
) -> JsonRpcResponse {
    let Some(handler) = handler else {
        return JsonRpcResponse::error(
            request.id,
            -32601,
            "server→client requests are not supported".to_owned(),
            None,
        );
    };

    let params_value = request.params.unwrap_or(serde_json::Value::Null);

    match request.method.as_str() {
        "sampling/createMessage" => {
            match serde_json::from_value::<CreateMessageParams>(params_value) {
                Ok(params) => match handler.handle_sampling(server_name, params).await {
                    Ok(result) => match serde_json::to_value(&result) {
                        Ok(v) => JsonRpcResponse::success(request.id, v),
                        Err(e) => JsonRpcResponse::error(
                            request.id,
                            -32603,
                            format!("failed to serialize sampling result: {e}"),
                            None,
                        ),
                    },
                    Err(e) => JsonRpcResponse::error(request.id, e.code, e.message, e.data),
                },
                Err(e) => JsonRpcResponse::error(
                    request.id,
                    -32602,
                    format!("invalid sampling/createMessage params: {e}"),
                    None,
                ),
            }
        }
        "elicitation/create" => {
            match serde_json::from_value::<ElicitationCreateParams>(params_value) {
                Ok(params) => match handler.handle_elicitation(server_name, params).await {
                    Ok(result) => match serde_json::to_value(&result) {
                        Ok(v) => JsonRpcResponse::success(request.id, v),
                        Err(e) => JsonRpcResponse::error(
                            request.id,
                            -32603,
                            format!("failed to serialize elicitation result: {e}"),
                            None,
                        ),
                    },
                    Err(e) => JsonRpcResponse::error(request.id, e.code, e.message, e.data),
                },
                Err(e) => JsonRpcResponse::error(
                    request.id,
                    -32602,
                    format!("invalid elicitation/create params: {e}"),
                    None,
                ),
            }
        }
        other => {
            JsonRpcResponse::error(request.id, -32601, format!("unknown method: {other}"), None)
        }
    }
}

/// POST a JSON-RPC response back to the server from the background task context.
async fn post_response_to_server(
    http: &reqwest::Client,
    url: &str,
    session: &RwLock<McpSession>,
    name: &str,
    response: &JsonRpcResponse,
) {
    let session = session.read().await;
    let mut builder = http.post(url).header("Content-Type", "application/json");

    if let Some(ref sid) = session.session_id {
        builder = builder.header("Mcp-Session-Id", sid);
    }
    if let Some(ref version) = session.protocol_version {
        builder = builder.header("MCP-Protocol-Version", version);
    }
    drop(session);

    match builder.json(response).send().await {
        Ok(resp) if resp.status().is_success() || resp.status().as_u16() == 202 => {}
        Ok(resp) => {
            tracing::warn!(
                upstream = %name,
                status = %resp.status(),
                "unexpected status when posting response to server"
            );
        }
        Err(e) => {
            tracing::warn!(
                upstream = %name,
                error = %e,
                "failed to post response to server"
            );
        }
    }
}

/// Shared context for the background SSE listener task.
struct SseListenerContext {
    name: String,
    pending: Arc<RwLock<HashMap<JsonRpcId, oneshot::Sender<JsonRpcResponse>>>>,
    handler: Option<Arc<dyn McpClientRequestHandler>>,
    notify: Option<NotifyHandles>,
    http: reqwest::Client,
    url: String,
    session: Arc<RwLock<McpSession>>,
}

impl SseListenerContext {
    /// Dispatch a single SSE message: resolve pending requests, handle
    /// server→client requests, or fire notification handles.
    async fn dispatch(&self, msg: SseJsonRpcMessage) {
        match msg {
            SseJsonRpcMessage::Response(resp) => {
                let mut p = self.pending.write().await;
                if let Some(tx) = p.remove(&resp.id) {
                    let _ = tx.send(resp);
                }
            }
            SseJsonRpcMessage::Request(req) => {
                let response =
                    dispatch_server_request(&self.name, self.handler.as_deref(), req).await;
                post_response_to_server(
                    &self.http,
                    &self.url,
                    &self.session,
                    &self.name,
                    &response,
                )
                .await;
            }
            SseJsonRpcMessage::Notification(notif) => {
                if let Some(handles) = &self.notify {
                    match notif.method.as_str() {
                        "notifications/tools/list_changed" => handles.tool.notify_one(),
                        "notifications/resources/list_changed" => handles.resource.notify_one(),
                        "notifications/prompts/list_changed" => handles.prompt.notify_one(),
                        _ => {}
                    }
                }
            }
        }
    }
}

// ── McpTransport impl ────────────────────────────────────────

impl super::McpTransport for McpHttpClient {
    async fn initialize(&self) -> Result<InitializeResult, McpGatewayError> {
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION.to_owned(),
            capabilities: ClientCapabilities {
                sampling: self.handler.as_ref().map(|_| SamplingCapability::default()),
                elicitation: self
                    .handler
                    .as_ref()
                    .map(|_| ElicitationCapability::default()),
            },
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

        // Spawn background SSE listener for server-initiated messages.
        self.spawn_sse_listener().await;

        Ok(init_result)
    }

    async fn terminate(&self) {
        // Abort the background SSE listener if running.
        {
            let mut task = self.sse_task.lock().await;
            if let Some(handle) = task.take() {
                handle.abort();
            }
        }

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

/// Parse an SSE body into individual event data strings.
///
/// Yields the `data:` payload of each complete SSE event.
fn parse_sse_events(body: &str) -> Vec<String> {
    let mut events = Vec::new();
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
                events.push(std::mem::take(&mut current_data));
            }
        }
        // Ignore `event:`, `id:`, `retry:` fields.
    }

    // Handle case where stream ends without trailing blank line.
    if !current_data.is_empty() {
        events.push(current_data);
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sse_simple_json_response() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n";
        let events = parse_sse_events(body);
        assert_eq!(events.len(), 1);
        let resp: JsonRpcResponse =
            serde_json::from_str(&events[0]).expect("should parse as response");
        assert_eq!(resp.id, JsonRpcId::Number(1));
        assert!(resp.result.is_some());
    }

    #[test]
    fn parse_sse_multiple_events() {
        let body = "\
            data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\
            \n\
            data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\
            \n";
        let events = parse_sse_events(body);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn parse_sse_with_event_and_id_fields() {
        let body = "\
            event: message\n\
            id: evt-1\n\
            data: {\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{}}\n\
            \n";
        let events = parse_sse_events(body);
        assert_eq!(events.len(), 1);
        let resp: JsonRpcResponse =
            serde_json::from_str(&events[0]).expect("should parse as response");
        assert_eq!(resp.id, JsonRpcId::Number(5));
    }

    #[test]
    fn parse_sse_no_trailing_newline() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}";
        let events = parse_sse_events(body);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn parse_sse_empty_body() {
        let events = parse_sse_events("");
        assert!(events.is_empty());
    }

    #[test]
    fn request_id_increments() {
        let id1 = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let id2 = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        assert!(id2 > id1);
    }

    #[test]
    fn sse_message_discriminates_response() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        let msg: SseJsonRpcMessage = serde_json::from_str(json).expect("parse");
        assert!(matches!(msg, SseJsonRpcMessage::Response(_)));
    }

    #[test]
    fn sse_message_discriminates_request() {
        let json = r#"{"jsonrpc":"2.0","id":2,"method":"sampling/createMessage","params":{"messages":[],"maxTokens":100}}"#;
        let msg: SseJsonRpcMessage = serde_json::from_str(json).expect("parse");
        assert!(matches!(msg, SseJsonRpcMessage::Request(_)));
    }

    #[test]
    fn sse_message_discriminates_notification() {
        let json = r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#;
        let msg: SseJsonRpcMessage = serde_json::from_str(json).expect("parse");
        assert!(matches!(msg, SseJsonRpcMessage::Notification(_)));
    }
}
