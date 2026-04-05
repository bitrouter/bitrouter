//! Warp filters for the MCP protocol.
//!
//! - `POST /mcp` — JSON-RPC dispatch for all MCP methods.
//! - `GET /mcp/sse` — SSE stream for change notifications.
//! - `POST /mcp/{name}` — per-server bridge endpoint (see [`mcp_bridge_filter`]).
//! - `GET /mcp/{name}/sse` — per-server bridge SSE stream.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use bitrouter_core::api::mcp::gateway::{
    McpCompletionServer, McpLoggingServer, McpPromptServer, McpResourceServer, McpServer,
    McpSubscriptionServer, McpToolServer, ToolCallHandler,
};
use bitrouter_core::api::mcp::types::McpGatewayError;
use bitrouter_core::api::mcp::types::{
    CallToolParams, CompleteParams, GetPromptParams, InitializeResult, JsonRpcId, JsonRpcMessage,
    JsonRpcResponse, ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult,
    ListToolsResult, LoggingCapability, PromptsCapability, ReadResourceParams, ReadResourceResult,
    ResourcesCapability, ServerCapabilities, ServerInfo, SetLoggingLevelParams,
    SubscribeResourceParams, ToolsCapability, UnsubscribeResourceParams, error_codes,
};
use bitrouter_core::observe::{
    CallerContext, ToolCallFailureEvent, ToolCallSuccessEvent, ToolObserveCallback,
    ToolRequestContext,
};
use tokio::time::Instant;
use tokio_stream::StreamExt;
use warp::Filter;

/// The MCP protocol version this server advertises.
const PROTOCOL_VERSION: &str = "2025-11-25";

/// Server name returned during initialization.
const SERVER_NAME: &str = "bitrouter";

/// Separator used in wire-format tool names exposed to downstream MCP clients.
///
/// Internally bitrouter namespaces tools as `"server/tool"`, but `/` is
/// invalid in many LLM function-name constraints (e.g. Gemini). On the wire
/// we emit `"server__tool"` and translate incoming calls back.
///
/// **Constraint:** Upstream MCP server names must not contain `__`, because
/// `from_wire_name` splits on the first `__` occurrence. A server named
/// `"my__srv"` with tool `"foo"` would produce the wire name `"my__srv__foo"`,
/// which would be incorrectly parsed as server `"my"`, tool `"srv__foo"`.
/// Server name validation enforces this at config load time.
const WIRE_SEPARATOR: &str = "__";

// ── Public entry points ─────────────────────────────────────────────

