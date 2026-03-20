//! JSON-RPC handlers for the `tools/*` MCP methods.

use super::types::{
    CallToolParams, JsonRpcId, JsonRpcResponse, ListToolsResult, McpGatewayError, McpToolServer,
    error_codes,
};

pub async fn handle_tools_list<T: McpToolServer>(id: &JsonRpcId, server: &T) -> JsonRpcResponse {
    let tools = server.list_tools().await;
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
pub fn gateway_error_to_jsonrpc(err: &McpGatewayError) -> (i64, String) {
    match err {
        McpGatewayError::ToolNotFound { .. }
        | McpGatewayError::ResourceNotFound { .. }
        | McpGatewayError::PromptNotFound { .. } => {
            (error_codes::METHOD_NOT_FOUND, err.to_string())
        }
        McpGatewayError::InvalidConfig { .. } | McpGatewayError::ParamDenied { .. } => {
            (error_codes::INVALID_PARAMS, err.to_string())
        }
        _ => (error_codes::INTERNAL_ERROR, err.to_string()),
    }
}
