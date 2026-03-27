//! Task operation handlers for A2A gateway.

use bitrouter_providers::a2a::client::upstream::UpstreamA2aAgent;
use tokio::time::Instant;

use super::convert::{WithId, deserialize_params, gateway_error_response, success_response};
use super::observe::{A2aObserveContext, emit_agent_failure, emit_agent_success};
use super::types::*;

/// Handle `tasks/get` JSON-RPC method.
pub(crate) async fn dispatch_get_task(
    request: &JsonRpcRequest,
    agent: &UpstreamA2aAgent,
    agent_name: &str,
    ctx: &Option<A2aObserveContext>,
) -> JsonRpcResponse {
    let req: GetTaskRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    let start = Instant::now();
    let result = agent.get_task(req).await;
    match &result {
        Ok(_) => emit_agent_success(ctx, agent_name, "tasks/get", start),
        Err(e) => emit_agent_failure(ctx, agent_name, "tasks/get", start, &e.to_string()),
    }
    match result {
        Ok(task) => success_response(&request.id, &task),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

/// Handle `tasks/cancel` JSON-RPC method.
pub(crate) async fn dispatch_cancel_task(
    request: &JsonRpcRequest,
    agent: &UpstreamA2aAgent,
    agent_name: &str,
    ctx: &Option<A2aObserveContext>,
) -> JsonRpcResponse {
    let req: CancelTaskRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    let start = Instant::now();
    let result = agent.cancel_task(req).await;
    match &result {
        Ok(_) => emit_agent_success(ctx, agent_name, "tasks/cancel", start),
        Err(e) => emit_agent_failure(ctx, agent_name, "tasks/cancel", start, &e.to_string()),
    }
    match result {
        Ok(task) => success_response(&request.id, &task),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

/// Handle `tasks/list` JSON-RPC method.
pub(crate) async fn dispatch_list_tasks(
    request: &JsonRpcRequest,
    agent: &UpstreamA2aAgent,
    agent_name: &str,
    ctx: &Option<A2aObserveContext>,
) -> JsonRpcResponse {
    let req: ListTasksRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    let start = Instant::now();
    let result = agent.list_tasks(req).await;
    match &result {
        Ok(_) => emit_agent_success(ctx, agent_name, "tasks/list", start),
        Err(e) => emit_agent_failure(ctx, agent_name, "tasks/list", start, &e.to_string()),
    }
    match result {
        Ok(resp) => success_response(&request.id, &resp),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}
