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
use futures::stream::{self, BoxStream, StreamExt};
use rmcp::ServiceExt;
use rmcp::handler::client::ClientHandler;
use rmcp::handler::client::progress::ProgressDispatcher;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, ClientInfo, ClientRequest,
    CreateElicitationRequestParams, CreateElicitationResult, CreateMessageRequestParams,
    CreateMessageResult, ErrorCode, ErrorData as McpError, GetPromptRequestParams, Implementation,
    ListRootsResult, ProgressNotificationParam, ReadResourceRequestParams, ServerResult,
};
use rmcp::service::{
    NotificationContext, Peer, PeerRequestOptions, RequestContext, RoleClient, RunningService,
    ServiceError,
};
use tokio::sync::{Mutex, broadcast};

use super::transport::McpTransport;
use super::{
    Executor, InvalidationEvent, InvalidationKind, McpRequest, McpResponse, McpStreamPart,
    McpTarget,
};
use crate::error::{BitrouterError, Result};

/// [`ClientHandler`] for upstream MCP servers reached through [`RmcpExecutor`].
///
/// Holds a per-connection [`ProgressDispatcher`] so `execute_streaming` can
/// subscribe to `notifications/progress` and forward them as
/// [`McpStreamPart::Notification`]. Also forwards
/// `notifications/tools/list_changed` (and siblings) onto the shared
/// invalidation channel so a [`super::CachingExecutor`] can evict stale
/// entries promptly.
#[derive(Debug, Clone)]
struct BitrouterMcpClient {
    server_name: String,
    progress: Arc<ProgressDispatcher>,
    invalidation: Arc<broadcast::Sender<InvalidationEvent>>,
}

impl ClientHandler for BitrouterMcpClient {
    fn get_info(&self) -> ClientInfo {
        let mut info = ClientInfo::default();
        info.client_info = Implementation::new("bitrouter", env!("CARGO_PKG_VERSION"));
        info
    }

    fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let dispatcher = self.progress.clone();
        async move {
            dispatcher.handle_notification(params).await;
        }
    }

    fn on_tool_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let server_name = self.server_name.clone();
        let tx = self.invalidation.clone();
        async move {
            let _ = tx.send(InvalidationEvent {
                server_name,
                kind: InvalidationKind::ToolsListChanged,
            });
        }
    }

    fn on_resource_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let server_name = self.server_name.clone();
        let tx = self.invalidation.clone();
        async move {
            let _ = tx.send(InvalidationEvent {
                server_name,
                kind: InvalidationKind::ResourcesListChanged,
            });
        }
    }

    fn on_prompt_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let server_name = self.server_name.clone();
        let tx = self.invalidation.clone();
        async move {
            let _ = tx.send(InvalidationEvent {
                server_name,
                kind: InvalidationKind::PromptsListChanged,
            });
        }
    }

    // Server→client requests — `sampling/createMessage`, `elicitation/create`,
    // `roots/list`. The bitrouter gateway is stateless: the inbound client
    // connected via a single HTTP request, with no channel back through which
    // we could relay a server→client request. Rather than rmcp's silent
    // defaults (the spec's #1 silent-breakage complaint of MCP-through-gateway
    // in 2026), we surface an explicit, spec-shaped `-32601` so the upstream
    // server's tool-call logic sees the rejection and can branch on it.
    fn create_message(
        &self,
        _params: CreateMessageRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = std::result::Result<CreateMessageResult, McpError>> + Send + '_
    {
        let server = self.server_name.clone();
        async move {
            tracing::warn!(
                server = %server,
                method = "sampling/createMessage",
                "mcp gateway rejected server→client request (stateless inbound)",
            );
            Err(deny_error("sampling/createMessage"))
        }
    }

    fn create_elicitation(
        &self,
        _request: CreateElicitationRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = std::result::Result<CreateElicitationResult, McpError>>
    + Send
    + '_ {
        let server = self.server_name.clone();
        async move {
            tracing::warn!(
                server = %server,
                method = "elicitation/create",
                "mcp gateway rejected server→client request (stateless inbound)",
            );
            Err(deny_error("elicitation/create"))
        }
    }

    fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = std::result::Result<ListRootsResult, McpError>> + Send + '_
    {
        let server = self.server_name.clone();
        async move {
            tracing::warn!(
                server = %server,
                method = "roots/list",
                "mcp gateway rejected server→client request (stateless inbound)",
            );
            Err(deny_error("roots/list"))
        }
    }
}

