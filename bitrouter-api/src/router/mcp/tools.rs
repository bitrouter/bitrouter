//! JSON-RPC handlers for the `tools/*` MCP methods.

use super::types::{
    CallToolParams, JsonRpcId, JsonRpcResponse, ListToolsResult, McpGatewayError, McpToolServer,
    error_codes,
};

/// Separator used in wire-format tool names exposed to downstream MCP clients.
///
/// Internally bitrouter namespaces tools as `"server/tool"`, but `/` is
/// invalid in many LLM function-name constraints (e.g. Gemini). On the wire
/// we emit `"server__tool"` and translate incoming calls back.
const WIRE_SEPARATOR: &str = "__";

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

pub async fn handle_tools_list<T: McpToolServer>(id: &JsonRpcId, server: &T) -> JsonRpcResponse {
    let mut tools = server.list_tools().await;
    for tool in &mut tools {
        tool.name = to_wire_name(&tool.name);
    }
    let result = ListToolsResult {
        tools,
        next_cursor: None,
    };
    let value = serde_json::to_value(&result).unwrap_or_default();
    JsonRpcResponse::success(id.clone(), value)
}

pub async fn handle_tools_call<T: McpToolServer>(
    id: &JsonRpcId,
    params: Option<serde_json::Value>,
    server: &T,
    observe_ctx: &Option<super::observe::McpObserveContext>,
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

    let internal_name = from_wire_name(&call_params.name);
    let (server_name, tool_name) = internal_name
        .split_once('/')
        .unwrap_or(("unknown", &internal_name));
    let start = tokio::time::Instant::now();

    match server
        .call_tool(&internal_name, call_params.arguments)
        .await
    {
        Ok(result) => {
            super::observe::emit_tool_success(observe_ctx, server_name, tool_name, start);
            let value = serde_json::to_value(&result).unwrap_or_default();
            JsonRpcResponse::success(id.clone(), value)
        }
        Err(err) => {
            let err_str = err.to_string();
            super::observe::emit_tool_failure(observe_ctx, server_name, tool_name, start, &err_str);
            let (code, message) = gateway_error_to_jsonrpc(&err);
            JsonRpcResponse::error(id.clone(), code, message, None)
        }
    }
}

/// Map a gateway error to a JSON-RPC error code and message.
pub fn gateway_error_to_jsonrpc(err: &McpGatewayError) -> (i64, String) {
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