/// Combined MCP server filter: `POST /mcp` + `GET /mcp/sse`.
///
/// When `server` is `None` (no MCP configured), both endpoints return 404.
pub fn mcp_server_filter<T>(
    server: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: McpServer + 'static,
{
    mcp_jsonrpc_filter(server.clone(), None, None).or(mcp_sse_filter(server))
}

/// Combined MCP server filter with tool call observation.
///
/// The `account_filter` extracts a [`CallerContext`] per-request (e.g. from
/// JWT claims) so that observation events carry account information.
///
/// When `tool_call_handler` is provided, `tools/call` requests are dispatched
/// through it instead of through `McpToolServer::call_tool`. This allows
/// tool execution to be routed through the [`ToolRouter`] dispatch chain
/// independently of the MCP server capabilities.
pub fn mcp_server_filter_with_observe<T, A>(
    server: Option<Arc<T>>,
    tool_call_handler: Option<Arc<dyn ToolCallHandler>>,
    observer: Arc<dyn ToolObserveCallback>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: McpServer + 'static,
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    mcp_jsonrpc_filter_with_observe(server.clone(), tool_call_handler, observer, account_filter)
        .or(mcp_sse_filter(server))
}

/// Combined bridge filter for all configured bridge servers.
///
/// Routes `POST /mcp/{name}` and `GET /mcp/{name}/sse` to the bridge
/// identified by `{name}`.  Returns 404 for names not in the map.
///
/// **Routing note:** compose this filter *after* the aggregated
/// [`mcp_server_filter`] so that the static paths `POST /mcp` and
/// `GET /mcp/sse` are matched first.
pub fn mcp_bridge_filter<T>(
    bridges: Arc<HashMap<String, Arc<T>>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: McpServer + 'static,
{
    mcp_bridge_jsonrpc_filter(bridges.clone()).or(mcp_bridge_sse_filter(bridges))
}

// ── POST /mcp ───────────────────────────────────────────────────────

fn mcp_jsonrpc_filter<T>(
    server: Option<Arc<T>>,
    tool_call_handler: Option<Arc<dyn ToolCallHandler>>,
    observe_ctx: Option<McpObserveContext>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: McpServer + 'static,
{
    warp::path("mcp")
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::body::json::<serde_json::Value>())
        .and(warp::any().map(move || server.clone()))
        .and(warp::any().map(move || tool_call_handler.clone()))
        .and(warp::any().map(move || observe_ctx.clone()))
        .then(handle_jsonrpc_value::<T>)
}

fn mcp_jsonrpc_filter_with_observe<T, A>(
    server: Option<Arc<T>>,
    tool_call_handler: Option<Arc<dyn ToolCallHandler>>,
    observer: Arc<dyn ToolObserveCallback>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: McpServer + 'static,
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    let tch = tool_call_handler;
    warp::path("mcp")
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::body::json::<serde_json::Value>())
        .and(warp::any().map(move || server.clone()))
        .and(warp::any().map(move || tch.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and(account_filter)
        .then(
            |body: serde_json::Value,
             server: Option<Arc<T>>,
             tool_call_handler: Option<Arc<dyn ToolCallHandler>>,
             observer: Arc<dyn ToolObserveCallback>,
             caller: CallerContext| async move {
                let ctx = Some(McpObserveContext { observer, caller });
                handle_jsonrpc_value::<T>(body, server, tool_call_handler, ctx).await
            },
        )
}

async fn handle_jsonrpc_value<T: McpServer>(
    body: serde_json::Value,
    server: Option<Arc<T>>,
    tool_call_handler: Option<Arc<dyn ToolCallHandler>>,
    observe_ctx: Option<McpObserveContext>,
) -> Box<dyn warp::Reply> {
    let Some(server) = server else {
        return Box::new(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({
                "error": {"message": "MCP server not configured"}
            })),
            warp::http::StatusCode::NOT_FOUND,
        ));
    };

    // Try to parse as JSON-RPC message.
    let message: JsonRpcMessage = match serde_json::from_value(body) {
        Ok(msg) => msg,
        Err(e) => {
            let resp = JsonRpcResponse::error(
                JsonRpcId::Number(0),
                error_codes::PARSE_ERROR,
                format!("parse error: {e}"),
                None,
            );
            return Box::new(warp::reply::json(&resp));
        }
    };

    match message {
        JsonRpcMessage::Request(req) => {
            let resp = dispatch_request(
                &req.id,
                &req.method,
                req.params,
                &*server,
                tool_call_handler.as_deref(),
                &observe_ctx,
                None,
            )
            .await;
            Box::new(warp::reply::json(&resp))
        }
        JsonRpcMessage::Notification(notif) => {
            // Notifications are acknowledged without a response body.
            // Dispatch on known methods for future extensibility.
            match notif.method.as_str() {
                "notifications/initialized"
                | "notifications/cancelled"
                | "notifications/progress"
                | "notifications/roots/list_changed" => {}
                _ => {}
            }
            Box::new(warp::reply::with_status(
                warp::reply::json(&serde_json::json!({})),
                warp::http::StatusCode::ACCEPTED,
            ))
        }
    }
}

// ── JSON-RPC dispatch ───────────────────────────────────────────────

async fn dispatch_request<T: McpServer>(
    id: &JsonRpcId,
    method: &str,
    params: Option<serde_json::Value>,
    server: &T,
    tool_call_handler: Option<&dyn ToolCallHandler>,
    observe_ctx: &Option<McpObserveContext>,
    server_name: Option<&str>,
) -> JsonRpcResponse {
    match method {
        "initialize" => handle_initialize(id, server_name),
        "ping" => handle_ping(id),
        "tools/list" => handle_tools_list(id, server).await,
        "tools/call" => handle_tools_call(id, params, server, tool_call_handler, observe_ctx).await,
        "resources/list" => handle_resources_list(id, server).await,
        "resources/read" => handle_resources_read(id, params, server).await,
        "resources/templates/list" => handle_resource_templates_list(id, server).await,
        "resources/subscribe" => handle_resource_subscribe(id, params, server).await,
        "resources/unsubscribe" => handle_resource_unsubscribe(id, params, server).await,
        "prompts/list" => handle_prompts_list(id, server).await,
        "prompts/get" => handle_prompts_get(id, params, server).await,
        "logging/setLevel" => handle_set_level(id, params, server).await,
        "completion/complete" => handle_complete(id, params, server).await,
        _ => JsonRpcResponse::error(
            id.clone(),
            error_codes::METHOD_NOT_FOUND,
            format!("method not found: {method}"),
            None,
        ),
    }
}

fn handle_initialize(id: &JsonRpcId, server_name: Option<&str>) -> JsonRpcResponse {
    let (name, instructions) = match server_name {
        Some(name) => (name.to_string(), format!("BitRouter MCP Bridge — {name}")),
        None => (
            SERVER_NAME.to_string(),
            "BitRouter MCP Gateway — aggregated tools from multiple upstream MCP servers"
                .to_string(),
        ),
    };
    let result = InitializeResult {
        protocol_version: PROTOCOL_VERSION.to_string(),
        capabilities: ServerCapabilities {
            tools: Some(ToolsCapability {
                list_changed: Some(true),
            }),
            resources: Some(ResourcesCapability {
                list_changed: Some(true),
                subscribe: None,
            }),
            prompts: Some(PromptsCapability {
                list_changed: Some(true),
            }),
            logging: Some(LoggingCapability {}),
            completions: None,
        },
        server_info: ServerInfo {
            name,
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        },
        instructions: Some(instructions),
    };
    serialize_success(id, &result)
}

fn handle_ping(id: &JsonRpcId) -> JsonRpcResponse {
    JsonRpcResponse::success(id.clone(), serde_json::json!({}))
}

/// Extract and deserialize JSON-RPC params, returning an error response on failure.
fn extract_params<T: serde::de::DeserializeOwned>(
    id: &JsonRpcId,
    params: Option<serde_json::Value>,
    method: &str,
) -> Result<T, Box<JsonRpcResponse>> {
    let value = params.ok_or_else(|| {
        Box::new(JsonRpcResponse::error(
            id.clone(),
            error_codes::INVALID_PARAMS,
            format!("{method} requires params"),
            None,
        ))
    })?;
    serde_json::from_value(value).map_err(|e| {
        Box::new(JsonRpcResponse::error(
            id.clone(),
            error_codes::INVALID_PARAMS,
            format!("invalid params: {e}"),
            None,
        ))
    })
}

/// Serialize a result into a JSON-RPC success response, returning an internal
/// error response if serialization fails.
fn serialize_success(id: &JsonRpcId, result: &impl serde::Serialize) -> JsonRpcResponse {
    match serde_json::to_value(result) {
        Ok(value) => JsonRpcResponse::success(id.clone(), value),
        Err(e) => JsonRpcResponse::error(
            id.clone(),
            error_codes::INTERNAL_ERROR,
            format!("serialization error: {e}"),
            None,
        ),
    }
}

// ── GET /mcp/sse ────────────────────────────────────────────────────

fn mcp_sse_filter<T>(
    server: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: McpServer + 'static,
{
    warp::path!("mcp" / "sse")
        .and(warp::get())
        .and(warp::any().map(move || server.clone()))
        .and_then(handle_sse::<T>)
}

/// Build an SSE event stream from a broadcast receiver that emits a JSON-RPC
/// notification with the given method name on each signal.
fn notification_stream(
    rx: tokio::sync::broadcast::Receiver<()>,
    method: &'static str,
) -> impl tokio_stream::Stream<Item = Result<warp::sse::Event, Infallible>> {
    tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(move |item| match item {
        Ok(()) => {
            let notification = serde_json::json!({
                "jsonrpc": "2.0",
                "method": method
            });
            match serde_json::to_string(&notification) {
                Ok(data) => Some(Ok(warp::sse::Event::default().data(data))),
                Err(e) => {
                    tracing::warn!(method, error = %e, "failed to serialize SSE notification");
                    None
                }
            }
        }
        Err(_) => None,
    })
}

async fn handle_sse<T: McpServer>(
    server: Option<Arc<T>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let Some(server) = server else {
        return Err(warp::reject::not_found());
    };

    let tool_rx = server.subscribe_tool_changes();
    let tool_stream = notification_stream(tool_rx, "notifications/tools/list_changed");

    let resource_rx = server.subscribe_resource_changes();
    let resource_stream = notification_stream(resource_rx, "notifications/resources/list_changed");

    let prompt_rx = server.subscribe_prompt_changes();
    let prompt_stream = notification_stream(prompt_rx, "notifications/prompts/list_changed");

    // Send an initial comment event to signal the SSE connection is established.
    let initial = tokio_stream::once(Ok::<_, Infallible>(
        warp::sse::Event::default().comment("connected"),
    ));
    let merged = initial.chain(tool_stream.merge(resource_stream).merge(prompt_stream));

    Ok(warp::sse::reply(warp::sse::keep_alive().stream(merged)))
}

// ── Bridge filters: POST /mcp/{name} + GET /mcp/{name}/sse ─────────

fn mcp_bridge_jsonrpc_filter<T>(
    bridges: Arc<HashMap<String, Arc<T>>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: McpServer + 'static,
{
    warp::path("mcp")
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::body::json::<serde_json::Value>())
        .and(warp::any().map(move || bridges.clone()))
        .then(
            |name: String,
             body: serde_json::Value,
             bridges: Arc<HashMap<String, Arc<T>>>| async move {
                let server = bridges.get(&name).cloned();
                handle_jsonrpc_value_bridge::<T>(body, server, name).await
            },
        )
}

fn mcp_bridge_sse_filter<T>(
    bridges: Arc<HashMap<String, Arc<T>>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: McpServer + 'static,
{
    warp::path("mcp")
        .and(warp::path::param::<String>())
        .and(warp::path("sse"))
        .and(warp::path::end())
        .and(warp::get())
        .and(warp::any().map(move || bridges.clone()))
        .and_then(
            |name: String, bridges: Arc<HashMap<String, Arc<T>>>| async move {
                let server = bridges.get(&name).cloned();
                handle_sse::<T>(server).await
            },
        )
}

async fn handle_jsonrpc_value_bridge<T: McpServer>(
    body: serde_json::Value,
    server: Option<Arc<T>>,
    server_name: String,
) -> Box<dyn warp::Reply> {
    let Some(server) = server else {
        return Box::new(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({
                "error": {"message": "MCP bridge not found"}
            })),
            warp::http::StatusCode::NOT_FOUND,
        ));
    };

    let message: JsonRpcMessage = match serde_json::from_value(body) {
        Ok(msg) => msg,
        Err(e) => {
            let resp = JsonRpcResponse::error(
                JsonRpcId::Number(0),
                error_codes::PARSE_ERROR,
                format!("parse error: {e}"),
                None,
            );
            return Box::new(warp::reply::json(&resp));
        }
    };

    match message {
        JsonRpcMessage::Request(req) => {
            let resp = dispatch_request(
                &req.id,
                &req.method,
                req.params,
                &*server,
                None,
                &None,
                Some(&server_name),
            )
            .await;
            Box::new(warp::reply::json(&resp))
        }
        JsonRpcMessage::Notification(_) => Box::new(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({})),
            warp::http::StatusCode::ACCEPTED,
        )),
    }
}