fn deny_error(method: &'static str) -> McpError {
    McpError::new(
        ErrorCode::METHOD_NOT_FOUND,
        format!(
            "bitrouter gateway does not relay server→client requests. The inbound client \
             connected statelessly; configure a direct MCP client connection if you need \
             {method}."
        ),
        None,
    )
}

/// One pooled upstream connection — the running rmcp service plus the
/// progress dispatcher its client holds (cloned-Arc) so the executor can
/// subscribe per call.
#[derive(Clone)]
struct PooledConnection {
    service: Arc<RunningService<RoleClient, BitrouterMcpClient>>,
    progress: Arc<ProgressDispatcher>,
}

/// Pooled rmcp client used by [`RmcpExecutor`].
type Pool = Mutex<HashMap<String, PooledConnection>>;

/// Broadcast capacity for the invalidation channel. Sized to absorb a burst
/// of `notifications/*_list_changed` from a freshly reconnected server
/// without dropping events for the caching subscriber.
const INVALIDATION_CHANNEL_CAPACITY: usize = 256;

/// [`Executor`] that forwards [`McpRequest`]s to upstream MCP servers via
/// rmcp.
pub struct RmcpExecutor {
    pool: Pool,
    invalidation_tx: Arc<broadcast::Sender<InvalidationEvent>>,
}

impl Default for RmcpExecutor {
    fn default() -> Self {
        let (tx, _rx) = broadcast::channel(INVALIDATION_CHANNEL_CAPACITY);
        Self {
            pool: Default::default(),
            invalidation_tx: Arc::new(tx),
        }
    }
}

impl RmcpExecutor {
    /// Fresh executor with an empty connection pool.
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe to upstream cache-invalidation notifications. Each
    /// `notifications/tools/list_changed` (or sibling) from any pooled server
    /// produces one event on this channel — typically consumed by a
    /// [`super::caching_executor::CachingExecutor`].
    pub fn invalidation_receiver(&self) -> broadcast::Receiver<InvalidationEvent> {
        self.invalidation_tx.subscribe()
    }

    /// Drop the pooled connection for `server_name`, if any. The next request
    /// for that server will re-dial — running the MCP `initialize` handshake
    /// against the upstream again and (for HTTP transports) rebuilding the
    /// transport with whatever headers the new connect call supplies.
    ///
    /// The SDK does not interpret *when* to evict — that policy lives in a
    /// downstream decorator. The canonical use case is an OAuth-refresh
    /// decorator that calls `evict` after rotating an access token so the
    /// next request reconnects with the new `Authorization` header rather
    /// than the one baked into the pooled transport at first-connect time.
    ///
    /// In-flight calls that still hold an `Arc` to the dropped service
    /// continue to completion — eviction affects only *subsequent* lookups.
    ///
    /// Returns `true` if an entry was removed, `false` if the pool had no
    /// entry for `server_name`. Downstream decorators can use the return
    /// value to skip no-op telemetry / log noise.
    pub async fn evict(&self, server_name: &str) -> bool {
        self.pool.lock().await.remove(server_name).is_some()
    }

    async fn connection_for(
        &self,
        server_name: &str,
        transport: &McpTransport,
    ) -> Result<PooledConnection> {
        // Fast path: already connected.
        if let Some(existing) = self.pool.lock().await.get(server_name).cloned() {
            return Ok(existing);
        }
        // Slow path: dial. We drop the lock across the network round-trip so
        // a slow `initialize` against one server can't block lookups for
        // another. If two requests race to dial the same server, both will
        // dial; the second one's value silently replaces the first in the
        // pool — fine because either RunningService is correct.
        let progress = Arc::new(ProgressDispatcher::new());
        let client = BitrouterMcpClient {
            server_name: server_name.to_string(),
            progress: progress.clone(),
            invalidation: self.invalidation_tx.clone(),
        };
        let service = connect(server_name, transport, client).await?;
        let entry = PooledConnection {
            service: Arc::new(service),
            progress,
        };
        self.pool
            .lock()
            .await
            .insert(server_name.to_string(), entry.clone());
        Ok(entry)
    }
}

