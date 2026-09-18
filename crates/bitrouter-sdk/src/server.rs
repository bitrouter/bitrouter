//! axum HTTP server — gated behind the `server` feature.
//!
//! Wires all four inbound protocols to the `language_model` pipeline:
//! - `POST /v1/messages` — Messages
//! - `POST /v1/chat/completions` — Chat Completions
//! - `POST /v1/responses` — Responses
//! - `POST /v1beta/models/{*model_action}` — Google `generateContent` /
//!   `streamGenerateContent`
//!
//! Each handler parses the inbound body with that protocol's adapter, runs the
//! pipeline, and renders the result back in the **same** inbound protocol —
//! the outbound (provider) protocol is chosen per routing target.

use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, serve};
use futures::StreamExt;

use crate::app::App;
use crate::caller::CallerContext;
use crate::error::{BitrouterError, Result};
use crate::language_model::Pipeline;
use crate::language_model::protocol::responses::encode_gateway_continuation_id;
use crate::language_model::protocol::{inbound_adapter_for, sanitize_model_name};
use crate::language_model::stream::{SseFrame, SseKeepaliveStream};
use crate::language_model::types::{ApiProtocol, PipelineRequest};
use crate::mcp;
use crate::metrics::MetricsRenderer;

const BITROUTER_REQUEST_ID_HEADER: &str = "x-bitrouter-request-id";
const REQUIRED_SHUTDOWN_RETRY_DELAY: Duration = Duration::from_millis(250);

/// Shared axum state.
#[derive(Clone)]
pub struct AppState {
    /// The `language_model` pipeline.
    pub language_model: Arc<Pipeline>,
    /// Optional `mcp` pipeline — `POST /mcp/{name}` is mounted only when set.
    pub mcp: Option<Arc<mcp::Pipeline>>,
    /// SDK-level `skip_auth`: when `true`, a credential-less request is given a
    /// synthesised local caller; otherwise a pre-auth anonymous placeholder
    /// (an `AuthHook` is then expected to validate / reject).
    pub skip_auth: bool,
    /// Optional Prometheus-style metrics renderer; `GET /metrics` reads this.
    pub metrics_renderer: Option<Arc<dyn MetricsRenderer>>,
    /// Ingress-time prompt transforms, applied in order after protocol parsing
    /// and before a request enters the pipeline (e.g. the `bitrouter/fusion`
    /// model alias).
    pub prompt_transforms: Vec<Arc<dyn crate::app::PromptTransform>>,
}

impl App {
    /// Serve this app's HTTP API on `listen` (e.g. `"0.0.0.0:4356"`).
    pub async fn serve(&self, listen: &str) -> Result<()> {
        self.serve_inner(listen, None, shutdown_signal()).await
    }

    /// Like [`App::serve`], but with a host-supplied router wrapper applied
    /// after the SDK has mounted every route — used by
    /// `bitrouter_telemetry::otel::http_layer` to open an OpenTelemetry SERVER
    /// span at HTTP ingress. It is a genuine extension point rather than an
    /// internal call: the renderer that uses it ships in another crate.
    pub async fn serve_with_router_wrapper<F>(&self, listen: &str, wrapper: F) -> Result<()>
    where
        F: Fn(Router) -> Router + Send + Sync + 'static,
    {
        self.serve_inner(listen, Some(Arc::new(wrapper)), shutdown_signal())
            .await
    }

    /// Serve until a host-owned shutdown future resolves.
    ///
    /// This additive entry point lets an embedding process coordinate other
    /// listeners without racing or dropping the HTTP server's graceful drain.
    pub async fn serve_with_shutdown<S>(&self, listen: &str, shutdown: S) -> Result<()>
    where
        S: Future<Output = ()> + Send + 'static,
    {
        self.serve_inner(listen, None, shutdown).await
    }

    /// Like [`Self::serve_with_shutdown`], with a host router wrapper.
    pub async fn serve_with_router_wrapper_and_shutdown<F, S>(
        &self,
        listen: &str,
        wrapper: F,
        shutdown: S,
    ) -> Result<()>
    where
        F: Fn(Router) -> Router + Send + Sync + 'static,
        S: Future<Output = ()> + Send + 'static,
    {
        self.serve_inner(listen, Some(Arc::new(wrapper)), shutdown)
            .await
    }

    /// Serve an already-bound listener with the normal router and shutdown drain.
    ///
    /// Hosts with multiple listeners can bind every required endpoint before
    /// publishing readiness, without rebuilding the SDK's HTTP lifecycle.
    pub async fn serve_listener_with_router_wrapper_and_shutdown<F, S>(
        &self,
        listener: tokio::net::TcpListener,
        wrapper: F,
        shutdown: S,
    ) -> Result<()>
    where
        F: Fn(Router) -> Router + Send + Sync + 'static,
        S: Future<Output = ()> + Send + 'static,
    {
        self.serve_listener_inner(listener, Some(Arc::new(wrapper)), shutdown)
            .await
    }

    async fn serve_inner<S>(
        &self,
        listen: &str,
        wrapper: Option<RouterWrapper>,
        shutdown: S,
    ) -> Result<()>
    where
        S: Future<Output = ()> + Send + 'static,
    {
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .map_err(|e| BitrouterError::internal(format!("bind {listen}: {e}")))?;
        self.serve_listener_inner(listener, wrapper, shutdown).await
    }

    async fn serve_listener_inner<S>(
        &self,
        listener: tokio::net::TcpListener,
        wrapper: Option<RouterWrapper>,
        shutdown: S,
    ) -> Result<()>
    where
        S: Future<Output = ()> + Send + 'static,
    {
        let pipeline = self
            .language_model()
            .ok_or_else(|| {
                BitrouterError::internal("App::serve: no language_model pipeline configured")
            })?
            .clone();
        let state = AppState {
            language_model: pipeline.clone(),
            mcp: self.mcp().cloned(),
            skip_auth: self.skip_auth(),
            metrics_renderer: self.metrics_renderer().cloned(),
            prompt_transforms: self.prompt_transforms().to_vec(),
        };
        let options = RouterOptions {
            omit_v1_models: false,
            mcp_aggregate_route: self.mcp_aggregate_route().map(String::from),
            router_wrapper: wrapper,
        };
        let router = build_router_with_options(state, options);
        tracing::info!(listen = ?listener.local_addr(), "bitrouter listening");
        // Graceful shutdown: on SIGINT/SIGTERM
        // stop accepting new connections and let in-flight requests finish.
        let drain_pipeline = pipeline.clone();
        let server = async move {
            serve(listener, router)
                .with_graceful_shutdown(shutdown)
                .await
                .map_err(|error| BitrouterError::internal(format!("serve: {error}")))
        };
        // Only after axum stops accepting connections and drains every handler
        // do we close detached execution and require success-critical
        // reconciliation. A failed attempt retains its evidence and is retried
        // serially; an external hard kill remains the only way to interrupt it.
        let drained = complete_graceful_shutdown(
            server,
            move || {
                let pipeline = drain_pipeline.clone();
                async move { pipeline.drain_required_pending_settlements().await }
            },
            REQUIRED_SHUTDOWN_RETRY_DELAY,
        )
        .await?;
        if drained > 0 {
            tracing::info!(drained, "drained pending settlements on shutdown");
        }
        Ok(())
    }
}

async fn complete_graceful_shutdown<S, D, DrainFuture>(
    server: S,
    mut drain: D,
    retry_delay: Duration,
) -> Result<usize>
where
    S: Future<Output = Result<()>>,
    D: FnMut() -> DrainFuture,
    DrainFuture: Future<Output = Result<usize>>,
{
    server.await?;
    loop {
        match drain().await {
            Ok(drained) => return Ok(drained),
            Err(_) => {
                tracing::warn!(
                    reason = "required_shutdown_drain_failed",
                    "required shutdown drain failed; retrying"
                );
                tokio::time::sleep(retry_delay).await;
            }
        }
    }
}

/// Resolves when the process receives `SIGINT` (Ctrl-C) or `SIGTERM`.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received — draining in-flight requests");
}

/// Inbound request body ceiling. LLM prompts can be large (long context, image
/// data-URLs), so the limit is generous — but bounded, so a request body can
/// never be an unbounded allocation.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// A router-wrapper closure. The wrapper runs after the SDK has mounted
/// every route and applied every built-in layer, so a host can wrap the
/// whole router in additional middleware (e.g. the layer that creates the
/// SERVER span at HTTP ingress).
///
/// Held behind an `Arc<dyn Fn>` so [`RouterOptions`] remains `Clone`.
/// `Fn` (not `FnOnce`) lets the same options be applied more than once.
pub type RouterWrapper = Arc<dyn Fn(Router) -> Router + Send + Sync>;

/// Options controlling which routes the SDK mounts. Hosts that ship their
/// own richer variant of a built-in route opt out of the SDK's plainer
/// version here so [`axum::Router::merge`] does not panic on the duplicate
/// path.
#[derive(Default, Clone)]
pub struct RouterOptions {
    /// When `true`, omit `GET /v1/models` from the returned router.
    pub omit_v1_models: bool,
    /// Path for the aggregate MCP endpoint (`Some("/mcp")` by typical
    /// convention). `None` omits the aggregate route — only per-server routes
    /// (`/mcp/{server}`) are mounted.
    pub mcp_aggregate_route: Option<String>,
    /// Optional wrapper applied to the fully-built router. Set via
    /// [`RouterOptions::with_router_wrapper`].
    pub router_wrapper: Option<RouterWrapper>,
}

impl RouterOptions {
    /// Install a router-wrapper closure that runs after the SDK has mounted
    /// every route. Used to add inbound HTTP middleware (e.g. the observe
    /// plugin's ingress-span layer) without coupling the SDK to OpenTelemetry
    /// or any other tracing backend.
    pub fn with_router_wrapper<F>(mut self, wrapper: F) -> Self
    where
        F: Fn(Router) -> Router + Send + Sync + 'static,
    {
        self.router_wrapper = Some(Arc::new(wrapper));
        self
    }
}

/// Build the axum router for the given state.
pub fn build_router(state: AppState) -> Router {
    build_router_with_options(state, RouterOptions::default())
}