// ── Tool handlers ───────────────────────────────────────────────────

/// Replace the first `/` with `WIRE_SEPARATOR` for wire-format names.
fn to_wire_name(internal: &str) -> String {
    match internal.split_once('/') {
        Some((server, tool)) => format!("{server}{WIRE_SEPARATOR}{tool}"),
        None => internal.to_owned(),
    }
}

/// Replace the first `WIRE_SEPARATOR` with `/` to recover the internal name.
fn from_wire_name(wire: &str) -> String {
    match wire.split_once(WIRE_SEPARATOR) {
        Some((server, tool)) => format!("{server}/{tool}"),
        None => wire.to_owned(),
    }
}

async fn handle_tools_list<T: McpToolServer>(id: &JsonRpcId, server: &T) -> JsonRpcResponse {
    let mut tools = server.list_tools().await;
    for tool in &mut tools {
        tool.name = to_wire_name(&tool.name);
    }
    let result = ListToolsResult {
        tools,
        next_cursor: None,
    };
    serialize_success(id, &result)
}

async fn handle_tools_call<T: McpToolServer>(
    id: &JsonRpcId,
    params: Option<serde_json::Value>,
    server: &T,
    tool_call_handler: Option<&dyn ToolCallHandler>,
    observe_ctx: &Option<McpObserveContext>,
) -> JsonRpcResponse {
    let call_params: CallToolParams = match extract_params(id, params, "tools/call") {
        Ok(p) => p,
        Err(resp) => return *resp,
    };

    let internal_name = from_wire_name(&call_params.name);
    let (server_name, tool_name) = internal_name
        .split_once('/')
        .unwrap_or(("unknown", &internal_name));
    let start = Instant::now();

    // When a ToolCallHandler is provided, dispatch through the protocol-neutral
    // ToolRouter chain. Otherwise fall back to the McpToolServer (bridge mode).
    let result = if let Some(handler) = tool_call_handler {
        handler
            .call_tool(&internal_name, call_params.arguments)
            .await
    } else {
        server
            .call_tool(&internal_name, call_params.arguments)
            .await
    };

    match result {
        Ok(result) => {
            emit_tool_success(observe_ctx, server_name, tool_name, start);
            serialize_success(id, &result)
        }
        Err(err) => {
            let err_str = err.to_string();
            emit_tool_failure(observe_ctx, server_name, tool_name, start, &err_str);
            let (code, message) = gateway_error_to_jsonrpc(&err);
            JsonRpcResponse::error(id.clone(), code, message, None)
        }
    }
}

