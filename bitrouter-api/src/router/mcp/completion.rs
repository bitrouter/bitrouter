//! JSON-RPC handler for `completion/complete`.

use super::tools::gateway_error_to_jsonrpc;
use super::types::{JsonRpcId, JsonRpcResponse, McpCompletionServer, error_codes};
use bitrouter_core::api::mcp::types::CompleteParams;

pub async fn handle_complete<T: McpCompletionServer>(
    id: &JsonRpcId,
    params: Option<serde_json::Value>,
    server: &T,
) -> JsonRpcResponse {
    let Some(params_value) = params else {
        return JsonRpcResponse::error(
            id.clone(),
            error_codes::INVALID_PARAMS,
            "completion/complete requires params".to_string(),
            None,
        );
    };

    let complete_params: CompleteParams = match serde_json::from_value(params_value) {
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

    match server.complete(complete_params).await {
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