/// Like [`build_router`], but lets the caller opt out of specific routes
/// before they are mounted (so a host can supply its own variant without
/// tripping `Router::merge`'s duplicate-route panic).
pub fn build_router_with_options(state: AppState, options: RouterOptions) -> Router {
    let mut router = Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses))
        .route("/v1beta/models/{*model_action}", post(generate_content));
    if !options.omit_v1_models {
        router = router.route("/v1/models", get(list_models));
    }
    router = router.route("/mcp/{server}", post(mcp_invoke));
    if let Some(path) = options.mcp_aggregate_route {
        router = router.route(&path, post(mcp_invoke_aggregate));
    }
    let router = router
        .route("/metrics", get(prometheus_metrics))
        .route("/health", get(health))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state);
    match options.router_wrapper {
        Some(wrapper) => wrapper(router),
        None => router,
    }
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({ "status": "ok" })))
}

/// `GET /metrics` — Prometheus text-exposition. Returns 404 when
/// no [`MetricsRenderer`] is wired into the app, so scrapers can probe.
async fn prometheus_metrics(State(state): State<AppState>) -> Response {
    match &state.metrics_renderer {
        Some(renderer) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, renderer.content_type())
            .body(Body::from(renderer.render()))
            .unwrap_or_else(|e| {
                BitrouterError::internal(format!("rendering metrics: {e}")).into_response()
            }),
        None => (StatusCode::NOT_FOUND, "metrics renderer not configured\n").into_response(),
    }
}

/// `POST /mcp/{server}` — Model Context Protocol invocation.
///
/// Implements legacy initialized sessions and the self-contained stateless
/// `2026-07-28` lifecycle, including the Streamable HTTP SSE response variant.
/// Spec refs:
/// - JSON-RPC envelope: <https://modelcontextprotocol.io/specification/2025-06-18/basic>
///   ("Result responses MUST include the same ID as the request they
///   correspond to"). The MCP Streamable HTTP transport (Origin /
///   `MCP-Protocol-Version` / `MCP-Session-Id` requirements) is at
///   <https://modelcontextprotocol.io/specification/2025-06-18/basic/transports>.
async fn mcp_invoke(
    State(state): State<AppState>,
    Path(server): Path<String>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    mcp_invoke_inner(state, mcp::ServerSelector::Direct(server), headers, body).await
}

/// `POST /mcp` — the aggregate (fan-out) MCP endpoint. Mounted only when
/// `RouterOptions.mcp_aggregate_route` is `Some(path)`.
async fn mcp_invoke_aggregate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    mcp_invoke_inner(state, mcp::ServerSelector::Aggregate, headers, body).await
}

async fn mcp_invoke_inner(
    state: AppState,
    selector: mcp::ServerSelector,
    headers: HeaderMap,
    body: serde_json::Value,
) -> Response {
    let Some(pipeline) = state.mcp.clone() else {
        return BitrouterError::NotFound("mcp pipeline not configured".to_string()).into_response();
    };

    // Validate the transport-level Origin before interpreting the message.
    // Protocol-version failures need the JSON-RPC id and the structured
    // `-32022` data payload, so they are handled after envelope validation.
    if let Err(e) = validate_mcp_transport_headers(&headers) {
        return e.into_response();
    }

    // Capture the inbound JSON-RPC envelope so we can echo `id` correctly even
    // for envelope-level rejections. Per JSON-RPC 2.0: `jsonrpc` MUST be exactly
    // "2.0"; `id` is string|number|null.
    let inbound_id = body.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let jsonrpc = body.get("jsonrpc").and_then(|v| v.as_str()).unwrap_or("");
    if jsonrpc != "2.0" {
        return mcp_error_response(
            inbound_id,
            -32600,
            "Invalid Request: missing or wrong 'jsonrpc' (MUST be \"2.0\")",
        );
    }

    let method = body
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .to_string();
    if method.is_empty() {
        return mcp_error_response(inbound_id, -32600, "Invalid Request: missing 'method'");
    }
    if let Some(version) = headers
        .get("mcp-protocol-version")
        .and_then(|value| value.to_str().ok())
        && !MCP_SUPPORTED_PROTOCOL_VERSIONS.contains(&version)
    {
        return mcp_error_response_with_data(
            inbound_id,
            -32022,
            &format!("unsupported MCP protocol version `{version}`"),
            serde_json::json!({
                "requested": version,
                "supported": MCP_SUPPORTED_PROTOCOL_VERSIONS,
            }),
        );
    }
    let params = body
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let modern_request =
        match validate_mcp_request_protocol(&headers, &method, &params, body.get("id").is_some()) {
            Ok(modern) => modern,
            Err((code, message, Some(data))) => {
                return mcp_error_response_with_data(inbound_id, code, &message, data);
            }
            Err((code, message, None)) => return mcp_error_response(inbound_id, code, &message),
        };
    if modern_request
        && let Err(message) = validate_mcp_standard_headers(&headers, &method, &params)
    {
        return mcp_error_response(inbound_id, -32020, &message);
    }

    // MCP lifecycle methods are answered by the gateway itself — they negotiate
    // the client<->gateway session and MUST NOT be proxied to an upstream
    // executor (which dispatches only `tools/*`, `resources/*`, `prompts/*` and
    // rejects everything else as "method not found"). Without this, every
    // spec-compliant client (Claude clients, opencode, the MCP SDK) fails its
    // opening `initialize` over Streamable HTTP and never reaches a tool call;
    // only handshake-skipping callers (curl, `bro tools`) worked before.
    // See <https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle>.
    match method.as_str() {
        "server/discover" => {
            return Json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": inbound_id,
                "result": {
                    "resultType": "complete",
                    "supportedVersions": MCP_SUPPORTED_PROTOCOL_VERSIONS,
                    "capabilities": mcp_gateway_capabilities(),
                    "ttlMs": 0,
                    "cacheScope": "private",
                    "_meta": {
                        "io.modelcontextprotocol/serverInfo": {
                            "name": "bitrouter-mcp-gateway",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                    },
                },
            }))
            .into_response();
        }
        "initialize" => {
            // Spec: echo the client's protocol version when we support it,
            // otherwise answer with our latest. `params.protocolVersion` is the
            // client's requested version.
            let protocol_version = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .filter(|v| MCP_LEGACY_PROTOCOL_VERSIONS.contains(v))
                .unwrap_or(MCP_LATEST_LEGACY_PROTOCOL_VERSION);
            return Json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": inbound_id,
                "result": {
                    "protocolVersion": protocol_version,
                    // `resources` is advertised because Agent Skills served
                    // over MCP (SEP-2640) ride entirely on `resources/read`: a
                    // compliant client that sees no `resources` capability
                    // never issues one, so withholding it makes skills
                    // unreachable through the gateway.
                    //
                    // The earlier reasoning for withholding it — that clients
                    // would probe upstreams which may not implement resources
                    // — is answered by the fan-out contract rather than by
                    // silence: a member without resources contributes an empty
                    // list and an entry under `_bitrouterErrors`, which is the
                    // correct answer to "what resources do you have".
                    //
                    // The skills extension (SEP-2640) is declared
                    // **optimistically**. The gateway answers `initialize`
                    // synchronously but discovers upstream capabilities lazily
                    // on first connect, so at this point it cannot know
                    // whether any member serves skills. Declaring costs a
                    // client one `skills/list` round trip that comes back
                    // empty; the alternative — probing every upstream at
                    // daemon start — would spawn every stdio child at boot.
                    //
                    // `directoryRead` is left absent (it defaults to `false`).
                    // The SEP forbids a client calling
                    // `resources/directory/read` against a server that has not
                    // declared it, so `false` is the safe answer for a gateway
                    // that cannot know whether its members implement it.
                    "capabilities": mcp_gateway_capabilities(),
                    "serverInfo": {
                        "name": "bitrouter-mcp-gateway",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                },
            }))
            .into_response();
        }
        // JSON-RPC notifications carry no `id` and expect no result body — ack
        // with 202 Accepted per the Streamable HTTP transport.
        "notifications/initialized" | "notifications/cancelled" => {
            return axum::http::StatusCode::ACCEPTED.into_response();
        }
        "ping" => {
            let mut result = serde_json::json!({});
            shape_mcp_result_for_peer(&method, modern_request, &mut result);
            return Json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": inbound_id,
                "result": result,
            }))
            .into_response();
        }
        _ => {}
    }

    // Default caller: `local` when auth is disabled, otherwise an `anonymous`
    // placeholder that a downstream `mcp::PreRequestHook` may upgrade to the
    // real identity by reading `ctx.headers()` and calling `ctx.set_caller()`.
    let caller = if state.skip_auth {
        CallerContext::local()
    } else {
        CallerContext::anonymous()
    };
    let client_context = modern_request
        .then(|| downstream_mcp_client_context(&params))
        .flatten();
    let upstream_params = strip_downstream_request_context(params);
    let mut request = match selector {
        mcp::ServerSelector::Direct(server) => {
            mcp::McpRequest::direct(server, method.clone(), upstream_params, caller)
        }
        mcp::ServerSelector::Aggregate => {
            mcp::McpRequest::aggregate(method.clone(), upstream_params, caller)
        }
    }
    .with_headers(headers.clone());
    if let Some(client_context) = client_context {
        request = request.with_client_context(client_context);
    }

    // A modern tools/call must be checked against the schema BitRouter
    // publishes on this same downstream route. Fetching through the pipeline
    // deliberately runs auth before schema lookup, then the shared cache keeps
    // the ordinary case cheap. The upstream hop will construct its own
    // headers; validating here prevents a caller from presenting one value to
    // downstream middleware while asking BitRouter to execute another.
    if modern_request && method == "tools/call" {
        let mut list_request = request.clone();
        list_request.method = "tools/list".to_string();
        list_request.params = serde_json::json!({});
        let listed = match pipeline.execute(list_request).await {
            Ok(response) => response,
            Err(error) => return mcp_pipeline_error_response(inbound_id, &error),
        };
        if let Err(message) =
            validate_mcp_tool_parameter_headers(&headers, &request.params, &listed.result)
        {
            return mcp_error_response(inbound_id, -32020, &message);
        }
    }

    // SSE branch per the MCP Streamable HTTP spec — if the client opts in via
    // `Accept: text/event-stream` we return the JSON-RPC frames as `data:`
    // events. JSON clients get the buffered JSON shape (the existing path).
    if accepts_event_stream(&headers) {
        return match pipeline.execute_streaming(request).await {
            Ok(stream) => sse_response(inbound_id, method, modern_request, stream),
            Err(e) => mcp_pipeline_error_response(inbound_id, &e),
        };
    }

    match pipeline.execute(request).await {
        Ok(mut response) => {
            shape_mcp_result_for_peer(&method, modern_request, &mut response.result);
            Json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": inbound_id,
                "result": response.result,
            }))
            .into_response()
        }
        Err(e) => mcp_pipeline_error_response(inbound_id, &e),
    }
}