// ── Resource handlers ───────────────────────────────────────────────

async fn handle_resources_list<T: McpResourceServer>(
    id: &JsonRpcId,
    server: &T,
) -> JsonRpcResponse {
    let resources = server.list_resources().await;
    let result = ListResourcesResult {
        resources,
        next_cursor: None,
    };
    serialize_success(id, &result)
}

async fn handle_resources_read<T: McpResourceServer>(
    id: &JsonRpcId,
    params: Option<serde_json::Value>,
    server: &T,
) -> JsonRpcResponse {
    let read_params: ReadResourceParams = match extract_params(id, params, "resources/read") {
        Ok(p) => p,
        Err(resp) => return *resp,
    };

    match server.read_resource(&read_params.uri).await {
        Ok(contents) => {
            let result = ReadResourceResult { contents };
            serialize_success(id, &result)
        }
        Err(err) => {
            let (code, message) = gateway_error_to_jsonrpc(&err);
            JsonRpcResponse::error(id.clone(), code, message, None)
        }
    }
}

async fn handle_resource_templates_list<T: McpResourceServer>(
    id: &JsonRpcId,
    server: &T,
) -> JsonRpcResponse {
    let templates = server.list_resource_templates().await;
    let result = ListResourceTemplatesResult {
        resource_templates: templates,
        next_cursor: None,
    };
    serialize_success(id, &result)
}

