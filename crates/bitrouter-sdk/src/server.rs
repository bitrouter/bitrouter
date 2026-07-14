//! axum HTTP server — gated behind the `server` feature.
//!
//! Wires all four inbound protocols to the `language_model` pipeline:
//! - `POST /v1/messages` — Messages
//! - `POST /v1/chat/completions` — Chat Completions
//! - `POST /v1/responses` — Responses
//! - `POST /v1beta/models/{model_action}` — Google `generateContent` /
//!   `streamGenerateContent`
//!
//! Each handler parses the inbound body with that protocol's adapter, runs the
//! pipeline, and renders the result back in the **same** inbound protocol —
//! the outbound (provider) protocol is chosen per routing target.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, serve};
use futures::StreamExt;

use crate::app::App;
use crate::caller::CallerContext;
use crate::error::{BitrouterError, Result};
use crate::language_model::Pipeline;
use crate::language_model::protocol::{inbound_adapter_for, sanitize_model_name};
use crate::language_model::stream::{SseFrame, SseKeepaliveStream};
use crate::language_model::types::{ApiProtocol, PipelineRequest};
use crate::mcp;
use crate::metrics::MetricsRenderer;

const BITROUTER_REQUEST_ID_HEADER: &str = "x-bitrouter-request-id";

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
        self.serve_inner(listen, None).await
    }

    /// Like [`App::serve`], but with a host-supplied router wrapper applied
    /// after the SDK has mounted every route — used by `bitrouter-observe`
    /// to install a `tower-http` `TraceLayer` at HTTP ingress.
    pub async fn serve_with_router_wrapper<F>(&self, listen: &str, wrapper: F) -> Result<()>
    where
        F: Fn(Router) -> Router + Send + Sync + 'static,
    {
        self.serve_inner(listen, Some(Arc::new(wrapper))).await
    }

    async fn serve_inner(&self, listen: &str, wrapper: Option<RouterWrapper>) -> Result<()> {
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
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .map_err(|e| BitrouterError::internal(format!("bind {listen}: {e}")))?;
        tracing::info!(%listen, "bitrouter listening");
        // Graceful shutdown: on SIGINT/SIGTERM
        // stop accepting new connections and let in-flight requests finish.
        let drain_pipeline = pipeline.clone();
        serve(listener, router)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .map_err(|e| BitrouterError::internal(format!("serve: {e}")))?;
        // After the HTTP server drains, also wait for every detached client-
        // disconnect settlement task (StreamSettlementGuard::drop) and every
        // detached non-streaming execution (Pipeline::execute_detached).
        // Without this, SIGTERM during heavy traffic could drop those tasks
        // mid-await and lose receipts.
        let drained = drain_pipeline.drain_pending_settlements().await;
        if drained > 0 {
            tracing::info!(drained, "drained pending settlements on shutdown");
        }
        Ok(())
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
/// whole router in additional middleware (e.g. a `tower-http`
/// `TraceLayer` that creates the SERVER span at HTTP ingress).
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
    /// every route. Used to add an inbound HTTP `tower::Layer` (e.g. the
    /// observe plugin's `tower-http::trace::TraceLayer`) without coupling
    /// the SDK to OpenTelemetry or any other tracing backend.
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
        .route("/v1beta/models/{model_action}", post(generate_content));
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
/// v1.0 implements the JSON-RPC request/response shape only; the Streamable
/// HTTP SSE response variant is a documented follow-up. Spec refs:
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

    // Validate Streamable HTTP transport headers per spec. `Origin` MUST be
    // validated to defeat DNS-rebinding; an unsupported `MCP-Protocol-Version`
    // MUST be rejected with 400.
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
    let params = body
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    // MCP lifecycle methods are answered by the gateway itself — they negotiate
    // the client<->gateway session and MUST NOT be proxied to an upstream
    // executor (which dispatches only `tools/*`, `resources/*`, `prompts/*` and
    // rejects everything else as "method not found"). Without this, every
    // spec-compliant client (Claude clients, opencode, the MCP SDK) fails its
    // opening `initialize` over Streamable HTTP and never reaches a tool call;
    // only handshake-skipping callers (curl, `bitrouter tools`) worked before.
    // See <https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle>.
    match method.as_str() {
        "initialize" => {
            // Spec: echo the client's protocol version when we support it,
            // otherwise answer with our latest. `params.protocolVersion` is the
            // client's requested version.
            let protocol_version = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .filter(|v| MCP_SUPPORTED_PROTOCOL_VERSIONS.contains(v))
                .unwrap_or(MCP_SUPPORTED_PROTOCOL_VERSIONS[0]);
            return Json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": inbound_id,
                "result": {
                    "protocolVersion": protocol_version,
                    // The gateway proxies tool calls; resources/prompts are not
                    // advertised so clients don't probe upstreams that may not
                    // implement them.
                    "capabilities": { "tools": {} },
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
            return Json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": inbound_id,
                "result": {},
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
    let request = match selector {
        mcp::ServerSelector::Direct(server) => {
            mcp::McpRequest::direct(server, method, params, caller)
        }
        mcp::ServerSelector::Aggregate => mcp::McpRequest::aggregate(method, params, caller),
    }
    .with_headers(headers.clone());

    // SSE branch per the MCP Streamable HTTP spec — if the client opts in via
    // `Accept: text/event-stream` we return the JSON-RPC frames as `data:`
    // events. JSON clients get the buffered JSON shape (the existing path).
    if accepts_event_stream(&headers) {
        return match pipeline.execute_streaming(request).await {
            Ok(stream) => sse_response(inbound_id, stream),
            Err(e) => mcp_pipeline_error_response(inbound_id, &e),
        };
    }

    match pipeline.execute(request).await {
        Ok(response) => Json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": inbound_id,
            "result": response.result,
        }))
        .into_response(),
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
            Ok(mcp::McpStreamPart::Final(response)) => {
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
const MCP_SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Validates the MCP Streamable HTTP transport headers per the spec at
/// <https://modelcontextprotocol.io/specification/2025-06-18/basic/transports>.
/// `Origin`: MUST be validated by the server to defeat DNS rebinding — we accept
/// localhost / 127.0.0.1 / [::1] by default (the only safe default for a local
/// daemon binding to loopback). `MCP-Protocol-Version`: if present, MUST be a
/// version this server supports.
fn validate_mcp_transport_headers(headers: &HeaderMap) -> Result<()> {
    if let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok())
        && !is_safe_mcp_origin(origin)
    {
        return Err(BitrouterError::Forbidden(format!(
            "MCP Origin not allowed: '{origin}'. Local daemons accept only loopback origins."
        )));
    }
    if let Some(version) = headers
        .get("mcp-protocol-version")
        .and_then(|v| v.to_str().ok())
        && !MCP_SUPPORTED_PROTOCOL_VERSIONS.contains(&version)
    {
        return Err(BitrouterError::bad_request(format!(
            "unsupported MCP-Protocol-Version '{version}' (supported: {})",
            MCP_SUPPORTED_PROTOCOL_VERSIONS.join(", ")
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

/// Generate Content encodes the model and the streaming verb in the path segment, e.g.
/// `gemini-2.0-flash:generateContent` or `…:streamGenerateContent`.
async fn generate_content(
    State(state): State<AppState>,
    Path(model_action): Path<String>,
    headers: HeaderMap,
    Json(mut body): Json<serde_json::Value>,
) -> Response {
    let (model, action) = match model_action.rsplit_once(':') {
        Some((m, a)) => (m.to_string(), a.to_string()),
        None => {
            return BitrouterError::bad_request(
                "google path must be 'models/{model}:generateContent'",
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
    let request_id = add_request_id_hint(&mut headers);
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
    let prompt = match adapter.parse_request(body) {
        Ok(mut p) => {
            if let Some(model) = model_override {
                p.model = model;
            }
            p.model = sanitize_model_name(&p.model);
            // Ingress-time prompt transforms (e.g. the bitrouter/fusion model
            // alias): the prompt body is freely mutable here, before it enters
            // the pipeline that exposes it read-only downstream.
            for transform in &state.prompt_transforms {
                transform.apply_with_headers(&mut p, &headers);
            }
            p
        }
        Err(e) => return e.into_response(),
    };

    // `skip_auth` decides the starting caller: a synthesised local caller when
    // on, else a pre-auth anonymous placeholder for `AuthHook` to upgrade or
    // reject.
    let caller = if state.skip_auth {
        CallerContext::local()
    } else {
        CallerContext::anonymous()
    };
    let mut req = PipelineRequest::new(prompt.model.clone(), caller, prompt.clone());
    req.request_id = request_id;
    req.headers = headers;
    // Carry the inbound wire protocol so route resolution can prefer a native,
    // same-protocol upstream — a faithful round-trip instead of a lossy
    // cross-protocol translation.
    req.inbound_protocol = Some(inbound.clone());

    if prompt.stream {
        stream_response(
            state.language_model.clone(),
            req,
            inbound.clone(),
            &prompt.model,
        )
        .await
    } else {
        // `execute_detached`, not `execute`: a non-streaming request must run to
        // completion and settle even if the client disconnects (axum drops this
        // handler future on disconnect). The upstream bills us for the accepted
        // request regardless, so the customer must be billed too.
        match state.language_model.clone().execute_detached(req).await {
            Ok(resp) => match adapter.render_response(&resp.result, &prompt, &resp.request_id) {
                Ok(json) => Json(json).into_response(),
                Err(e) => e.into_response(),
            },
            Err(e) => e.into_response(),
        }
    }
}

fn add_inbound_protocol_hint(headers: &mut HeaderMap, inbound: &ApiProtocol) {
    if let Ok(value) = HeaderValue::from_str(inbound.as_str()) {
        headers.insert("x-bitrouter-inbound-protocol", value);
    }
}

fn add_request_id_hint(headers: &mut HeaderMap) -> String {
    if let Some(request_id) = headers
        .get(BITROUTER_REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return request_id.to_string();
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        headers.insert(BITROUTER_REQUEST_ID_HEADER, value);
    }
    request_id
}

/// Build a `text/event-stream` response: pipe the canonical part stream through
/// the inbound protocol's `StreamEncoder`, wrap it in `SseKeepaliveStream`, and
/// stream the wire bytes.
async fn stream_response(
    pipeline: Arc<Pipeline>,
    req: PipelineRequest,
    inbound: ApiProtocol,
    model: &str,
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
    let mut encoder = adapter.stream_encoder(&req.request_id, model);
    let keepalive = pipeline.keepalive_interval();

    // Route resolution and the upstream HTTP handshake happen before the SSE
    // response is constructed. Pre-stream failures therefore retain their real
    // HTTP status (notably upstream 429) instead of being trapped inside an
    // already-committed HTTP 200 event stream.
    let mut parts = match pipeline.execute_stream(req).await {
        Ok(parts) => parts,
        Err(error) => return error.into_response(),
    };

    let frame_stream = async_stream::stream! {
        while let Some(item) = parts.next().await {
            match item {
                Ok(part) => {
                    if let Ok(frames) = encoder.encode(&part) {
                        for f in frames {
                            yield f;
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
        if let Ok(frames) = encoder.finish() {
            for f in frames {
                yield f;
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
        BitrouterError::UpstreamRateLimited { retry_after } => {
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
    use crate::language_model::PipelineBuilder;
    use crate::language_model::executor::{Executor, MockExecutor, MockResponse};
    use crate::language_model::routing::StaticRoutingTable;
    use crate::language_model::types::{ApiProtocol, AuthScheme, RoutingTarget};
    use axum::body::to_bytes;
    use axum::http::{Request, header};
    use tower::ServiceExt;

    fn test_state_with_models() -> AppState {
        test_state_with_executor(Arc::new(MockExecutor::always_text("ok")))
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
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                auth_scheme: AuthScheme::XApiKey,
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
        let response = sse_response(serde_json::json!(7), stream);

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

        let request_id = add_request_id_hint(&mut headers);

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

        let request_id = add_request_id_hint(&mut headers);

        assert!(!request_id.is_empty());
        assert_eq!(
            headers
                .get("x-bitrouter-request-id")
                .and_then(|v| v.to_str().ok()),
            Some(request_id.as_str())
        );
    }

    #[tokio::test]
    async fn upstream_rate_limit_has_wrapped_code_source_and_retry_after() {
        let response = BitrouterError::UpstreamRateLimited {
            retry_after: Some(12),
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
}