/// Map a [`BitrouterError`] from the MCP pipeline into the JSON-RPC error
/// envelope. Pipeline failures are returned with `error.code` mapped from the
/// `BitrouterError` variant; unknown-server (`NotFound` from
/// `RoutingTable::resolve`) maps to JSON-RPC "Method not found" (-32601).
/// Pre-request denies / upstream errors keep their HTTP status so MCP-unaware
/// proxies still surface them — but the body remains a JSON-RPC error object
/// for the spec-aware client.
fn mcp_pipeline_error_response(inbound_id: serde_json::Value, e: &BitrouterError) -> Response {
    let status = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let code = match e {
        BitrouterError::NotFound(_) => -32601,
        BitrouterError::BadRequest { .. } => -32602,
        BitrouterError::Unauthorized(_)
        | BitrouterError::Forbidden(_)
        | BitrouterError::PaymentRequired(_) => -32000,
        _ => -32603,
    };
    let mut response = (
        status,
        Json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": inbound_id,
            "error": { "code": code, "message": e.public_message() },
        })),
    )
        .into_response();
    apply_error_headers(&mut response, e);
    response
}

/// True if the client opted into the SSE response variant.
fn accepts_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| {
            s.split(',')
                .any(|p| p.trim().starts_with("text/event-stream"))
        })
}

/// Build an `Sse` response from a stream of [`mcp::McpStreamPart`]s. Each part
/// becomes one SSE `data:` event carrying the JSON-RPC notification or
/// response. The stream closes after the terminating frame — either the
/// `Final` result or the first error — so JSON-RPC semantics hold (one
/// terminal frame per `id`) and a client that has already seen the answer
/// never sits on an open connection waiting for nothing.
fn sse_response(
    inbound_id: serde_json::Value,
    method: String,
    modern: bool,
    stream: futures::stream::BoxStream<'static, crate::error::Result<mcp::McpStreamPart>>,
) -> Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    let inbound_id = Arc::new(inbound_id);
    // `scan` carries a "have we emitted the terminal frame?" flag. Once true,
    // the next poll returns `None` and the SSE stream closes — `take_while`
    // would drop the terminal frame itself, and `futures` has no
    // `take_while_inclusive`, so this is the portable equivalent.
    let terminated_stream = stream.scan(false, |done, item| {
        if *done {
            return std::future::ready(None);
        }
        if matches!(item, Ok(mcp::McpStreamPart::Final(_)) | Err(_)) {
            *done = true;
        }
        std::future::ready(Some(item))
    });
    let event_stream = terminated_stream.map(move |item| {
        let inbound_id = inbound_id.clone();
        match item {
            Ok(mcp::McpStreamPart::Notification { method, params }) => {
                let payload = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": method,
                    "params": params,
                });
                Ok::<_, Infallible>(Event::default().data(payload.to_string()))
            }
            Ok(mcp::McpStreamPart::Final(mut response)) => {
                shape_mcp_result_for_peer(&method, modern, &mut response.result);
                let payload = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": &*inbound_id,
                    "result": response.result,
                });
                Ok(Event::default().data(payload.to_string()))
            }
            Err(e) => {
                let payload = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": &*inbound_id,
                    "error": { "code": -32603, "message": e.public_message() },
                });
                Ok(Event::default().data(payload.to_string()))
            }
        }
    });
    Sse::new(event_stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// MCP supported transport protocol versions. Update when adding spec revisions.
/// See <https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle>.
const MCP_MODERN_PROTOCOL_VERSION: &str = "2026-07-28";
const MCP_LATEST_LEGACY_PROTOCOL_VERSION: &str = "2025-11-25";
const MCP_LEGACY_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const MCP_SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    MCP_MODERN_PROTOCOL_VERSION,
    MCP_LATEST_LEGACY_PROTOCOL_VERSION,
    MCP_LEGACY_PROTOCOL_VERSIONS[1],
    MCP_LEGACY_PROTOCOL_VERSIONS[2],
    MCP_LEGACY_PROTOCOL_VERSIONS[3],
];

fn mcp_gateway_capabilities() -> serde_json::Value {
    serde_json::json!({
        "tools": {},
        "resources": {},
        "extensions": { "io.modelcontextprotocol/skills": {} },
    })
}

/// Validate SEP-2575 per-request context and the Streamable HTTP protocol
/// header. Returns whether this request uses the stateless 2026 lifecycle.
fn validate_mcp_request_protocol(
    headers: &HeaderMap,
    method: &str,
    params: &serde_json::Value,
    has_id: bool,
) -> std::result::Result<bool, (i64, String, Option<serde_json::Value>)> {
    let header_version = headers
        .get("mcp-protocol-version")
        .and_then(|value| value.to_str().ok());

    if method == "initialize" {
        if let (Some(header), Some(body)) = (
            header_version,
            params
                .get("protocolVersion")
                .and_then(|value| value.as_str()),
        ) && header != body
        {
            return Err((
                -32600,
                format!(
                    "Invalid Request: MCP-Protocol-Version header ({header}) does not match initialize params.protocolVersion ({body})"
                ),
                None,
            ));
        }
        return Ok(false);
    }

    // Notifications do not carry per-request context and receive no result.
    if !has_id {
        return Ok(header_version == Some(MCP_MODERN_PROTOCOL_VERSION));
    }

    let meta = params.get("_meta").and_then(serde_json::Value::as_object);
    let meta_version = meta
        .and_then(|value| value.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(serde_json::Value::as_str);
    let modern = method == "server/discover"
        || meta_version.is_some()
        || header_version == Some(MCP_MODERN_PROTOCOL_VERSION);
    if !modern {
        return Ok(false);
    }

    let mut missing = Vec::new();
    if meta_version.is_none() {
        missing.push("io.modelcontextprotocol/protocolVersion");
    }
    if !meta
        .and_then(|value| value.get("io.modelcontextprotocol/clientCapabilities"))
        .is_some_and(serde_json::Value::is_object)
    {
        missing.push("io.modelcontextprotocol/clientCapabilities");
    }
    if !missing.is_empty() {
        return Err((
            -32602,
            format!(
                "Invalid params: request _meta is missing or has malformed required fields: {}",
                missing.join(", ")
            ),
            None,
        ));
    }
    let Some(header_version) = header_version else {
        return Err((
            -32020,
            "request _meta protocolVersion requires MCP-Protocol-Version header".to_string(),
            None,
        ));
    };
    let meta_version = meta_version.unwrap_or(MCP_MODERN_PROTOCOL_VERSION);
    if header_version != meta_version {
        return Err((
            -32020,
            format!(
                "MCP-Protocol-Version header ({header_version}) does not match request _meta protocolVersion ({meta_version})"
            ),
            None,
        ));
    }
    if meta_version != MCP_MODERN_PROTOCOL_VERSION {
        return Err((
            -32022,
            format!("protocol version `{meta_version}` does not support stateless requests"),
            Some(serde_json::json!({
                "requested": meta_version,
                "supported": [MCP_MODERN_PROTOCOL_VERSION],
            })),
        ));
    }
    Ok(true)
}

/// Per-request identity belongs to the downstream hop. The rmcp client adds
/// BitRouter's own protocol/client context for the upstream hop, so forwarding
/// the caller's reserved fields would conflate two MCP peers.
fn strip_downstream_request_context(mut params: serde_json::Value) -> serde_json::Value {
    let Some(params_object) = params.as_object_mut() else {
        return params;
    };
    let Some(meta) = params_object
        .get_mut("_meta")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return params;
    };
    for key in [
        "io.modelcontextprotocol/protocolVersion",
        "io.modelcontextprotocol/clientInfo",
        "io.modelcontextprotocol/clientCapabilities",
    ] {
        meta.remove(key);
    }
    if meta.is_empty() {
        params_object.remove("_meta");
    }
    params
}

fn downstream_mcp_client_context(params: &serde_json::Value) -> Option<mcp::McpClientContext> {
    let meta = params.get("_meta")?.as_object()?;
    Some(mcp::McpClientContext {
        protocol_version: meta
            .get("io.modelcontextprotocol/protocolVersion")?
            .as_str()?
            .to_string(),
        client_info: meta.get("io.modelcontextprotocol/clientInfo").cloned(),
        client_capabilities: meta
            .get("io.modelcontextprotocol/clientCapabilities")?
            .clone(),
    })
}

/// Validate the SEP-2243 standard routing headers introduced with
/// `2026-07-28`. Tool-parameter headers are validated separately after the
/// authenticated pipeline resolves the schema published on this route.
fn validate_mcp_standard_headers(
    headers: &HeaderMap,
    method: &str,
    params: &serde_json::Value,
) -> std::result::Result<(), String> {
    match headers
        .get("mcp-method")
        .and_then(|value| value.to_str().ok())
    {
        None => return Err("missing required Mcp-Method header".to_string()),
        Some(value) if value != method => {
            return Err(format!(
                "Mcp-Method header `{value}` does not match body method `{method}`"
            ));
        }
        Some(_) => {}
    }
    let name_key = if matches!(method, "tools/call" | "prompts/get") {
        Some("name")
    } else if matches!(
        method,
        "resources/read" | "resources/subscribe" | "resources/unsubscribe"
    ) {
        Some("uri")
    } else if matches!(method, "tasks/get" | "tasks/update" | "tasks/cancel") {
        Some("taskId")
    } else {
        None
    };
    let Some(expected) = name_key
        .and_then(|key| params.get(key))
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(());
    };
    let raw = headers
        .get("mcp-name")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| format!("missing required Mcp-Name header for `{method}`"))?;
    let decoded = decode_mcp_header_value(raw)
        .ok_or_else(|| "Mcp-Name header is not valid Base64".to_string())?;
    if decoded != expected {
        return Err(format!(
            "Mcp-Name header `{decoded}` does not match body value `{expected}`"
        ));
    }
    Ok(())
}

fn decode_mcp_header_value(value: &str) -> Option<String> {
    use base64::{Engine, prelude::BASE64_STANDARD};

    match value
        .strip_prefix("=?base64?")
        .and_then(|inner| inner.strip_suffix("?="))
    {
        Some(inner) => String::from_utf8(BASE64_STANDARD.decode(inner).ok()?).ok(),
        None => Some(value.to_string()),
    }
}