// ── Subscription handlers ───────────────────────────────────────────

async fn handle_resource_subscribe<T: McpSubscriptionServer>(
    id: &JsonRpcId,
    params: Option<serde_json::Value>,
    server: &T,
) -> JsonRpcResponse {
    let sub_params: SubscribeResourceParams =
        match extract_params(id, params, "resources/subscribe") {
            Ok(p) => p,
            Err(resp) => return *resp,
        };

    match server.subscribe_resource(&sub_params.uri).await {
        Ok(()) => JsonRpcResponse::success(id.clone(), serde_json::json!({})),
        Err(err) => {
            let (code, message) = gateway_error_to_jsonrpc(&err);
            JsonRpcResponse::error(id.clone(), code, message, None)
        }
    }
}

async fn handle_resource_unsubscribe<T: McpSubscriptionServer>(
    id: &JsonRpcId,
    params: Option<serde_json::Value>,
    server: &T,
) -> JsonRpcResponse {
    let unsub_params: UnsubscribeResourceParams =
        match extract_params(id, params, "resources/unsubscribe") {
            Ok(p) => p,
            Err(resp) => return *resp,
        };

    match server.unsubscribe_resource(&unsub_params.uri).await {
        Ok(()) => JsonRpcResponse::success(id.clone(), serde_json::json!({})),
        Err(err) => {
            let (code, message) = gateway_error_to_jsonrpc(&err);
            JsonRpcResponse::error(id.clone(), code, message, None)
        }
    }
}

// ── Prompt handlers ─────────────────────────────────────────────────

async fn handle_prompts_list<T: McpPromptServer>(id: &JsonRpcId, server: &T) -> JsonRpcResponse {
    let prompts = server.list_prompts().await;
    let result = ListPromptsResult {
        prompts,
        next_cursor: None,
    };
    serialize_success(id, &result)
}

