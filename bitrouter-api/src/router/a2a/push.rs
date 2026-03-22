//! Push notification config handlers for A2A gateway.

use super::convert::{WithId, deserialize_params, gateway_error_response, success_response};
use super::types::*;

/// Handle `tasks/pushNotificationConfig/set` JSON-RPC method.
pub(crate) async fn dispatch_set_push<T: A2aProxy>(
    request: &JsonRpcRequest,
    gw: &T,
) -> JsonRpcResponse {
    let config: TaskPushNotificationConfig = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    match gw.set_push_config(config).await {
        Ok(stored) => success_response(&request.id, &stored),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

/// Handle `tasks/pushNotificationConfig/get` JSON-RPC method.
pub(crate) async fn dispatch_get_push<T: A2aProxy>(
    request: &JsonRpcRequest,
    gw: &T,
) -> JsonRpcResponse {
    let req: GetTaskPushNotificationConfigRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    match gw
        .get_push_config(&req.id, req.push_notification_config_id.as_deref())
        .await
    {
        Ok(config) => success_response(&request.id, &config),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

/// Handle `tasks/pushNotificationConfig/list` JSON-RPC method.
pub(crate) async fn dispatch_list_push<T: A2aProxy>(
    request: &JsonRpcRequest,
    gw: &T,
) -> JsonRpcResponse {
    let req: ListTaskPushNotificationConfigsRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    match gw.list_push_configs(&req.id).await {
        Ok(resp) => success_response(&request.id, &resp),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

/// Handle `tasks/pushNotificationConfig/delete` JSON-RPC method.
pub(crate) async fn dispatch_delete_push<T: A2aProxy>(
    request: &JsonRpcRequest,
    gw: &T,
) -> JsonRpcResponse {
    let req: DeleteTaskPushNotificationConfigRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    match gw
        .delete_push_config(&req.id, &req.push_notification_config_id)
        .await
    {
        Ok(()) => success_response(&request.id, &serde_json::json!({"success": true})),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}