#[derive(Clone, Copy)]
enum McpParameterKind {
    String,
    Integer,
    Boolean,
}

struct McpParameterHeader {
    name: HeaderName,
    display_name: String,
    path: Vec<String>,
    kind: McpParameterKind,
}

fn validate_mcp_tool_parameter_headers(
    headers: &HeaderMap,
    params: &serde_json::Value,
    tools_result: &serde_json::Value,
) -> std::result::Result<(), String> {
    let Some(tool_name) = params.get("name").and_then(serde_json::Value::as_str) else {
        return Ok(());
    };
    let Some(tool) = tools_result
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .and_then(|tools| {
            tools.iter().find(|tool| {
                tool.get("name").and_then(serde_json::Value::as_str) == Some(tool_name)
            })
        })
    else {
        return Ok(());
    };
    let Some(schema) = tool.get("inputSchema") else {
        return Ok(());
    };
    let bindings = collect_mcp_parameter_headers(schema).map_err(|error| {
        format!("tool `{tool_name}` has invalid x-mcp-header metadata: {error}")
    })?;
    let null_arguments = serde_json::Value::Null;
    let arguments = params.get("arguments").unwrap_or(&null_arguments);
    for binding in bindings {
        let value = value_at_property_path(arguments, &binding.path);
        let raw = headers
            .get(&binding.name)
            .map(|header| header.to_str())
            .transpose()
            .map_err(|_| format!("{} contains invalid bytes", binding.display_name))?;
        match value {
            None | Some(serde_json::Value::Null) => {
                if raw.is_some() {
                    return Err(format!(
                        "{} is present but its tool argument is absent or null",
                        binding.display_name
                    ));
                }
            }
            Some(value) => {
                let raw = raw.ok_or_else(|| {
                    format!(
                        "missing required {} for argument `{}`",
                        binding.display_name,
                        binding.path.join(".")
                    )
                })?;
                let decoded = decode_mcp_header_value(raw)
                    .ok_or_else(|| format!("{} is not valid Base64", binding.display_name))?;
                if !mcp_parameter_value_matches(binding.kind, value, &decoded) {
                    return Err(format!(
                        "{} does not match argument `{}`",
                        binding.display_name,
                        binding.path.join(".")
                    ));
                }
            }
        }
    }
    Ok(())
}

fn collect_mcp_parameter_headers(
    schema: &serde_json::Value,
) -> std::result::Result<Vec<McpParameterHeader>, String> {
    fn count_annotations(value: &serde_json::Value) -> usize {
        match value {
            serde_json::Value::Object(object) => {
                usize::from(object.contains_key("x-mcp-header"))
                    + object.values().map(count_annotations).sum::<usize>()
            }
            serde_json::Value::Array(values) => values.iter().map(count_annotations).sum(),
            _ => 0,
        }
    }

    fn visit_properties(
        schema: &serde_json::Value,
        path: &mut Vec<String>,
        seen_names: &mut std::collections::BTreeSet<String>,
        bindings: &mut Vec<McpParameterHeader>,
        visited: &mut usize,
    ) -> std::result::Result<(), String> {
        let Some(properties) = schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
        else {
            return Ok(());
        };
        for (property_name, property_schema) in properties {
            path.push(property_name.clone());
            if let Some(annotation) = property_schema.get("x-mcp-header") {
                *visited += 1;
                let annotation = annotation
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| "annotation name must be a non-empty string".to_string())?;
                let canonical = annotation.to_ascii_lowercase();
                let wire_name = format!("mcp-param-{canonical}");
                if !seen_names.insert(canonical) {
                    return Err(format!(
                        "duplicate case-insensitive annotation name `{annotation}`"
                    ));
                }
                let display_name = format!("Mcp-Param-{annotation}");
                let name = HeaderName::from_bytes(wire_name.as_bytes())
                    .map_err(|_| format!("`{annotation}` is not a valid HTTP field-name token"))?;
                let kind = match property_schema
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                {
                    Some("string") => McpParameterKind::String,
                    Some("integer") => McpParameterKind::Integer,
                    Some("boolean") => McpParameterKind::Boolean,
                    _ => {
                        return Err(format!(
                            "annotation `{annotation}` is not on a string, integer, or boolean"
                        ));
                    }
                };
                bindings.push(McpParameterHeader {
                    name,
                    display_name,
                    path: path.clone(),
                    kind,
                });
            }
            visit_properties(property_schema, path, seen_names, bindings, visited)?;
            path.pop();
        }
        Ok(())
    }

    let total = count_annotations(schema);
    let mut bindings = Vec::new();
    let mut visited = 0;
    visit_properties(
        schema,
        &mut Vec::new(),
        &mut std::collections::BTreeSet::new(),
        &mut bindings,
        &mut visited,
    )?;
    if visited != total {
        return Err(
            "annotation is not reachable from the schema root through properties only".to_string(),
        );
    }
    Ok(bindings)
}

fn value_at_property_path<'a>(
    arguments: &'a serde_json::Value,
    path: &[String],
) -> Option<&'a serde_json::Value> {
    path.iter().try_fold(arguments, |value, segment| {
        value.as_object().and_then(|object| object.get(segment))
    })
}

fn mcp_parameter_value_matches(
    kind: McpParameterKind,
    value: &serde_json::Value,
    header: &str,
) -> bool {
    match kind {
        McpParameterKind::String => value.as_str() == Some(header),
        McpParameterKind::Boolean => match value.as_bool() {
            Some(true) => header == "true",
            Some(false) => header == "false",
            None => false,
        },
        McpParameterKind::Integer => {
            const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
            let Some(body) = value.as_f64().filter(|number| {
                number.is_finite() && number.fract() == 0.0 && number.abs() <= MAX_SAFE_INTEGER
            }) else {
                return false;
            };
            header.parse::<f64>().is_ok_and(|candidate| {
                candidate.is_finite()
                    && candidate.fract() == 0.0
                    && candidate.abs() <= MAX_SAFE_INTEGER
                    && candidate == body
            })
        }
    }
}

fn shape_mcp_result_for_peer(method: &str, modern: bool, result: &mut serde_json::Value) {
    let Some(object) = result.as_object_mut() else {
        return;
    };
    if modern {
        object
            .entry("resultType".to_string())
            .or_insert_with(|| "complete".into());
        if matches!(
            method,
            "tools/list"
                | "resources/list"
                | "resources/templates/list"
                | "resources/read"
                | "prompts/list"
                | "skills/list"
                | "skills/get"
        ) {
            object
                .entry("ttlMs".to_string())
                .or_insert_with(|| 0.into());
            object
                .entry("cacheScope".to_string())
                .or_insert_with(|| "private".into());
        }
        let meta = object
            .entry("_meta".to_string())
            .or_insert_with(|| serde_json::json!({}));
        if !meta.is_object() {
            *meta = serde_json::json!({});
        }
        if let Some(meta) = meta.as_object_mut() {
            meta.insert(
                "io.modelcontextprotocol/serverInfo".to_string(),
                serde_json::json!({
                    "name": "bitrouter-mcp-gateway",
                    "version": env!("CARGO_PKG_VERSION"),
                }),
            );
        }
    } else if !matches!(method, "skills/list" | "skills/get") {
        object.remove("resultType");
        object.remove("ttlMs");
        object.remove("cacheScope");
    }
}

/// Validates the MCP Streamable HTTP transport headers per the spec at
/// <https://modelcontextprotocol.io/specification/2025-06-18/basic/transports>.
/// `Origin`: MUST be validated by the server to defeat DNS rebinding — we accept
/// localhost / 127.0.0.1 / [::1] by default (the only safe default for a local
/// daemon binding to loopback). Protocol-version validation happens after the
/// JSON-RPC envelope is parsed so failures can carry the request id and the
/// structured `UnsupportedProtocolVersion` data.
fn validate_mcp_transport_headers(headers: &HeaderMap) -> Result<()> {
    if let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok())
        && !is_safe_mcp_origin(origin)
    {
        return Err(BitrouterError::Forbidden(format!(
            "MCP Origin not allowed: '{origin}'. Local daemons accept only loopback origins."
        )));
    }
    Ok(())
}

/// Loopback-only Origin allow-list; covers the browser shape (`http://...`),
/// the file:// shape, and the bare-host shape some MCP clients use.
fn is_safe_mcp_origin(origin: &str) -> bool {
    // null-Origin (e.g. file://) and same-origin requests with no Origin header
    // already pass — this only inspects values that *did* arrive.
    if origin == "null" {
        return true;
    }
    let host = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .unwrap_or(origin);
    let host = host.split('/').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

/// Build a JSON-RPC error response with HTTP 400 (transport-level rejection).
fn mcp_error_response(id: serde_json::Value, code: i64, message: &str) -> Response {
    (
        axum::http::StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        })),
    )
        .into_response()
}

/// Build a JSON-RPC protocol error whose machine-readable payload is required
/// for version negotiation.
fn mcp_error_response_with_data(
    id: serde_json::Value,
    code: i64,
    message: &str,
    data: serde_json::Value,
) -> Response {
    (
        axum::http::StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message, "data": data },
        })),
    )
        .into_response()
}

async fn list_models(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let models = state.language_model.routing_table().list_models();
    let data: Vec<_> = models
        .into_iter()
        .map(|m| serde_json::json!({ "id": m.id, "object": "model", "providers": m.providers }))
        .collect();
    let mut body = serde_json::json!({ "object": "list", "data": data });
    if is_codex_user_agent(&headers)
        && let Some(obj) = body.as_object_mut()
        && let Some(data) = obj.get("data").cloned()
    {
        obj.insert("models".to_string(), data);
    }
    Json(body)
}

fn is_codex_user_agent(headers: &HeaderMap) -> bool {
    headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ua| ua.to_ascii_lowercase().contains("codex"))
}

// ===== inbound protocol handlers =====

async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    handle(state, headers, ApiProtocol::Messages, body, None).await
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    handle(state, headers, ApiProtocol::ChatCompletions, body, None).await
}

async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    handle(state, headers, ApiProtocol::Responses, body, None).await
}

