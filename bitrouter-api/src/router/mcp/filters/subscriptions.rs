//! JSON-RPC handlers for `resources/subscribe` and `resources/unsubscribe`.

use super::super::types::{JsonRpcId, JsonRpcResponse, McpSubscriptionServer, error_codes};
use super::tools::gateway_error_to_jsonrpc;
use bitrouter_core::api::mcp::types::{SubscribeResourceParams, UnsubscribeResourceParams};

pub async fn handle_resource_subscribe<T: McpSubscriptionServer>(
    id: &JsonRpcId,
    params: Option<serde_json::Value>,
    server: &T,
) -> JsonRpcResponse {
    let Some(params_value) = params else {
        return JsonRpcResponse::error(
            id.clone(),
            error_codes::INVALID_PARAMS,
            "resources/subscribe requires params".to_string(),
            None,
        );
    };

    let sub_params: SubscribeResourceParams = match serde_json::from_value(params_value) {
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

    match server.subscribe_resource(&sub_params.uri).await {
        Ok(()) => JsonRpcResponse::success(id.clone(), serde_json::json!({})),
        Err(err) => {
            let (code, message) = gateway_error_to_jsonrpc(&err);
            JsonRpcResponse::error(id.clone(), code, message, None)
        }
    }
}

pub async fn handle_resource_unsubscribe<T: McpSubscriptionServer>(
    id: &JsonRpcId,
    params: Option<serde_json::Value>,
    server: &T,
) -> JsonRpcResponse {
    let Some(params_value) = params else {
        return JsonRpcResponse::error(
            id.clone(),
            error_codes::INVALID_PARAMS,
            "resources/unsubscribe requires params".to_string(),
            None,
        );
    };

    let unsub_params: UnsubscribeResourceParams = match serde_json::from_value(params_value) {
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

    match server.unsubscribe_resource(&unsub_params.uri).await {
        Ok(()) => JsonRpcResponse::success(id.clone(), serde_json::json!({})),
        Err(err) => {
            let (code, message) = gateway_error_to_jsonrpc(&err);
            JsonRpcResponse::error(id.clone(), code, message, None)
        }
    }
}
