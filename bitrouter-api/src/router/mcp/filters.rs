//! Warp filters for the MCP protocol.
//!
//! - `POST /mcp` — JSON-RPC dispatch for all MCP methods.
//! - `GET /mcp/sse` — SSE stream for change notifications.

use std::convert::Infallible;
use std::sync::Arc;

use bitrouter_core::observe::{CallerContext, ToolObserveCallback};
use tokio_stream::StreamExt;
use warp::Filter;

use super::types::{
    CompletionsCapability, InitializeResult, JsonRpcId, JsonRpcMessage, JsonRpcResponse,
    LoggingCapability, McpServer, PromptsCapability, ResourcesCapability, ServerCapabilities,
    ServerInfo, ToolsCapability, error_codes,
};
use super::{completion, logging, prompts, resources, subscriptions, tools};

/// The MCP protocol version this server advertises.
const PROTOCOL_VERSION: &str = "2025-11-25";

/// Server name returned during initialization.
const SERVER_NAME: &str = "bitrouter";

use super::observe::McpObserveContext;

/// Combined MCP server filter: `POST /mcp` + `GET /mcp/sse`.
///
/// When `server` is `None` (no MCP configured), both endpoints return 404.
pub fn mcp_server_filter<T>(
    server: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: McpServer + 'static,
{
    mcp_jsonrpc_filter(server.clone(), None).or(mcp_sse_filter(server))
}

/// Combined MCP server filter with tool call observation.
///
/// The `account_filter` extracts a [`CallerContext`] per-request (e.g. from
/// JWT claims) so that observation events carry account information.
pub fn mcp_server_filter_with_observe<T, A>(
    server: Option<Arc<T>>,
    observer: Arc<dyn ToolObserveCallback>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: McpServer + 'static,
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    mcp_jsonrpc_filter_with_observe(server.clone(), observer, account_filter)
        .or(mcp_sse_filter(server))
}

// ── POST /mcp ────────────────────────────────────────────────────────

fn mcp_jsonrpc_filter<T>(
    server: Option<Arc<T>>,
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
        .and(warp::any().map(move || observe_ctx.clone()))
        .then(handle_jsonrpc_value::<T>)
}

fn mcp_jsonrpc_filter_with_observe<T, A>(
    server: Option<Arc<T>>,
    observer: Arc<dyn ToolObserveCallback>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: McpServer + 'static,
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    warp::path("mcp")
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::body::json::<serde_json::Value>())
        .and(warp::any().map(move || server.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and(account_filter)
        .then(
            |body: serde_json::Value,
             server: Option<Arc<T>>,
             observer: Arc<dyn ToolObserveCallback>,
             caller: CallerContext| async move {
                let ctx = Some(McpObserveContext { observer, caller });
                handle_jsonrpc_value::<T>(body, server, ctx).await
            },
        )
}

async fn handle_jsonrpc_value<T: McpServer>(
    body: serde_json::Value,
    server: Option<Arc<T>>,
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
            let resp =
                dispatch_request(&req.id, &req.method, req.params, &*server, &observe_ctx).await;
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

async fn dispatch_request<T: McpServer>(
    id: &JsonRpcId,
    method: &str,
    params: Option<serde_json::Value>,
    server: &T,
    observe_ctx: &Option<McpObserveContext>,
) -> JsonRpcResponse {
    match method {
        "initialize" => handle_initialize(id),
        "ping" => handle_ping(id),
        "tools/list" => tools::handle_tools_list(id, server).await,
        "tools/call" => tools::handle_tools_call(id, params, server, observe_ctx).await,
        "resources/list" => resources::handle_resources_list(id, server).await,
        "resources/read" => resources::handle_resources_read(id, params, server).await,
        "resources/templates/list" => resources::handle_resource_templates_list(id, server).await,
        "resources/subscribe" => subscriptions::handle_resource_subscribe(id, params, server).await,
        "resources/unsubscribe" => {
            subscriptions::handle_resource_unsubscribe(id, params, server).await
        }
        "prompts/list" => prompts::handle_prompts_list(id, server).await,
        "prompts/get" => prompts::handle_prompts_get(id, params, server).await,
        "logging/setLevel" => logging::handle_set_level(id, params, server).await,
        "completion/complete" => completion::handle_complete(id, params, server).await,
        _ => JsonRpcResponse::error(
            id.clone(),
            error_codes::METHOD_NOT_FOUND,
            format!("method not found: {method}"),
            None,
        ),
    }
}

fn handle_initialize(id: &JsonRpcId) -> JsonRpcResponse {
    let result = InitializeResult {
        protocol_version: PROTOCOL_VERSION.to_string(),
        capabilities: ServerCapabilities {
            tools: Some(ToolsCapability {
                list_changed: Some(true),
            }),
            resources: Some(ResourcesCapability {
                list_changed: Some(true),
                subscribe: Some(true),
            }),
            prompts: Some(PromptsCapability {
                list_changed: Some(true),
            }),
            logging: Some(LoggingCapability {}),
            completions: Some(CompletionsCapability {}),
        },
        server_info: ServerInfo {
            name: SERVER_NAME.to_string(),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        },
        instructions: Some(
            "BitRouter MCP Gateway — aggregated tools from multiple upstream MCP servers"
                .to_string(),
        ),
    };
    let value = serde_json::to_value(&result).unwrap_or_default();
    JsonRpcResponse::success(id.clone(), value)
}

fn handle_ping(id: &JsonRpcId) -> JsonRpcResponse {
    JsonRpcResponse::success(id.clone(), serde_json::json!({}))
}

// ── GET /mcp/sse ─────────────────────────────────────────────────────

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

async fn handle_sse<T: McpServer>(
    server: Option<Arc<T>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let Some(server) = server else {
        return Err(warp::reject::not_found());
    };

    let tool_rx = server.subscribe_tool_changes();
    let tool_stream =
        tokio_stream::wrappers::BroadcastStream::new(tool_rx).filter_map(|item| match item {
            Ok(()) => {
                let notification = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/tools/list_changed"
                });
                let data = serde_json::to_string(&notification).unwrap_or_default();
                Some(Ok::<_, Infallible>(warp::sse::Event::default().data(data)))
            }
            Err(_) => None,
        });

    let resource_rx = server.subscribe_resource_changes();
    let resource_stream =
        tokio_stream::wrappers::BroadcastStream::new(resource_rx).filter_map(|item| match item {
            Ok(()) => {
                let notification = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/resources/list_changed"
                });
                let data = serde_json::to_string(&notification).unwrap_or_default();
                Some(Ok::<_, Infallible>(warp::sse::Event::default().data(data)))
            }
            Err(_) => None,
        });

    let prompt_rx = server.subscribe_prompt_changes();
    let prompt_stream =
        tokio_stream::wrappers::BroadcastStream::new(prompt_rx).filter_map(|item| match item {
            Ok(()) => {
                let notification = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/prompts/list_changed"
                });
                let data = serde_json::to_string(&notification).unwrap_or_default();
                Some(Ok::<_, Infallible>(warp::sse::Event::default().data(data)))
            }
            Err(_) => None,
        });

    let merged = tool_stream.merge(resource_stream).merge(prompt_stream);

    Ok(warp::sse::reply(warp::sse::keep_alive().stream(merged)))
}