/// Generate Content encodes the model and streaming verb in the path. The
/// catch-all also admits slash selectors such as `bitrouter/coding`; Axum
/// decodes a percent-escaped slash before this handler validates the selector.
async fn generate_content(
    State(state): State<AppState>,
    Path(model_action): Path<String>,
    headers: HeaderMap,
    Json(mut body): Json<serde_json::Value>,
) -> Response {
    let (model, action) = match model_action.rsplit_once(':') {
        Some((m, a))
            if !m.is_empty() && matches!(a, "generateContent" | "streamGenerateContent") =>
        {
            (m.to_string(), a.to_string())
        }
        _ => {
            return BitrouterError::bad_request(
                "google path must be 'models/{model}:generateContent' or 'models/{model}:streamGenerateContent'",
            )
            .into_response();
        }
    };
    // Generate Content carries the model in the URL, not the body — inject it so the
    // adapter sees it, and set the stream flag from the verb.
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".into(), model.clone().into());
        obj.insert("stream".into(), (action == "streamGenerateContent").into());
    }
    handle(
        state,
        headers,
        ApiProtocol::GenerateContent,
        body,
        Some(model),
    )
    .await
}

/// Shared handler: parse with the inbound adapter, run the pipeline, render the
/// reply back in the same inbound protocol.
async fn handle(
    state: AppState,
    mut headers: HeaderMap,
    inbound: ApiProtocol,
    body: serde_json::Value,
    model_override: Option<String>,
) -> Response {
    add_inbound_protocol_hint(&mut headers, &inbound);
    let request_id = match add_request_id_hint(&mut headers) {
        Ok(request_id) => request_id,
        Err(error) => return error.into_response(),
    };
    if inbound == ApiProtocol::Responses
        && let Err(error) = encode_gateway_continuation_id(&request_id)
    {
        return error.into_response();
    }
    let adapter = match inbound_adapter_for(&inbound) {
        Some(a) => a,
        None => {
            return BitrouterError::internal(format!(
                "no inbound adapter for protocol '{inbound}' — Custom protocols are \
                 outbound-only by design"
            ))
            .into_response();
        }
    };
    let (prompt, original_model) = match adapter.parse_request(body) {
        Ok(mut p) => {
            if let Some(model) = model_override {
                p.model = model;
            }
            p.model = sanitize_model_name(&p.model);
            let original_model = p.model.clone();
            // Ingress-time prompt transforms (e.g. the bitrouter/fusion model
            // alias): the prompt body is freely mutable here, before it enters
            // the pipeline that exposes it read-only downstream.
            for transform in &state.prompt_transforms {
                transform.apply_with_headers(&mut p, &headers);
            }
            (p, original_model)
        }
        Err(e) => return e.into_response(),
    };

    // `skip_auth` decides the starting caller: a synthesised local caller when
    // on, else a pre-auth anonymous placeholder for `AuthHook` to upgrade or
    // reject.
    //
    // On the `skip_auth` path only, a credential `bro launch` minted
    // tags the caller with its session, so per-launch spend is answerable on
    // the zero-config install that cannot answer it any other way. This never
    // grants anything — see `caller::launch_tag` — and a real key falls
    // through it untouched into the `AuthHook` path below.
    let caller = if state.skip_auth {
        let authorization = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        match crate::caller::launch_tag(authorization) {
            Some(launch_id) => CallerContext::local_launch(launch_id),
            None => CallerContext::local(),
        }
    } else {
        CallerContext::anonymous()
    };
    let mut req = PipelineRequest::new(prompt.model.clone(), caller, prompt.clone());
    req.request_id = request_id.clone();
    req.original_model = original_model;
    req.headers = headers;
    // Carry the inbound wire protocol so route resolution can prefer a native,
    // same-protocol upstream — a faithful round-trip instead of a lossy
    // cross-protocol translation.
    req.inbound_protocol = Some(inbound.clone());

    let mut response = if prompt.stream {
        stream_response(state.language_model.clone(), req, inbound.clone()).await
    } else {
        // `execute_detached`, not `execute`: a non-streaming request must run to
        // completion and settle even if the client disconnects (axum drops this
        // handler future on disconnect). The upstream bills us for the accepted
        // request regardless, so the customer must be billed too.
        match state
            .language_model
            .clone()
            .execute_detached_prepared(req)
            .await
        {
            Ok(prepared) => {
                let mut response_prompt = prompt.clone();
                response_prompt.model = prepared.model_id.clone();
                match adapter.render_response(
                    &prepared.response.result,
                    &response_prompt,
                    &prepared.response.request_id,
                ) {
                    Ok(json) => match prepared.delivery.deliver().await {
                        Ok(()) => Json(json).into_response(),
                        Err(error) => error.into_response(),
                    },
                    Err(e) => match prepared.delivery.fail(e.clone()).await {
                        Ok(()) => e.into_response(),
                        Err(authorization_error) => authorization_error.into_response(),
                    },
                }
            }
            Err(e) => e.into_response(),
        }
    };
    // Every admitted pipeline result, including a pre-request rejection, must
    // expose the correlation ID used by the daemon's process-local receipts.
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response
            .headers_mut()
            .insert(BITROUTER_REQUEST_ID_HEADER, value);
    }
    response
}

fn add_inbound_protocol_hint(headers: &mut HeaderMap, inbound: &ApiProtocol) {
    if let Ok(value) = HeaderValue::from_str(inbound.as_str()) {
        headers.insert("x-bitrouter-inbound-protocol", value);
    }
}

fn add_request_id_hint(headers: &mut HeaderMap) -> Result<String> {
    if let Some(value) = headers.get(BITROUTER_REQUEST_ID_HEADER) {
        let request_id = value
            .to_str()
            .map_err(|_| BitrouterError::bad_request("request id header is not valid text"))?
            .trim();
        if request_id.is_empty() {
            return Err(BitrouterError::bad_request(
                "request id header must not be empty",
            ));
        }
        return Ok(request_id.to_string());
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        headers.insert(BITROUTER_REQUEST_ID_HEADER, value);
    }
    Ok(request_id)
}

/// Build a `text/event-stream` response: pipe the canonical part stream through
/// the inbound protocol's `StreamEncoder`, wrap it in `SseKeepaliveStream`, and
/// stream the wire bytes.
async fn stream_response(
    pipeline: Arc<Pipeline>,
    req: PipelineRequest,
    inbound: ApiProtocol,
) -> Response {
    let adapter = match inbound_adapter_for(&inbound) {
        Some(a) => a,
        None => {
            return BitrouterError::internal(format!(
                "no inbound adapter for protocol '{inbound}' — Custom protocols are \
                 outbound-only by design"
            ))
            .into_response();
        }
    };
    let request_id = req.request_id.clone();
    let keepalive = pipeline.keepalive_interval();

    // Route resolution and the upstream HTTP handshake happen before the SSE
    // response is constructed. Pre-stream failures therefore retain their real
    // HTTP status (notably upstream 429) instead of being trapped inside an
    // already-committed HTTP 200 event stream.
    let prepared = match pipeline.execute_stream_prepared(req).await {
        Ok(prepared) => prepared,
        Err(error) => return error.into_response(),
    };
    let mut encoder = adapter.stream_encoder(&request_id, &prepared.model_id);
    let mut parts = prepared.parts;

    let frame_stream = async_stream::stream! {
        while let Some(item) = parts.next().await {
            match item {
                Ok(prepared) => {
                    let crate::language_model::pipeline::PreparedStreamPart {
                        part,
                        mut delivery,
                    } = prepared;
                    match encoder.encode(&part) {
                        Ok(mut frames) => {
                            if let Some(permit) = delivery.take() {
                                // A canonical success terminal ends the
                                // prepared stream. Expand both that part and
                                // `encoder.finish()` first, then authorize only
                                // the final wire frame. Earlier expansion
                                // frames remain provisional if the body drops.
                                match parts.next().await {
                                    None => {}
                                    Some(Err(error)) => {
                                        if let Err(authorization_error) =
                                            permit.fail(error.clone()).await
                                        {
                                            for frame in encoder
                                                .encode_bitrouter_error(&authorization_error)
                                            {
                                                yield frame;
                                            }
                                            return;
                                        }
                                        for frame in encoder.encode_bitrouter_error(&error) {
                                            yield frame;
                                        }
                                        return;
                                    }
                                    Some(Ok(_)) => {
                                        let error = BitrouterError::internal(
                                            "prepared stream continued after success terminal",
                                        );
                                        if let Err(authorization_error) =
                                            permit.fail(error.clone()).await
                                        {
                                            for frame in encoder
                                                .encode_bitrouter_error(&authorization_error)
                                            {
                                                yield frame;
                                            }
                                            return;
                                        }
                                        for frame in encoder.encode_bitrouter_error(
                                            &error,
                                        ) {
                                            yield frame;
                                        }
                                        return;
                                    }
                                }
                                match encoder.finish() {
                                    Ok(finish_frames) => frames.extend(finish_frames),
                                    Err(error) => {
                                        if let Err(authorization_error) =
                                            permit.fail(error.clone()).await
                                        {
                                            for frame in encoder
                                                .encode_bitrouter_error(&authorization_error)
                                            {
                                                yield frame;
                                            }
                                            return;
                                        }
                                        for frame in encoder.encode_bitrouter_error(&error) {
                                            yield frame;
                                        }
                                        return;
                                    }
                                }
                                let Some(last) = frames.len().checked_sub(1) else {
                                    // Zero wire frames cannot prove delivery.
                                    let error = BitrouterError::internal(
                                        "successful terminal encoded to zero wire frames",
                                    );
                                    if let Err(authorization_error) = permit.fail(error.clone()).await
                                    {
                                        for frame in
                                            encoder.encode_bitrouter_error(&authorization_error)
                                        {
                                            yield frame;
                                        }
                                        return;
                                    }
                                    for frame in encoder.encode_bitrouter_error(&error) {
                                        yield frame;
                                    }
                                    return;
                                };
                                let mut permit = Some(permit);
                                for (index, frame) in frames.into_iter().enumerate() {
                                    if index == last
                                        && let Some(permit) = permit.take()
                                        && let Err(error) = permit.deliver().await
                                    {
                                        for error_frame in encoder.encode_bitrouter_error(&error) {
                                            yield error_frame;
                                        }
                                        return;
                                    }
                                    yield frame;
                                }
                                return;
                            }
                            for frame in frames {
                                yield frame;
                            }
                        }
                        Err(error) => {
                            if let Some(permit) = delivery.take()
                                && let Err(authorization_error) =
                                    permit.fail(error.clone()).await
                            {
                                for frame in encoder.encode_bitrouter_error(&authorization_error) {
                                    yield frame;
                                }
                                return;
                            }
                            for f in encoder.encode_bitrouter_error(&error) {
                                yield f;
                            }
                            return;
                        }
                    }
                },
                Err(e) => {
                    // HTTP status is immutable after streaming begins. Emit a
                    // typed protocol-shaped terminal event instead.
                    for f in encoder.encode_bitrouter_error(&e) {
                        yield f;
                    }
                    return;
                }
            }
        }
        match encoder.finish() {
            Ok(frames) => {
                for f in frames {
                    yield f;
                }
            }
            Err(error) => {
                for f in encoder.encode_bitrouter_error(&error) {
                    yield f;
                }
            }
        }
    };

    let with_keepalive = SseKeepaliveStream::new(frame_stream, keepalive);
    let byte_stream =
        with_keepalive.map(|frame: SseFrame| Ok::<_, Infallible>(frame.to_wire().into_bytes()));

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(byte_stream))
        .unwrap_or_else(|e| {
            BitrouterError::internal(format!("building stream response: {e}")).into_response()
        })
}

