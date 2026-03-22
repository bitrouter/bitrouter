//! Task operation handlers for A2A gateway.

use super::convert::{WithId, deserialize_params, gateway_error_response, success_response};
use super::types::*;

/// Handle `tasks/get` JSON-RPC method.
pub(crate) async fn dispatch_get_task<T: A2aProxy>(
    request: &JsonRpcRequest,
    gw: &T,
) -> JsonRpcResponse {
    let req: GetTaskRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    match gw.get_task(req).await {
        Ok(task) => success_response(&request.id, &task),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

/// Handle `tasks/cancel` JSON-RPC method.
pub(crate) async fn dispatch_cancel_task<T: A2aProxy>(
    request: &JsonRpcRequest,
    gw: &T,
) -> JsonRpcResponse {
    let req: CancelTaskRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    match gw.cancel_task(req).await {
        Ok(task) => success_response(&request.id, &task),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

/// Handle `tasks/list` JSON-RPC method.
pub(crate) async fn dispatch_list_tasks<T: A2aProxy>(
    request: &JsonRpcRequest,
    gw: &T,
) -> JsonRpcResponse {
    let req: ListTasksRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    match gw.list_tasks(req).await {
        Ok(resp) => success_response(&request.id, &resp),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}