async fn handle_prompts_get<T: McpPromptServer>(
    id: &JsonRpcId,
    params: Option<serde_json::Value>,
    server: &T,
) -> JsonRpcResponse {
    let get_params: GetPromptParams = match extract_params(id, params, "prompts/get") {
        Ok(p) => p,
        Err(resp) => return *resp,
    };

    match server
        .get_prompt(&get_params.name, get_params.arguments)
        .await
    {
        Ok(result) => serialize_success(id, &result),
        Err(err) => {
            let (code, message) = gateway_error_to_jsonrpc(&err);
            JsonRpcResponse::error(id.clone(), code, message, None)
        }
    }
}

// ── Logging handler ─────────────────────────────────────────────────

async fn handle_set_level<T: McpLoggingServer>(
    id: &JsonRpcId,
    params: Option<serde_json::Value>,
    server: &T,
) -> JsonRpcResponse {
    let level_params: SetLoggingLevelParams = match extract_params(id, params, "logging/setLevel") {
        Ok(p) => p,
        Err(resp) => return *resp,
    };

    match server.set_logging_level(level_params.level).await {
        Ok(()) => JsonRpcResponse::success(id.clone(), serde_json::json!({})),
        Err(err) => {
            let (code, message) = gateway_error_to_jsonrpc(&err);
            JsonRpcResponse::error(id.clone(), code, message, None)
        }
    }
}

// ── Completion handler ──────────────────────────────────────────────

async fn handle_complete<T: McpCompletionServer>(
    id: &JsonRpcId,
    params: Option<serde_json::Value>,
    server: &T,
) -> JsonRpcResponse {
    let complete_params: CompleteParams = match extract_params(id, params, "completion/complete") {
        Ok(p) => p,
        Err(resp) => return *resp,
    };

    match server.complete(complete_params).await {
        Ok(result) => serialize_success(id, &result),
        Err(err) => {
            let (code, message) = gateway_error_to_jsonrpc(&err);
            JsonRpcResponse::error(id.clone(), code, message, None)
        }
    }
}

// ── Error mapping ───────────────────────────────────────────────────

/// Map a gateway error to a JSON-RPC error code and message.
fn gateway_error_to_jsonrpc(err: &McpGatewayError) -> (i64, String) {
    match err {
        McpGatewayError::ToolNotFound { .. }
        | McpGatewayError::ResourceNotFound { .. }
        | McpGatewayError::PromptNotFound { .. } => {
            (error_codes::METHOD_NOT_FOUND, err.to_string())
        }
        McpGatewayError::InvalidConfig { .. }
        | McpGatewayError::ParamDenied { .. }
        | McpGatewayError::SubscriptionNotSupported { .. }
        | McpGatewayError::CompletionNotAvailable { .. } => {
            (error_codes::INVALID_PARAMS, err.to_string())
        }
        _ => (error_codes::INTERNAL_ERROR, err.to_string()),
    }
}

// ── Observation helpers ─────────────────────────────────────────────

/// Shared context threaded through MCP tool call handlers for observation.
#[derive(Clone)]
struct McpObserveContext {
    observer: Arc<dyn ToolObserveCallback>,
    caller: CallerContext,
}

// ── Payment-gated MCP filters ───────────────────────────────────────

/// Combined MCP server filter with payment gating.
///
/// Like [`mcp_server_filter_with_observe`], but additionally verifies payment
/// via the [`PaymentGate`](crate::mpp::PaymentGate) trait before dispatching
/// JSON-RPC requests. Management actions (channel open / top-up / close)
/// short-circuit and return the management response with a payment receipt.
///
/// `GET /mcp/sse` is served without payment verification (notification-only).
#[cfg(any(feature = "mpp-tempo", feature = "mpp-solana"))]
pub fn mcp_server_filter_with_payment_gate<T, A>(
    server: Option<Arc<T>>,
    tool_call_handler: Option<Arc<dyn ToolCallHandler>>,
    observer: Arc<dyn ToolObserveCallback>,
    payment_gate: Arc<dyn crate::mpp::PaymentGate>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: McpServer + 'static,
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    mcp_jsonrpc_filter_with_payment_gate(
        server.clone(),
        tool_call_handler,
        observer,
        payment_gate,
        account_filter,
    )
    .or(mcp_sse_filter(server))
}

