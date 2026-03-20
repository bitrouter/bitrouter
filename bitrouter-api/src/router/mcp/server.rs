//! Warp filters for the MCP server protocol.
//!
//! Exposes bitrouter's aggregated tools to downstream MCP clients via:
//!
//! - `POST /mcp` — JSON-RPC endpoint for `initialize`, `tools/list`,
//!   `tools/call`, and `notifications/initialized`.
//! - `GET /mcp/sse` — Long-lived SSE stream for server-push
//!   notifications (`notifications/tools/list_changed`).

use std::convert::Infallible;
use std::sync::Arc;

use tokio_stream::StreamExt;
use warp::Filter;

use bitrouter_mcp::error::McpGatewayError;
use bitrouter_mcp::server::McpToolServer;
use bitrouter_mcp::server::error_codes;
use bitrouter_mcp::server::jsonrpc::{JsonRpcId, JsonRpcMessage, JsonRpcResponse};
use bitrouter_mcp::server::protocol::{
    CallToolParams, InitializeResult, ListToolsResult, ServerCapabilities, ServerInfo,
    ToolsCapability,
};

/// The MCP protocol version this server advertises.
const PROTOCOL_VERSION: &str = "2025-03-26";

/// Server name returned during initialization.
const SERVER_NAME: &str = "bitrouter";

/// Combined MCP server filter: `POST /mcp` + `GET /mcp/sse`.
///
/// When `server` is `None` (no MCP configured), both endpoints return 404.
pub fn mcp_server_filter<T>(
    server: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: McpToolServer + 'static,
{
    mcp_jsonrpc_filter(server.clone()).or(mcp_sse_filter(server))
}

// ── POST /mcp ────────────────────────────────────────────────────────

fn mcp_jsonrpc_filter<T>(
    server: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: McpToolServer + 'static,
{
    warp::path("mcp")
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::body::json::<serde_json::Value>())
        .and(warp::any().map(move || server.clone()))
        .then(handle_jsonrpc_value::<T>)
}

async fn handle_jsonrpc_value<T: McpToolServer>(
    body: serde_json::Value,
    server: Option<Arc<T>>,
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
            let resp = dispatch_request(&req.id, &req.method, req.params, &*server).await;
            Box::new(warp::reply::json(&resp))
        }
        JsonRpcMessage::Notification(_notif) => {
            // Notifications get no response body — return 202 Accepted.
            Box::new(warp::reply::with_status(
                warp::reply::json(&serde_json::json!({})),
                warp::http::StatusCode::ACCEPTED,
            ))
        }
    }
}