impl IntoResponse for BitrouterError {
    fn into_response(self) -> Response {
        match self {
            BitrouterError::UpstreamBadRequest { error } => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": error})),
            )
                .into_response(),
            error => {
                let status = StatusCode::from_u16(error.status())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                let body = Json(serde_json::json!({
                    "error": {
                        "message": error.public_message(),
                        "type": error.error_type(),
                        "code": error.error_code(),
                    }
                }));
                //.4 — payment / rate-limit responses must carry the headers
                // that auto-paying clients (e.g. the MPP autopay flow,) and
                // well-behaved API consumers expect. RFC 7235 §4.1 for
                // WWW-Authenticate, RFC 7231 §7.1.3 for Retry-After.
                let mut response = (status, body).into_response();
                apply_error_headers(&mut response, &error);
                response
            }
        }
    }
}

fn apply_error_headers(response: &mut Response, error: &BitrouterError) {
    match error {
        BitrouterError::Unauthorized(_) => {
            // RFC 7235 §3.1: a 401 MUST include a `WWW-Authenticate`
            // header field containing at least one challenge applicable
            // to the resource. BitRouter's primary credential is a virtual
            // API key (`Authorization: Bearer <brvk_...>`).
            if let Ok(v) = header::HeaderValue::from_str("Bearer realm=\"bitrouter\"") {
                response.headers_mut().insert(header::WWW_AUTHENTICATE, v);
            }
        }
        BitrouterError::PaymentRequired(_) => {
            // 402 + WWW-Authenticate: our scheme name (`Bitrouter-MPP`)
            // and params predate the mpp.dev finalised wire format and
            // remain compatible with v0 clients ( will revisit
            // alignment with <https://mpp.dev/protocol/http-402>).
            if let Ok(v) = header::HeaderValue::from_str(
                "Bitrouter-MPP realm=\"bitrouter\", scheme=\"tempo-voucher\"",
            ) {
                response.headers_mut().insert(header::WWW_AUTHENTICATE, v);
            }
        }
        BitrouterError::UpstreamAuth {
            www_authenticate: Some(challenge),
            ..
        } => {
            if let Ok(value) = header::HeaderValue::from_str(challenge) {
                response
                    .headers_mut()
                    .insert(header::WWW_AUTHENTICATE, value);
            }
        }
        BitrouterError::RateLimited {
            retry_after: Some(secs),
        } => {
            if let Ok(v) = header::HeaderValue::from_str(&secs.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        BitrouterError::UpstreamRateLimited { retry_after, .. } => {
            if let Some(secs) = retry_after
                && let Ok(v) = header::HeaderValue::from_str(&secs.to_string())
            {
                response.headers_mut().insert(header::RETRY_AFTER, v);
            }
            response.headers_mut().insert(
                header::HeaderName::from_static("x-bitrouter-error-source"),
                header::HeaderValue::from_static("upstream"),
            );
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PromptTransform;
    use crate::language_model::executor::{Executor, MockExecutor, MockResponse};
    use crate::language_model::routing::StaticRoutingTable;
    use crate::language_model::settlement::{RequiredFinalizationContext, RequiredFinalizer};
    use crate::language_model::types::{
        ApiProtocol, AuthScheme, ExecutionResult, Prompt, RoutingTarget,
    };
    use crate::language_model::{
        HookDecision, PipelineBuilder, PipelineContext, PreRequestHook, StreamPartStream,
    };
    use async_trait::async_trait;
    use axum::body::to_bytes;
    use axum::http::{Request, header};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tower::ServiceExt;

    fn annotated_tool_result(schema: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "tools": [{
                "name": "deploy",
                "inputSchema": schema
            }]
        })
    }

    #[test]
    fn mcp_parameter_headers_validate_nested_primitive_arguments() {
        let tools = annotated_tool_result(serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "object",
                    "properties": {
                        "region": {"type": "string", "x-mcp-header": "Region"}
                    }
                },
                "count": {"type": "integer", "x-mcp-header": "Count"},
                "enabled": {"type": "boolean", "x-mcp-header": "Enabled"}
            }
        }));
        let params = serde_json::json!({
            "name": "deploy",
            "arguments": {
                "target": {"region": "us-east"},
                "count": 42,
                "enabled": true
            }
        });
        let mut headers = HeaderMap::new();
        headers.insert(
            "mcp-param-region",
            "=?base64?dXMtZWFzdA==?=".parse().unwrap(),
        );
        headers.insert("mcp-param-count", "42.0".parse().unwrap());
        headers.insert("mcp-param-enabled", "true".parse().unwrap());

        validate_mcp_tool_parameter_headers(&headers, &params, &tools)
            .expect("matching parameter headers are valid");
    }

    #[test]
    fn mcp_parameter_headers_reject_missing_mismatched_and_spurious_values() {
        let tools = annotated_tool_result(serde_json::json!({
            "type": "object",
            "properties": {
                "tenant": {"type": "string", "x-mcp-header": "Tenant"}
            }
        }));
        let params = serde_json::json!({
            "name": "deploy",
            "arguments": {"tenant": "alpha"}
        });
        let missing = validate_mcp_tool_parameter_headers(&HeaderMap::new(), &params, &tools)
            .expect_err("body value requires a header");
        assert!(missing.contains("missing required Mcp-Param-Tenant"));

        let mut mismatched = HeaderMap::new();
        mismatched.insert("mcp-param-tenant", "beta".parse().unwrap());
        let mismatch = validate_mcp_tool_parameter_headers(&mismatched, &params, &tools)
            .expect_err("header and body must match");
        assert!(mismatch.contains("does not match"));

        let params_without_tenant = serde_json::json!({
            "name": "deploy",
            "arguments": {"tenant": null}
        });
        let spurious =
            validate_mcp_tool_parameter_headers(&mismatched, &params_without_tenant, &tools)
                .expect_err("header must be absent for null arguments");
        assert!(spurious.contains("absent or null"));
    }

    #[test]
    fn mcp_parameter_header_annotations_must_be_reachable_and_unique() {
        let unreachable = annotated_tool_result(serde_json::json!({
            "type": "object",
            "properties": {
                "values": {
                    "type": "array",
                    "items": {"type": "string", "x-mcp-header": "Value"}
                }
            }
        }));
        let params = serde_json::json!({"name": "deploy", "arguments": {}});
        let error = validate_mcp_tool_parameter_headers(&HeaderMap::new(), &params, &unreachable)
            .expect_err("array annotations are not statically reachable");
        assert!(error.contains("properties only"));

        let duplicate = annotated_tool_result(serde_json::json!({
            "type": "object",
            "properties": {
                "a": {"type": "string", "x-mcp-header": "Tenant"},
                "b": {"type": "string", "x-mcp-header": "tenant"}
            }
        }));
        let error = validate_mcp_tool_parameter_headers(&HeaderMap::new(), &params, &duplicate)
            .expect_err("header annotation names are case-insensitively unique");
        assert!(error.contains("duplicate case-insensitive"));
    }

    struct CountingExecutor {
        calls: Arc<AtomicUsize>,
        inner: MockExecutor,
    }

    struct RecoveringDrainFinalizer {
        attempts: Arc<AtomicUsize>,
        recovered: Arc<AtomicBool>,
    }

    struct RewriteModel(&'static str);

    impl PromptTransform for RewriteModel {
        fn apply(&self, prompt: &mut Prompt) {
            prompt.model = self.0.to_string();
        }
    }

    struct RecordModelIntent(Arc<std::sync::Mutex<Option<(String, String)>>>);

    #[async_trait]
    impl PreRequestHook for RecordModelIntent {
        async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision> {
            let observed = (ctx.original_model().to_string(), ctx.model().to_string());
            match self.0.lock() {
                Ok(mut slot) => *slot = Some(observed),
                Err(poisoned) => *poisoned.into_inner() = Some(observed),
            }
            Ok(HookDecision::Allow)
        }
    }

    #[async_trait]
    impl RequiredFinalizer for RecoveringDrainFinalizer {
        async fn finalize(&self, _ctx: &RequiredFinalizationContext) -> Result<()> {
            Ok(())
        }

        async fn drain_pending_work(&self) -> Result<()> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.recovered.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(BitrouterError::internal(
                    "private production required drain failure",
                ))
            }
        }
    }

    #[async_trait]
    impl Executor for CountingExecutor {
        async fn execute(
            &self,
            target: &RoutingTarget,
            prompt: &Prompt,
            ctx: &PipelineContext,
        ) -> Result<ExecutionResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.execute(target, prompt, ctx).await
        }

        async fn execute_stream(
            &self,
            target: &RoutingTarget,
            prompt: &Prompt,
            ctx: &PipelineContext,
        ) -> Result<StreamPartStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.execute_stream(target, prompt, ctx).await
        }
    }

    fn test_state_with_models() -> AppState {
        test_state_with_executor(Arc::new(MockExecutor::always_text("ok")))
    }

    #[tokio::test]
    async fn google_slash_selectors_and_actions_reach_the_pipeline()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let calls = Arc::new(AtomicUsize::new(0));
        let scripted = (0..2)
            .flat_map(|_| {
                [
                    MockResponse::Generate(crate::language_model::GenerateResult {
                        content: vec![crate::language_model::Content::Text {
                            text: "ok".into(),
                            provider_metadata: Default::default(),
                        }],
                        usage: None,
                        finish_reason: Some(crate::language_model::FinishReason::Stop),
                        response_id: Some("response-fixture".into()),
                        stop_details: None,
                        provider_metadata: Default::default(),
                    }),
                    MockResponse::Stream(vec![
                        crate::language_model::StreamPart::TextDelta { text: "ok".into() },
                        crate::language_model::StreamPart::Finish {
                            reason: crate::language_model::FinishReason::Stop,
                        },
                    ]),
                ]
            })
            .collect();
        let table = StaticRoutingTable::new();
        table.insert(
            "gpt-5.5",
            vec![RoutingTarget {
                provider_name: "fixture".into(),
                service_id: "gpt-5.5".into(),
                api_base: "https://fixture.invalid".into(),
                api_key: String::new(),
                api_protocol: ApiProtocol::ChatCompletions,
                chat_token_limit_field: None,
                chat_supports_store: None,
                chat_supports_stream_options: None,
                reasoning_effort: None,
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                auth_scheme: AuthScheme::Bearer,
                headers: Vec::new(),
            }],
        );
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(Arc::new(table))
            .executor(Arc::new(CountingExecutor {
                calls: calls.clone(),
                inner: MockExecutor::new(scripted),
            }));
        let app = build_router(AppState {
            language_model: Arc::new(builder.build()?),
            mcp: None,
            skip_auth: true,
            metrics_renderer: None,
            prompt_transforms: vec![Arc::new(RewriteModel("gpt-5.5"))],
        });
        for selector in ["bitrouter/coding", "bitrouter%2Fcoding"] {
            for action in ["generateContent", "streamGenerateContent"] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri(format!("/v1beta/models/{selector}:{action}"))
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}]}"#,
                            ))?,
                    )
                    .await?;
                assert_eq!(response.status(), axum::http::StatusCode::OK);
                assert!(
                    !to_bytes(response.into_body(), MAX_BODY_BYTES)
                        .await?
                        .is_empty()
                );
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        for invalid in [
            "bitrouter/coding:unknown",
            "bitrouter/coding",
            ":generateContent",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/v1beta/models/{invalid}"))
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from("{}"))?,
                )
                .await?;
            assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "invalid action must not execute upstream"
        );
        Ok(())
    }

    fn test_state_with_executor(executor: Arc<dyn Executor>) -> AppState {
        test_state_with_executor_and_server_tools(executor, false)
    }

    fn test_state_with_executor_and_server_tools(
        executor: Arc<dyn Executor>,
        enable_server_tools: bool,
    ) -> AppState {
        let table = StaticRoutingTable::new();
        table.insert(
            "gpt-5.5",
            vec![RoutingTarget {
                provider_name: "openai-codex".to_string(),
                service_id: "gpt-5.5".to_string(),
                api_base: "https://example.invalid".to_string(),
                api_key: "test-key".to_string(),
                api_protocol: ApiProtocol::Responses,
                chat_token_limit_field: None,
                chat_supports_store: None,
                chat_supports_stream_options: None,
                reasoning_effort: None,
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                auth_scheme: AuthScheme::XApiKey,
                headers: Vec::new(),
            }],
        );
        let mut builder = PipelineBuilder::new();
        builder.routing_table(Arc::new(table)).executor(executor);
        if enable_server_tools {
            builder.server_tool_loop(Arc::new(
                crate::language_model::server_tools::loop_controller::ServerToolLoop::new(
                    crate::language_model::server_tools::toolset::ToolsetRegistry::new(Vec::new()),
                    crate::language_model::server_tools::config::ServerToolLoopConfig::default(),
                    Arc::new(crate::language_model::server_tools::approval::AllowAll),
                ),
            ));
        }
        let pipeline = builder.build().unwrap();
        AppState {
            language_model: Arc::new(pipeline),
            mcp: None,
            skip_auth: true,
            metrics_renderer: None,
            prompt_transforms: vec![],
        }
    }

    #[tokio::test]
    async fn http_ingress_preserves_model_before_prompt_transforms() {
        let table = StaticRoutingTable::new();
        table.insert(
            "gpt-5.5",
            vec![RoutingTarget {
                provider_name: "openai-codex".to_string(),
                service_id: "gpt-5.5".to_string(),
                api_base: "https://example.invalid".to_string(),
                api_key: "test-key".to_string(),
                api_protocol: ApiProtocol::Responses,
                chat_token_limit_field: None,
                chat_supports_store: None,
                chat_supports_stream_options: None,
                reasoning_effort: None,
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                auth_scheme: AuthScheme::XApiKey,
                headers: Vec::new(),
            }],
        );
        let observed = Arc::new(std::sync::Mutex::new(None));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(Arc::new(table))
            .executor(Arc::new(MockExecutor::always_text("ok")))
            .pre_request_hook(RecordModelIntent(Arc::clone(&observed)));
        let state = AppState {
            language_model: Arc::new(builder.build().unwrap()),
            mcp: None,
            skip_auth: true,
            metrics_renderer: None,
            prompt_transforms: vec![Arc::new(RewriteModel("gpt-5.5"))],
        };
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "model":"caller-model",
                    "messages":[{"role":"user","content":"hello"}]
                })
                .to_string(),
            ))
            .unwrap();

        let _response = build_router(state).oneshot(request).await.unwrap();

        let captured = match observed.lock() {
            Ok(slot) => slot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        assert_eq!(
            captured,
            Some(("caller-model".to_string(), "gpt-5.5".to_string()))
        );
    }

    async fn models_json(user_agent: Option<&str>) -> serde_json::Value {
        let mut builder = Request::builder().uri("/v1/models");
        if let Some(ua) = user_agent {
            builder = builder.header(header::USER_AGENT, ua);
        }
        let response = build_router(test_state_with_models())
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn v1_models_keeps_openai_shape_for_generic_clients() {
        let body = models_json(None).await;
        assert_eq!(body["object"], serde_json::json!("list"));
        assert!(body.get("data").is_some());
        assert!(
            body.get("models").is_none(),
            "generic OpenAI-compatible clients should keep the existing response shape: {body}"
        );
    }

    #[tokio::test]
    async fn v1_models_adds_codex_models_field_for_codex_user_agent() {
        let body = models_json(Some("codex-cli/0.142.5")).await;
        assert_eq!(body["object"], serde_json::json!("list"));
        assert_eq!(body["data"][0]["id"], serde_json::json!("gpt-5.5"));
        assert_eq!(
            body["models"][0]["id"],
            serde_json::json!("gpt-5.5"),
            "Codex CLI expects a top-level models field while the OpenAI data field remains present"
        );
    }

    #[test]
    fn payment_required_emits_www_authenticate() {
        let response =
            BitrouterError::PaymentRequired("send a Tempo voucher".to_string()).into_response();
        assert_eq!(response.status().as_u16(), 402);
        let www_auth = response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .expect("402 must carry WWW-Authenticate")
            .to_str()
            .unwrap();
        assert!(www_auth.contains("Bitrouter-MPP"));
        assert!(www_auth.contains("tempo-voucher"));
    }

    #[tokio::test]
    async fn upstream_diagnostics_are_not_exposed_in_http_errors() {
        let response = BitrouterError::Upstream {
            status: 500,
            message: "provider secret stack trace".to_string(),
        }
        .into_response();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["message"], "upstream request failed");
        assert_eq!(body["error"]["code"], "upstream_bad_gateway");
        assert!(!String::from_utf8_lossy(&bytes).contains("secret"));
    }

    #[tokio::test]
    async fn mcp_preflight_rate_limit_keeps_status_headers_and_safe_message()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let response = mcp_pipeline_error_response(
            serde_json::json!(7),
            &BitrouterError::UpstreamRateLimited {
                retry_after: Some(12),
                detail: None,
            },
        );

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "12");
        assert_eq!(response.headers()["x-bitrouter-error-source"], "upstream");
        let bytes = to_bytes(response.into_body(), 64 * 1024).await?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        assert_eq!(value["error"]["message"], "upstream rate limited");
        Ok(())
    }

    #[tokio::test]
    async fn mcp_preflight_upstream_diagnostics_are_not_exposed()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let response = mcp_pipeline_error_response(
            serde_json::json!(7),
            &BitrouterError::Upstream {
                status: 502,
                message: "provider secret stack trace".into(),
            },
        );

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        assert_eq!(value["error"]["message"], "upstream request failed");
        assert!(!String::from_utf8_lossy(&bytes).contains("secret"));
        Ok(())
    }

    #[test]
    fn mcp_preflight_upstream_auth_preserves_valid_challenges() {
        for status in [401, 403] {
            let response = mcp_pipeline_error_response(
                serde_json::json!(7),
                &BitrouterError::UpstreamAuth {
                    status,
                    www_authenticate: Some(
                        "Bearer realm=\"upstream\", scope=\"files:read\"".into(),
                    ),
                    required_scope: Some("files:read".into()),
                },
            );

            assert_eq!(response.status().as_u16(), status);
            assert_eq!(
                response.headers()[header::WWW_AUTHENTICATE],
                "Bearer realm=\"upstream\", scope=\"files:read\""
            );
        }
    }

    #[test]
    fn mcp_preflight_upstream_auth_omits_malformed_challenge() {
        let response = mcp_pipeline_error_response(
            serde_json::json!(7),
            &BitrouterError::UpstreamAuth {
                status: 401,
                www_authenticate: Some("Bearer\nsecret".into()),
                required_scope: None,
            },
        );

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(header::WWW_AUTHENTICATE).is_none());
    }

    #[tokio::test]
    async fn mcp_midstream_upstream_diagnostics_are_not_exposed()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let stream = futures::stream::once(async {
            Err(BitrouterError::Upstream {
                status: 502,
                message: "provider secret stack trace".into(),
            })
        })
        .boxed();
        let response = sse_response(
            serde_json::json!(7),
            "tools/call".to_string(),
            false,
            stream,
        );

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await?;
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("upstream request failed"));
        assert!(!body.contains("secret"));
        Ok(())
    }

    #[test]
    fn unauthorized_emits_www_authenticate_bearer() {
        // RFC 7235 §3.1: 401 MUST include WWW-Authenticate.
        let response = BitrouterError::Unauthorized("no key".to_string()).into_response();
        assert_eq!(response.status().as_u16(), 401);
        let www_auth = response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .expect("401 must carry WWW-Authenticate (RFC 7235 §3.1)")
            .to_str()
            .unwrap();
        assert!(
            www_auth.starts_with("Bearer "),
            "401 challenge should be Bearer, got: {www_auth}"
        );
    }

    #[test]
    fn rate_limited_emits_retry_after_when_present() {
        let response = BitrouterError::RateLimited {
            retry_after: Some(42),
        }
        .into_response();
        assert_eq!(response.status().as_u16(), 429);
        let retry = response
            .headers()
            .get(header::RETRY_AFTER)
            .expect("429 with retry_after must carry Retry-After")
            .to_str()
            .unwrap();
        assert_eq!(retry, "42");
    }

    #[test]
    fn rate_limited_omits_retry_after_when_unknown() {
        let response = BitrouterError::RateLimited { retry_after: None }.into_response();
        assert_eq!(response.status().as_u16(), 429);
        assert!(
            response.headers().get(header::RETRY_AFTER).is_none(),
            "no Retry-After when the daemon doesn't know how long to wait"
        );
    }

    #[test]
    fn inbound_protocol_hint_is_added_for_prompt_transforms_and_observers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-bitrouter-protocol", "responses".parse().unwrap());

        add_inbound_protocol_hint(&mut headers, &ApiProtocol::Messages);

        assert_eq!(
            headers
                .get("x-bitrouter-inbound-protocol")
                .and_then(|v| v.to_str().ok()),
            Some("messages")
        );
        assert_eq!(
            headers
                .get("x-bitrouter-protocol")
                .and_then(|v| v.to_str().ok()),
            Some("responses"),
            "operator/client explicit protocol hint should remain available"
        );
    }

    #[test]
    fn request_id_hint_prefers_existing_capture_header_and_is_preserved() {
        let mut headers = HeaderMap::new();
        headers.insert("x-bitrouter-request-id", "bench-req-001".parse().unwrap());

        let request_id = add_request_id_hint(&mut headers).unwrap();

        assert_eq!(request_id, "bench-req-001");
        assert_eq!(
            headers
                .get("x-bitrouter-request-id")
                .and_then(|v| v.to_str().ok()),
            Some("bench-req-001")
        );
    }

    #[test]
    fn request_id_hint_inserts_generated_id_when_missing() {
        let mut headers = HeaderMap::new();

        let request_id = add_request_id_hint(&mut headers).unwrap();

        assert!(!request_id.is_empty());
        assert_eq!(
            headers
                .get("x-bitrouter-request-id")
                .and_then(|v| v.to_str().ok()),
            Some(request_id.as_str())
        );
    }

    #[tokio::test]
    async fn responses_rejects_overlong_request_id_before_upstream_for_both_modes() {
        for stream in [false, true] {
            let calls = Arc::new(AtomicUsize::new(0));
            let executor = Arc::new(CountingExecutor {
                calls: calls.clone(),
                inner: MockExecutor::always_text("must not run"),
            });
            let response = build_router(test_state_with_executor(executor))
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/responses")
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(BITROUTER_REQUEST_ID_HEADER, "x".repeat(129))
                        .body(Body::from(
                            serde_json::json!({
                                "model": "gpt-5.5",
                                "input": "ping",
                                "stream": stream
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn upstream_rate_limit_has_wrapped_code_source_and_retry_after() {
        let response = BitrouterError::UpstreamRateLimited {
            retry_after: Some(12),
            detail: None,
        }
        .into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "12");
        assert_eq!(response.headers()["x-bitrouter-error-source"], "upstream");
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(value["error"]["type"], "rate_limit_error");
        assert_eq!(value["error"]["code"], "upstream_rate_limited");
        assert_eq!(value["error"]["message"], "upstream rate limited");
    }

    #[tokio::test]
    async fn upstream_bad_request_passthroughs_object_without_invented_metadata() {
        let response = BitrouterError::UpstreamBadRequest {
            error: serde_json::json!({
                "message": "max_tokens rejected",
                "param": "max_tokens"
            }),
        }
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response.headers().get("x-bitrouter-error-source").is_none());
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "error": {
                    "message": "max_tokens rejected",
                    "param": "max_tokens"
                }
            })
        );
    }

    #[tokio::test]
    async fn upstream_bad_request_passthroughs_string() {
        let response = BitrouterError::UpstreamBadRequest {
            error: serde_json::json!("bad temperature"),
        }
        .into_response();

        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(value, serde_json::json!({"error": "bad temperature"}));
    }

    #[tokio::test]
    async fn streaming_preflight_rate_limit_keeps_http_429() {
        let state =
            test_state_with_executor(Arc::new(MockExecutor::new(vec![MockResponse::Error(
                BitrouterError::UpstreamRateLimited {
                    retry_after: Some(7),
                    detail: None,
                },
            )])));
        let response = build_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "model": "gpt-5.5",
                            "messages": [{"role": "user", "content": "ping"}],
                            "stream": true
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_ne!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
        assert_eq!(response.headers()[header::RETRY_AFTER], "7");
    }

    #[tokio::test]
    async fn streaming_preflight_upstream_bad_request_keeps_http_400() {
        let state =
            test_state_with_executor(Arc::new(MockExecutor::new(vec![MockResponse::Error(
                BitrouterError::UpstreamBadRequest {
                    error: serde_json::json!({
                        "message": "temperature is unsupported",
                        "param": "temperature"
                    }),
                },
            )])));
        let response = build_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "model": "gpt-5.5",
                            "messages": [{"role": "user", "content": "ping"}],
                            "stream": true
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_ne!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );
        assert!(response.headers().get("x-bitrouter-error-source").is_none());
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "error": {
                    "message": "temperature is unsupported",
                    "param": "temperature"
                }
            })
        );
    }

    #[tokio::test]
    async fn streaming_preflight_rate_limit_with_server_tools_keeps_http_429()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let state = test_state_with_executor_and_server_tools(
            Arc::new(MockExecutor::new(vec![MockResponse::Error(
                BitrouterError::UpstreamRateLimited {
                    retry_after: Some(7),
                    detail: None,
                },
            )])),
            true,
        );
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "model": "gpt-5.5",
                    "messages": [{"role": "user", "content": "ping"}],
                    "stream": true
                })
                .to_string(),
            ))?;
        let response = build_router(state).oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_ne!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );
        assert_eq!(response.headers()[header::RETRY_AFTER], "7");
        assert_eq!(response.headers()["x-bitrouter-error-source"], "upstream");
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn graceful_shutdown_waits_for_required_drain_recovery_after_server_stops()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let accepting = Arc::new(AtomicBool::new(true));
        let attempts = Arc::new(AtomicUsize::new(0));
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let server_accepting = accepting.clone();
        let server = async move {
            stop_rx
                .await
                .map_err(|_| BitrouterError::internal("test server shutdown sender disappeared"))?;
            server_accepting.store(false, Ordering::SeqCst);
            Ok(())
        };
        let drain_attempts = attempts.clone();
        let drain_accepting = accepting.clone();
        let shutdown = tokio::spawn(complete_graceful_shutdown(
            server,
            move || {
                let attempt = drain_attempts.fetch_add(1, Ordering::SeqCst);
                let accepting = drain_accepting.load(Ordering::SeqCst);
                async move {
                    assert!(
                        !accepting,
                        "required drain ran while requests were accepted"
                    );
                    if attempt == 0 {
                        Err(BitrouterError::internal("private required drain failure"))
                    } else {
                        Ok(7)
                    }
                }
            },
            std::time::Duration::from_millis(250),
        ));

        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
        stop_tx
            .send(())
            .map_err(|_| "test server shutdown receiver disappeared")?;
        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(
            !shutdown.is_finished(),
            "one drain error completed shutdown"
        );

        tokio::time::advance(std::time::Duration::from_millis(249)).await;
        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(shutdown.await??, 7);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        Ok(())
    }

    #[tokio::test]
    async fn external_shutdown_entrypoint_uses_required_pipeline_drain()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let attempts = Arc::new(AtomicUsize::new(0));
        let recovered = Arc::new(AtomicBool::new(false));
        let finalizer = RecoveringDrainFinalizer {
            attempts: attempts.clone(),
            recovered: recovered.clone(),
        };
        let app = App::builder()
            .language_model(move |builder| {
                builder
                    .routing_table(Arc::new(StaticRoutingTable::new()))
                    .executor(Arc::new(MockExecutor::always_text("unused")))
                    .required_finalizer(finalizer);
            })
            .build()?;
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            app.serve_with_shutdown("127.0.0.1:0", async move {
                let _ = shutdown_rx.await;
            })
            .await
        });
        shutdown_tx
            .send(())
            .map_err(|_| "external shutdown receiver disappeared")?;
        tokio::time::timeout(Duration::from_secs(2), async {
            while attempts.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| "required pipeline drain did not start")?;
        assert!(
            !server.is_finished(),
            "the external shutdown entry point swallowed the required drain error"
        );

        recovered.store(true, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .map_err(|_| "server did not finish after required drain recovery")???;
        assert!(attempts.load(Ordering::SeqCst) >= 2);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn persistent_required_drain_failure_stays_serial_and_pending()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let accepting = Arc::new(AtomicBool::new(true));
        let attempts = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let server_accepting = accepting.clone();
        let server = async move {
            server_accepting.store(false, Ordering::SeqCst);
            Ok(())
        };
        let drain_attempts = attempts.clone();
        let drain_active = active.clone();
        let drain_max_active = max_active.clone();
        let drain_accepting = accepting.clone();
        let shutdown = tokio::spawn(complete_graceful_shutdown(
            server,
            move || {
                drain_attempts.fetch_add(1, Ordering::SeqCst);
                let concurrent = drain_active.fetch_add(1, Ordering::SeqCst) + 1;
                drain_max_active.fetch_max(concurrent, Ordering::SeqCst);
                let accepting = drain_accepting.load(Ordering::SeqCst);
                let active = drain_active.clone();
                async move {
                    assert!(
                        !accepting,
                        "required drain ran while requests were accepted"
                    );
                    active.fetch_sub(1, Ordering::SeqCst);
                    Err(BitrouterError::internal("private persistent drain failure"))
                }
            },
            std::time::Duration::from_millis(250),
        ));

        tokio::task::yield_now().await;
        let retained_active_references = Arc::strong_count(&active);
        for _ in 0..64 {
            tokio::time::advance(std::time::Duration::from_millis(250)).await;
            tokio::task::yield_now().await;
        }
        assert!(!shutdown.is_finished());
        assert_eq!(attempts.load(Ordering::SeqCst), 65);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(max_active.load(Ordering::SeqCst), 1);
        assert_eq!(Arc::strong_count(&active), retained_active_references);
        assert!(!accepting.load(Ordering::SeqCst));
        shutdown.abort();
        assert!(shutdown.await.is_err());
        Ok(())
    }
}
