//! JSON-RPC handler for `logging/setLevel`.

use super::tools::gateway_error_to_jsonrpc;
use bitrouter_core::api::mcp::gateway::McpLoggingServer;
use bitrouter_core::api::mcp::types::{
    JsonRpcId, JsonRpcResponse, SetLoggingLevelParams, error_codes,
};

pub async fn handle_set_level<T: McpLoggingServer>(
    id: &JsonRpcId,
    params: Option<serde_json::Value>,
    server: &T,
) -> JsonRpcResponse {
    let Some(params_value) = params else {
        return JsonRpcResponse::error(
            id.clone(),
            error_codes::INVALID_PARAMS,
            "logging/setLevel requires params".to_string(),
            None,
        );
    };

    let level_params: SetLoggingLevelParams = match serde_json::from_value(params_value) {
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

    match server.set_logging_level(level_params.level).await {
        Ok(()) => JsonRpcResponse::success(id.clone(), serde_json::json!({})),
        Err(err) => {
            let (code, message) = gateway_error_to_jsonrpc(&err);
            JsonRpcResponse::error(id.clone(), code, message, None)
        }
    }
}
