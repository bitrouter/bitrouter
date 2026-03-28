//! Task operation handlers for A2A gateway.

use bitrouter_core::api::a2a::gateway::A2aProxy;
use tokio::time::Instant;

use super::observe::{A2aObserveContext, emit_agent_failure, emit_agent_success};
use bitrouter_core::api::a2a::types::*;

/// Handle `tasks/get` JSON-RPC method.
pub(crate) async fn dispatch_get_task(
    request: &JsonRpcRequest,
    agent: &impl A2aProxy,
    agent_name: &str,
    ctx: &Option<A2aObserveContext>,
) -> JsonRpcResponse {
    let req: GetTaskRequest = match request.deserialize_params() {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let start = Instant::now();
    let result = agent.get_task(req).await;
    match &result {
        Ok(_) => emit_agent_success(ctx, agent_name, "tasks/get", start),
        Err(e) => emit_agent_failure(ctx, agent_name, "tasks/get", start, &e.to_string()),
    }
    match result {
        Ok(task) => JsonRpcResponse::success(&request.id, &task),
        Err(e) => JsonRpcResponse::gateway_error(&request.id, &e),
    }
}

/// Handle `tasks/cancel` JSON-RPC method.
pub(crate) async fn dispatch_cancel_task(
    request: &JsonRpcRequest,
    agent: &impl A2aProxy,
    agent_name: &str,
    ctx: &Option<A2aObserveContext>,
) -> JsonRpcResponse {
    let req: CancelTaskRequest = match request.deserialize_params() {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let start = Instant::now();
    let result = agent.cancel_task(req).await;
    match &result {
        Ok(_) => emit_agent_success(ctx, agent_name, "tasks/cancel", start),
        Err(e) => emit_agent_failure(ctx, agent_name, "tasks/cancel", start, &e.to_string()),
    }
    match result {
        Ok(task) => JsonRpcResponse::success(&request.id, &task),
        Err(e) => JsonRpcResponse::gateway_error(&request.id, &e),
    }
}

/// Handle `tasks/list` JSON-RPC method.
pub(crate) async fn dispatch_list_tasks(
    request: &JsonRpcRequest,
    agent: &impl A2aProxy,
    agent_name: &str,
    ctx: &Option<A2aObserveContext>,
) -> JsonRpcResponse {
    let req: ListTasksRequest = match request.deserialize_params() {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let start = Instant::now();
    let result = agent.list_tasks(req).await;
    match &result {
        Ok(_) => emit_agent_success(ctx, agent_name, "tasks/list", start),
        Err(e) => emit_agent_failure(ctx, agent_name, "tasks/list", start, &e.to_string()),
    }
    match result {
        Ok(resp) => JsonRpcResponse::success(&request.id, &resp),
        Err(e) => JsonRpcResponse::gateway_error(&request.id, &e),
    }
}
