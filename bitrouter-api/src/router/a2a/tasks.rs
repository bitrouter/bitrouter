//! Task operation handlers for A2A gateway.

use bitrouter_a2a::client::upstream::UpstreamA2aAgent;

use super::convert::{WithId, deserialize_params, gateway_error_response, success_response};
use super::types::*;

/// Handle `tasks/get` JSON-RPC method.
pub(crate) async fn dispatch_get_task(
    request: &JsonRpcRequest,
    agent: &UpstreamA2aAgent,
) -> JsonRpcResponse {
    let req: GetTaskRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    match agent.get_task(req).await {
        Ok(task) => success_response(&request.id, &task),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

/// Handle `tasks/cancel` JSON-RPC method.
pub(crate) async fn dispatch_cancel_task(
    request: &JsonRpcRequest,
    agent: &UpstreamA2aAgent,
) -> JsonRpcResponse {
    let req: CancelTaskRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    match agent.cancel_task(req).await {
        Ok(task) => success_response(&request.id, &task),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

/// Handle `tasks/list` JSON-RPC method.
pub(crate) async fn dispatch_list_tasks(
    request: &JsonRpcRequest,
    agent: &UpstreamA2aAgent,
) -> JsonRpcResponse {
    let req: ListTasksRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    match agent.list_tasks(req).await {
        Ok(resp) => success_response(&request.id, &resp),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}