#[cfg(any(feature = "mpp-tempo", feature = "mpp-solana"))]
fn mcp_jsonrpc_filter_with_payment_gate<T, A>(
    server: Option<Arc<T>>,
    tool_call_handler: Option<Arc<dyn ToolCallHandler>>,
    observer: Arc<dyn ToolObserveCallback>,
    payment_gate: Arc<dyn crate::mpp::PaymentGate>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: McpServer + 'static,
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    warp::path("mcp")
        .and(warp::path::end())
        .and(warp::post())
        .and(account_filter)
        .and(warp::any().map(move || payment_gate.clone()))
        .and(warp::header::optional::<String>("authorization"))
        .and(warp::body::json::<serde_json::Value>())
        .and(warp::any().map(move || server.clone()))
        .and(warp::any().map(move || tool_call_handler.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and_then(handle_mcp_jsonrpc_with_gate::<T>)
}

#[cfg(any(feature = "mpp-tempo", feature = "mpp-solana"))]
async fn handle_mcp_jsonrpc_with_gate<T: McpServer>(
    caller: CallerContext,
    payment_gate: Arc<dyn crate::mpp::PaymentGate>,
    auth_header: Option<String>,
    body: serde_json::Value,
    server: Option<Arc<T>>,
    tool_call_handler: Option<Arc<dyn ToolCallHandler>>,
    observer: Arc<dyn ToolObserveCallback>,
) -> Result<Box<dyn warp::Reply>, warp::Rejection> {
    let mpp_ctx = payment_gate
        .verify_payment(caller.chain.clone(), auth_header)
        .await?;

    // Management actions (channel open/topUp/close) short-circuit.
    if let Some(ref management) = mpp_ctx.management_response {
        let reply = warp::reply::json(management);
        if let Ok(receipt_header) = mpp::format_receipt(&mpp_ctx.receipt) {
            return Ok(Box::new(warp::reply::with_header(
                reply,
                mpp::PAYMENT_RECEIPT_HEADER,
                receipt_header,
            )));
        }
        return Ok(Box::new(reply));
    }

    let _close_guard = crate::mpp::SessionCloseGuard::new(
        payment_gate,
        mpp_ctx.backend_key.clone(),
        mpp_ctx.channel_id.clone(),
    );

    let ctx = Some(McpObserveContext { observer, caller });
    let reply = handle_jsonrpc_value::<T>(body, server, tool_call_handler, ctx).await;

    // Attach payment receipt header.
    if let Ok(receipt_header) = mpp::format_receipt(&mpp_ctx.receipt) {
        Ok(Box::new(warp::reply::with_header(
            reply,
            mpp::PAYMENT_RECEIPT_HEADER,
            receipt_header,
        )))
    } else {
        Ok(reply)
    }
}

// ── Observation helpers ─────────────────────────────────────────────

/// Fire a success [`ToolCallSuccessEvent`] for a completed MCP tool call.
///
/// The event is spawned as an async task so it never blocks the response path.
fn emit_tool_success(ctx: &Option<McpObserveContext>, server: &str, tool: &str, start: Instant) {
    let Some(ctx) = ctx else { return };
    let event = ToolCallSuccessEvent {
        ctx: ToolRequestContext {
            provider: server.to_string(),
            operation: tool.to_string(),
            caller: ctx.caller.clone(),
            latency_ms: start.elapsed().as_millis() as u64,
        },
    };
    let obs = ctx.observer.clone();
    tokio::spawn(async move { obs.on_tool_call_success(event).await });
}

/// Fire a failure [`ToolCallFailureEvent`] for a failed MCP tool call.
///
/// The event is spawned as an async task so it never blocks the response path.
fn emit_tool_failure(
    ctx: &Option<McpObserveContext>,
    server: &str,
    tool: &str,
    start: Instant,
    error: &str,
) {
    let Some(ctx) = ctx else { return };
    let event = ToolCallFailureEvent {
        ctx: ToolRequestContext {
            provider: server.to_string(),
            operation: tool.to_string(),
            caller: ctx.caller.clone(),
            latency_ms: start.elapsed().as_millis() as u64,
        },
        error: error.to_string(),
    };
    let obs = ctx.observer.clone();
    tokio::spawn(async move { obs.on_tool_call_failure(event).await });
}