async fn dispatch_request<T: McpToolServer>(
    id: &JsonRpcId,
    method: &str,
    params: Option<serde_json::Value>,
    server: &T,
) -> JsonRpcResponse {
    match method {
        "initialize" => handle_initialize(id),
        "tools/list" => handle_tools_list(id, server).await,
        "tools/call" => handle_tools_call(id, params, server).await,
        "ping" => handle_ping(id),
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

async fn handle_tools_list<T: McpToolServer>(id: &JsonRpcId, server: &T) -> JsonRpcResponse {
    let tools = server.list_tools().await;
    let result = ListToolsResult {
        tools,
        next_cursor: None,
    };
    let value = serde_json::to_value(&result).unwrap_or_default();
    JsonRpcResponse::success(id.clone(), value)
}

async fn handle_tools_call<T: McpToolServer>(
    id: &JsonRpcId,
    params: Option<serde_json::Value>,
    server: &T,
) -> JsonRpcResponse {
    let Some(params_value) = params else {
        return JsonRpcResponse::error(
            id.clone(),
            error_codes::INVALID_PARAMS,
            "tools/call requires params".to_string(),
            None,
        );
    };

    let call_params: CallToolParams = match serde_json::from_value(params_value) {
        Ok(p) => p,
        Err(e) => {
            return JsonRpcResponse::error(
                id.clone(),
                error_codes::INVALID_PARAMS,
                format!("invalid params: {e}"),
                None,
            );
        }
    };

    match server
        .call_tool(&call_params.name, call_params.arguments)
        .await
    {
        Ok(result) => {
            let value = serde_json::to_value(&result).unwrap_or_default();
            JsonRpcResponse::success(id.clone(), value)
        }
        Err(err) => {
            let (code, message) = gateway_error_to_jsonrpc(&err);
            JsonRpcResponse::error(id.clone(), code, message, None)
        }
    }
}

/// Map a gateway error to a JSON-RPC error code and message.
fn gateway_error_to_jsonrpc(err: &McpGatewayError) -> (i64, String) {
    match err {
        McpGatewayError::ToolNotFound { .. } => (error_codes::METHOD_NOT_FOUND, err.to_string()),
        McpGatewayError::InvalidConfig { .. } | McpGatewayError::ParamDenied { .. } => {
            (error_codes::INVALID_PARAMS, err.to_string())
        }
        _ => (error_codes::INTERNAL_ERROR, err.to_string()),
    }
}

// ── GET /mcp/sse ─────────────────────────────────────────────────────

fn mcp_sse_filter<T>(
    server: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: McpToolServer + 'static,
{
    warp::path!("mcp" / "sse")
        .and(warp::get())
        .and(warp::any().map(move || server.clone()))
        .and_then(handle_sse::<T>)
}

async fn handle_sse<T: McpToolServer>(
    server: Option<Arc<T>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let Some(server) = server else {
        return Err(warp::reject::not_found());
    };

    let rx = server.subscribe_tool_changes();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|item| match item {
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

    Ok(warp::sse::reply(warp::sse::keep_alive().stream(stream)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast;

    struct MockToolServer {
        change_tx: broadcast::Sender<()>,
    }

    impl MockToolServer {
        fn new() -> Self {
            let (change_tx, _) = broadcast::channel(16);
            Self { change_tx }
        }
    }

    impl McpToolServer for MockToolServer {
        async fn list_tools(&self) -> Vec<bitrouter_mcp::server::types::McpTool> {
            vec![bitrouter_mcp::server::types::McpTool {
                name: "test/echo".to_string(),
                description: Some("Echo tool".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"message": {"type": "string"}}
                }),
            }]
        }

        async fn call_tool(
            &self,
            name: &str,
            _arguments: Option<serde_json::Map<String, serde_json::Value>>,
        ) -> Result<bitrouter_mcp::server::types::McpToolCallResult, McpGatewayError> {
            if name == "test/echo" {
                Ok(bitrouter_mcp::server::types::McpToolCallResult {
                    content: vec![bitrouter_mcp::server::types::McpContent::Text {
                        text: "echoed".to_string(),
                    }],
                    is_error: None,
                })
            } else {
                Err(McpGatewayError::ToolNotFound {
                    name: name.to_string(),
                })
            }
        }

        fn subscribe_tool_changes(&self) -> broadcast::Receiver<()> {
            self.change_tx.subscribe()
        }
    }

    fn make_filter() -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
    {
        let server = Arc::new(MockToolServer::new());
        mcp_server_filter(Some(server))
    }

    fn make_none_filter()
    -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        mcp_server_filter::<MockToolServer>(None)
    }

    #[tokio::test]
    async fn initialize_returns_capabilities() {
        let filter = make_filter();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "test"}
            }
        });
        let resp = warp::test::request()
            .method("POST")
            .path("/mcp")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse");
        assert_eq!(json["jsonrpc"], "2.0");
        assert!(
            json["result"]["capabilities"]["tools"]["listChanged"]
                .as_bool()
                .unwrap_or(false)
        );
        assert_eq!(json["result"]["serverInfo"]["name"], "bitrouter");
    }

    #[tokio::test]
    async fn tools_list_returns_tools() {
        let filter = make_filter();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        });
        let resp = warp::test::request()
            .method("POST")
            .path("/mcp")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse");
        let tools = json["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "test/echo");
    }

    #[tokio::test]
    async fn tools_call_succeeds() {
        let filter = make_filter();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "test/echo", "arguments": {"message": "hi"}}
        });
        let resp = warp::test::request()
            .method("POST")
            .path("/mcp")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse");
        assert!(
            json["result"]["content"][0]["text"]
                .as_str()
                .is_some_and(|s| s == "echoed")
        );
    }

    #[tokio::test]
    async fn tools_call_not_found() {
        let filter = make_filter();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {"name": "nonexistent/tool"}
        });
        let resp = warp::test::request()
            .method("POST")
            .path("/mcp")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse");
        assert_eq!(json["error"]["code"], error_codes::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let filter = make_filter();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "unknown/method"
        });
        let resp = warp::test::request()
            .method("POST")
            .path("/mcp")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse");
        assert_eq!(json["error"]["code"], error_codes::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn malformed_json_returns_400() {
        let filter = make_filter();
        let resp = warp::test::request()
            .method("POST")
            .path("/mcp")
            .header("content-type", "application/json")
            .body("not json")
            .reply(&filter)
            .await;
        // Warp rejects malformed JSON at the body filter level.
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn valid_json_but_bad_jsonrpc_returns_parse_error() {
        let filter = make_filter();
        // Valid JSON but missing required jsonrpc fields.
        let body = serde_json::json!({"foo": "bar"});
        let resp = warp::test::request()
            .method("POST")
            .path("/mcp")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse");
        assert_eq!(json["error"]["code"], error_codes::PARSE_ERROR);
    }

    #[tokio::test]
    async fn notification_returns_accepted() {
        let filter = make_filter();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        let resp = warp::test::request()
            .method("POST")
            .path("/mcp")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 202);
    }

    #[tokio::test]
    async fn none_server_returns_404() {
        let filter = make_none_filter();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize"
        });
        let resp = warp::test::request()
            .method("POST")
            .path("/mcp")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn ping_returns_empty_result() {
        let filter = make_filter();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "ping"
        });
        let resp = warp::test::request()
            .method("POST")
            .path("/mcp")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse");
        assert!(json["result"].is_object());
        assert!(json["error"].is_null());
    }

    #[tokio::test]
    async fn tools_call_missing_params() {
        let filter = make_filter();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call"
        });
        let resp = warp::test::request()
            .method("POST")
            .path("/mcp")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse");
        assert_eq!(json["error"]["code"], error_codes::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn string_id_preserved() {
        let filter = make_filter();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "abc-123",
            "method": "ping"
        });
        let resp = warp::test::request()
            .method("POST")
            .path("/mcp")
            .json(&body)
            .reply(&filter)
            .await;
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse");
        assert_eq!(json["id"], "abc-123");
    }
}