async fn connect(
    server_name: &str,
    transport: &McpTransport,
    client: BitrouterMcpClient,
) -> Result<RunningService<RoleClient, BitrouterMcpClient>> {
    match transport {
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
                        "mcp '{server_name}': invalid header name '{k}': {e}"
                    ))
                })?;
                let value: HeaderValue = v.parse().map_err(|e| {
                    BitrouterError::internal(format!(
                        "mcp '{server_name}': invalid header value for '{k}': {e}"
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
            client
                .serve(transport)
                .await
                .map_err(|e| map_initialize_error(server_name, e))
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
                .map_err(|e| upstream(server_name, format!("spawning '{command}': {e}")))?;
            client
                .serve(transport)
                .await
                .map_err(|e| map_initialize_error(server_name, e))
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

/// Inspect a transport error for an upstream auth challenge (rmcp 1.7
/// classifies these as typed variants). Returns a status-bearing
/// [`BitrouterError::UpstreamAuth`] for `401`/`403` so the cloud can drive
/// token refresh / OAuth step-up; `None` for any other transport error
/// (the caller then falls back to the generic 502 `upstream(...)`).
fn classify_transport_auth_error(
    dte: &rmcp::transport::DynamicTransportError,
) -> Option<BitrouterError> {
    use rmcp::transport::streamable_http_client::StreamableHttpError;
    // regression: rmcp uses reqwest 0.13; downcast must name that type
    // (rmcp_reqwest = reqwest 0.13). Our own client is reqwest 0.12 — a
    // distinct, separately-compiled crate with a different TypeId, so naming
    // bare `reqwest::Error` here would make this downcast silently never match.
    let err = dte
        .error
        .downcast_ref::<StreamableHttpError<rmcp_reqwest::Error>>()?;
    match err {
        StreamableHttpError::AuthRequired(e) => Some(BitrouterError::UpstreamAuth {
            status: 401,
            www_authenticate: Some(e.www_authenticate_header.clone()),
            required_scope: None,
        }),
        StreamableHttpError::InsufficientScope(e) => Some(BitrouterError::UpstreamAuth {
            status: 403,
            www_authenticate: Some(e.www_authenticate_header.clone()),
            required_scope: e.required_scope.clone(),
        }),
        _ => None,
    }
}

/// Map a `ServiceError` from a live MCP call to a `BitrouterError`, preferring
/// a typed `UpstreamAuth` when the transport error is an auth challenge, else
/// the generic 502.
fn map_service_error(
    server: &str,
    method: &str,
    err: rmcp::service::ServiceError,
) -> BitrouterError {
    if let rmcp::service::ServiceError::TransportSend(dte) = &err
        && let Some(auth) = classify_transport_auth_error(dte)
    {
        return auth;
    }
    upstream(server, format!("{method}: {err}"))
}

/// Map a connect/initialize failure to a `BitrouterError`, preferring a typed
/// `UpstreamAuth` when the underlying transport error is an auth challenge,
/// else the generic 502.
fn map_initialize_error(server: &str, err: rmcp::service::ClientInitializeError) -> BitrouterError {
    if let rmcp::service::ClientInitializeError::TransportError { error, .. } = &err
        && let Some(auth) = classify_transport_auth_error(error)
    {
        return auth;
    }
    upstream(server, format!("connect: {err}"))
}

#[async_trait]
impl Executor for RmcpExecutor {
    async fn execute(&self, target: &McpTarget, request: &McpRequest) -> Result<McpResponse> {
        let (server_name, transport) = direct_target(target)?;
        let conn = self.connection_for(server_name, transport).await?;
        let peer = conn.service.peer().clone();
        let result = dispatch(&peer, server_name, request).await?;
        Ok(McpResponse {
            request_id: request.request_id.clone(),
            result,
        })
    }

    async fn execute_streaming(
        &self,
        target: &McpTarget,
        request: &McpRequest,
    ) -> Result<BoxStream<'static, Result<McpStreamPart>>> {
        // Streaming is only meaningful for `tools/call`: every other dispatched
        // method is a one-shot list/read with no upstream notifications, so
        // the default impl (wrap `execute` as a single `Final`) covers them.
        if request.method != "tools/call" {
            let response = self.execute(target, request).await?;
            return Ok(stream::once(async move { Ok(McpStreamPart::Final(response)) }).boxed());
        }
        let (server_name, transport) = direct_target(target)?;
        let conn = self.connection_for(server_name, transport).await?;
        Ok(stream_tools_call(conn, server_name.to_string(), request.clone()).boxed())
    }
}

fn direct_target(target: &McpTarget) -> Result<(&str, &McpTransport)> {
    match target {
        McpTarget::Direct {
            server_name,
            transport,
        } => Ok((server_name.as_str(), transport)),
        McpTarget::Aggregate { .. } => Err(BitrouterError::internal(
            "RmcpExecutor cannot handle Aggregate targets directly — wrap it in an \
             AggregatingExecutor",
        )),
    }
}

/// Drive `tools/call` with a parallel progress-notification stream.
///
/// rmcp's `Peer::call_tool` shorthand goes through `send_request`, which
/// **unconditionally overwrites** `_meta.progressToken` with the service's own
/// provider value (see `service.rs::send_request_with_option`). That means
/// any token we inject at the params layer is silently clobbered before the
/// request hits the wire. We use `send_cancellable_request` instead so the
/// returned `RequestHandle` tells us the token rmcp actually chose, then
/// subscribe to it on the [`ProgressDispatcher`] before awaiting the response.
fn stream_tools_call(
    conn: PooledConnection,
    server_name: String,
    request: McpRequest,
) -> impl futures::Stream<Item = Result<McpStreamPart>> + Send + 'static {
    async_stream::stream! {
        let call_params: CallToolRequestParams =
            match serde_json::from_value(request.params.clone()) {
                Ok(p) => p,
                Err(e) => {
                    yield Err(bad_params(&server_name, "tools/call", e));
                    return;
                }
            };
        let call_request = ClientRequest::CallToolRequest(CallToolRequest::new(call_params));

        let peer = conn.service.peer().clone();
        let handle = match peer
            .send_cancellable_request(call_request, PeerRequestOptions::no_options())
            .await
        {
            Ok(h) => h,
            Err(e) => {
                yield Err(upstream(&server_name, format!("tools/call: {e}")));
                return;
            }
        };
        let mut subscriber = Some(conn.progress.subscribe(handle.progress_token.clone()).await);
        let request_id = request.request_id.clone();
        let server = server_name.clone();
        let call_fut = async move {
            handle.await_response().await.map_err(|e| match e {
                ServiceError::McpError(err) => upstream(&server, format!("tools/call: {err}")),
                other => upstream(&server, format!("tools/call: {other}")),
            })
        };
        tokio::pin!(call_fut);

        loop {
            // The progress arm awaits `pending()` once the subscriber has
            // closed — that future is `Poll::Pending` forever, so the `biased`
            // select stops hot-polling this branch and waits cleanly for
            // `call_fut` to resolve. (A bare `subscriber.next().await` would
            // resolve to `None` instantly each tick and burn CPU.)
            let next_notif = async {
                match subscriber.as_mut() {
                    Some(s) => s.next().await,
                    None => std::future::pending::<Option<_>>().await,
                }
            };
            tokio::select! {
                biased;
                notif = next_notif => {
                    match notif {
                        Some(n) => {
                            let params = serde_json::to_value(&n).unwrap_or(serde_json::Value::Null);
                            yield Ok(McpStreamPart::Notification {
                                method: "notifications/progress".to_string(),
                                params,
                            });
                        }
                        None => {
                            // Subscriber closed before the call returned —
                            // drop it so subsequent iterations sit on
                            // `pending()` instead of re-polling a closed stream.
                            subscriber = None;
                        }
                    }
                }
                call_result = &mut call_fut => {
                    match call_result {
                        Ok(ServerResult::CallToolResult(result)) => {
                            let value = serde_json::to_value(&result).unwrap_or(serde_json::Value::Null);
                            yield Ok(McpStreamPart::Final(McpResponse {
                                request_id,
                                result: value,
                            }));
                        }
                        Ok(other) => {
                            yield Err(upstream(
                                &server_name,
                                format!("tools/call: unexpected server result {other:?}"),
                            ));
                        }
                        Err(e) => yield Err(e),
                    }
                    return;
                }
            }
        }
    }
}

async fn dispatch(
    peer: &Peer<RoleClient>,
    server: &str,
    request: &McpRequest,
) -> Result<serde_json::Value> {
    let method = request.method.as_str();
    match method {
        "tools/list" => {
            let tools = peer
                .list_all_tools()
                .await
                .map_err(|e| map_service_error(server, method, e))?;
            Ok(serde_json::json!({ "tools": tools }))
        }
        "tools/call" => {
            let params: CallToolRequestParams = serde_json::from_value(request.params.clone())
                .map_err(|e| bad_params(server, method, e))?;
            let result = peer
                .call_tool(params)
                .await
                .map_err(|e| map_service_error(server, method, e))?;
            serde_json::to_value(&result).map_err(|e| {
                BitrouterError::internal(format!("mcp '{server}' tools/call serialise: {e}"))
            })
        }
        "resources/list" => {
            let resources = peer
                .list_all_resources()
                .await
                .map_err(|e| map_service_error(server, method, e))?;
            Ok(serde_json::json!({ "resources": resources }))
        }
        "resources/read" => {
            let params: ReadResourceRequestParams = serde_json::from_value(request.params.clone())
                .map_err(|e| bad_params(server, method, e))?;
            let result = peer
                .read_resource(params)
                .await
                .map_err(|e| map_service_error(server, method, e))?;
            serde_json::to_value(&result).map_err(|e| {
                BitrouterError::internal(format!("mcp '{server}' resources/read serialise: {e}"))
            })
        }
        "resources/templates/list" => {
            let templates = peer
                .list_all_resource_templates()
                .await
                .map_err(|e| map_service_error(server, method, e))?;
            Ok(serde_json::json!({ "resourceTemplates": templates }))
        }
        "prompts/list" => {
            let prompts = peer
                .list_all_prompts()
                .await
                .map_err(|e| map_service_error(server, method, e))?;
            Ok(serde_json::json!({ "prompts": prompts }))
        }
        "prompts/get" => {
            let params: GetPromptRequestParams = serde_json::from_value(request.params.clone())
                .map_err(|e| bad_params(server, method, e))?;
            let result = peer
                .get_prompt(params)
                .await
                .map_err(|e| map_service_error(server, method, e))?;
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
    use crate::caller::CallerContext;
    use std::collections::HashMap;

    fn req(server: &str, method: &str) -> McpRequest {
        McpRequest::direct(
            server,
            method,
            serde_json::json!({}),
            CallerContext::new("k", "u"),
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
        let target = McpTarget::Direct {
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
        let target = McpTarget::Direct {
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

    /// `evict` on an empty pool returns `false` and is idempotent. The
    /// populated-path behaviour (entry removed, next request re-dials) requires
    /// a real upstream `RunningService` and is covered by integration tests in
    /// downstream consumers; the unit surface only asserts the public contract.
    #[tokio::test]
    async fn evict_on_empty_pool_returns_false_and_is_idempotent() {
        let exec = RmcpExecutor::new();
        assert!(!exec.evict("never-connected").await);
        assert!(!exec.evict("never-connected").await);
    }

    #[tokio::test]
    async fn executor_rejects_aggregate_targets() {
        let exec = RmcpExecutor::new();
        let target = McpTarget::Aggregate { members: vec![] };
        let err = exec
            .execute(&target, &req("anything", "tools/list"))
            .await
            .unwrap_err();
        // Internal — RmcpExecutor without an AggregatingExecutor wrapper is
        // a programming bug, not a transport failure.
        assert_eq!(err.status(), 500, "unexpected error: {err}");
    }

    #[test]
    fn classifies_insufficient_scope_transport_error() {
        use rmcp::transport::DynamicTransportError;
        use rmcp::transport::streamable_http_client::{
            InsufficientScopeError, StreamableHttpError,
        };

        let inner: StreamableHttpError<rmcp_reqwest::Error> =
            StreamableHttpError::InsufficientScope(InsufficientScopeError::new(
                "Bearer error=\"insufficient_scope\", scope=\"read:files\"".to_string(),
                Some("read:files".to_string()),
            ));
        let dte = DynamicTransportError::from_parts(
            "test",
            std::any::TypeId::of::<()>(),
            Box::new(inner),
        );

        let classified = classify_transport_auth_error(&dte).expect("should classify");
        match classified {
            BitrouterError::UpstreamAuth {
                status,
                required_scope,
                ..
            } => {
                assert_eq!(status, 403);
                assert_eq!(required_scope.as_deref(), Some("read:files"));
            }
            other => panic!("expected UpstreamAuth, got {other:?}"),
        }
    }

    #[test]
    fn classifies_auth_required_transport_error() {
        use rmcp::transport::DynamicTransportError;
        use rmcp::transport::streamable_http_client::{AuthRequiredError, StreamableHttpError};

        let inner: StreamableHttpError<rmcp_reqwest::Error> =
            StreamableHttpError::AuthRequired(AuthRequiredError::new("Bearer".to_string()));
        let dte = DynamicTransportError::from_parts(
            "test",
            std::any::TypeId::of::<()>(),
            Box::new(inner),
        );
        let classified = classify_transport_auth_error(&dte).expect("should classify");
        assert!(matches!(
            classified,
            BitrouterError::UpstreamAuth { status: 401, .. }
        ));
    }

    #[test]
    fn non_auth_transport_error_is_not_classified() {
        use rmcp::transport::DynamicTransportError;
        use rmcp::transport::streamable_http_client::StreamableHttpError;
        let inner: StreamableHttpError<rmcp_reqwest::Error> =
            StreamableHttpError::UnexpectedEndOfStream;
        let dte = DynamicTransportError::from_parts(
            "test",
            std::any::TypeId::of::<()>(),
            Box::new(inner),
        );
        assert!(classify_transport_auth_error(&dte).is_none());
    }

    #[test]
    fn service_transport_send_auth_maps_to_upstream_auth() {
        use rmcp::service::ServiceError;
        use rmcp::transport::DynamicTransportError;
        use rmcp::transport::streamable_http_client::{
            InsufficientScopeError, StreamableHttpError,
        };

        let inner: StreamableHttpError<rmcp_reqwest::Error> =
            StreamableHttpError::InsufficientScope(InsufficientScopeError::new(
                "Bearer error=\"insufficient_scope\", scope=\"x\"".into(),
                Some("x".into()),
            ));
        let svc_err = ServiceError::TransportSend(DynamicTransportError::from_parts(
            "test",
            std::any::TypeId::of::<()>(),
            Box::new(inner),
        ));
        let mapped = map_service_error("srv", "tools/call", svc_err);
        assert!(matches!(
            mapped,
            BitrouterError::UpstreamAuth {
                status: 403,
                required_scope: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn initialize_transport_auth_maps_to_upstream_auth() {
        use rmcp::service::ClientInitializeError;
        use rmcp::transport::DynamicTransportError;
        use rmcp::transport::streamable_http_client::{
            InsufficientScopeError, StreamableHttpError,
        };

        let inner: StreamableHttpError<rmcp_reqwest::Error> =
            StreamableHttpError::InsufficientScope(InsufficientScopeError::new(
                "Bearer error=\"insufficient_scope\", scope=\"a\"".into(),
                Some("a".into()),
            ));
        let init_err = ClientInitializeError::TransportError {
            error: DynamicTransportError::from_parts(
                "test",
                std::any::TypeId::of::<()>(),
                Box::new(inner),
            ),
            context: "send initialize request".into(),
        };
        let mapped = map_initialize_error("srv", init_err);
        assert!(matches!(
            mapped,
            BitrouterError::UpstreamAuth { status: 403, .. }
        ));
    }
}
