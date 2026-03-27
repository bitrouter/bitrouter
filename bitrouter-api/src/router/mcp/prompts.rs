//! JSON-RPC handlers for the `prompts/*` MCP methods.

use super::tools::gateway_error_to_jsonrpc;
use super::types::{
    GetPromptParams, JsonRpcId, JsonRpcResponse, ListPromptsResult, McpPromptServer, error_codes,
};
use bitrouter_core::api::mcp::types::ListPromptsParams;

pub async fn handle_prompts_list<T: McpPromptServer>(
    id: &JsonRpcId,
    params: Option<serde_json::Value>,
    server: &T,
) -> JsonRpcResponse {
    let cursor = params
        .and_then(|v| serde_json::from_value::<ListPromptsParams>(v).ok())
        .and_then(|p| p.cursor);
    let (prompts, next_cursor) = server.list_prompts(cursor.as_deref()).await;
    let result = ListPromptsResult {
        prompts,
        next_cursor,
    };
    let value = serde_json::to_value(&result).unwrap_or_default();
    JsonRpcResponse::success(id.clone(), value)
}

pub async fn handle_prompts_get<T: McpPromptServer>(
    id: &JsonRpcId,
    params: Option<serde_json::Value>,
    server: &T,
) -> JsonRpcResponse {
    let Some(params_value) = params else {
        return JsonRpcResponse::error(
            id.clone(),
            error_codes::INVALID_PARAMS,
            "prompts/get requires params".to_string(),
            None,
        );
    };

    let get_params: GetPromptParams = match serde_json::from_value(params_value) {
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
        .get_prompt(&get_params.name, get_params.arguments)
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
