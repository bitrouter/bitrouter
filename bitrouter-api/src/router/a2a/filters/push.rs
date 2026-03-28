//! Push notification config handlers for A2A gateway.

use bitrouter_core::api::a2a::gateway::A2aProxy;
use tokio::time::Instant;

use super::observe::{A2aObserveContext, emit_agent_failure, emit_agent_success};
use bitrouter_core::api::a2a::types::*;

/// Handle `tasks/pushNotificationConfig/set` JSON-RPC method.
pub(crate) async fn dispatch_set_push(
    request: &JsonRpcRequest,
    agent: &impl A2aProxy,
    agent_name: &str,
    ctx: &Option<A2aObserveContext>,
) -> JsonRpcResponse {
    let config: TaskPushNotificationConfig = match request.deserialize_params() {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let start = Instant::now();
    let result = agent.set_push_config(config).await;
    match &result {
        Ok(_) => emit_agent_success(ctx, agent_name, "tasks/pushNotificationConfig/set", start),
        Err(e) => emit_agent_failure(
            ctx,
            agent_name,
            "tasks/pushNotificationConfig/set",
            start,
            &e.to_string(),
        ),
    }
    match result {
        Ok(stored) => JsonRpcResponse::success(&request.id, &stored),
        Err(e) => JsonRpcResponse::gateway_error(&request.id, &e),
    }
}

/// Handle `tasks/pushNotificationConfig/get` JSON-RPC method.
pub(crate) async fn dispatch_get_push(
    request: &JsonRpcRequest,
    agent: &impl A2aProxy,
    agent_name: &str,
    ctx: &Option<A2aObserveContext>,
) -> JsonRpcResponse {
    let req: GetTaskPushNotificationConfigRequest = match request.deserialize_params() {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let start = Instant::now();
    let result = agent
        .get_push_config(&req.id, req.push_notification_config_id.as_deref())
        .await;
    match &result {
        Ok(_) => emit_agent_success(ctx, agent_name, "tasks/pushNotificationConfig/get", start),
        Err(e) => emit_agent_failure(
            ctx,
            agent_name,
            "tasks/pushNotificationConfig/get",
            start,
            &e.to_string(),
        ),
    }
    match result {
        Ok(config) => JsonRpcResponse::success(&request.id, &config),
        Err(e) => JsonRpcResponse::gateway_error(&request.id, &e),
    }
}

/// Handle `tasks/pushNotificationConfig/list` JSON-RPC method.
pub(crate) async fn dispatch_list_push(
    request: &JsonRpcRequest,
    agent: &impl A2aProxy,
    agent_name: &str,
    ctx: &Option<A2aObserveContext>,
) -> JsonRpcResponse {
    let req: ListTaskPushNotificationConfigsRequest = match request.deserialize_params() {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let start = Instant::now();
    let result = agent.list_push_configs(&req.id).await;
    match &result {
        Ok(_) => emit_agent_success(ctx, agent_name, "tasks/pushNotificationConfig/list", start),
        Err(e) => emit_agent_failure(
            ctx,
            agent_name,
            "tasks/pushNotificationConfig/list",
            start,
            &e.to_string(),
        ),
    }
    match result {
        Ok(resp) => JsonRpcResponse::success(&request.id, &resp),
        Err(e) => JsonRpcResponse::gateway_error(&request.id, &e),
    }
}

/// Handle `tasks/pushNotificationConfig/delete` JSON-RPC method.
pub(crate) async fn dispatch_delete_push(
    request: &JsonRpcRequest,
    agent: &impl A2aProxy,
    agent_name: &str,
    ctx: &Option<A2aObserveContext>,
) -> JsonRpcResponse {
    let req: DeleteTaskPushNotificationConfigRequest = match request.deserialize_params() {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let start = Instant::now();
    let result = agent
        .delete_push_config(&req.id, &req.push_notification_config_id)
        .await;
    match &result {
        Ok(_) => emit_agent_success(
            ctx,
            agent_name,
            "tasks/pushNotificationConfig/delete",
            start,
        ),
        Err(e) => emit_agent_failure(
            ctx,
            agent_name,
            "tasks/pushNotificationConfig/delete",
            start,
            &e.to_string(),
        ),
    }
    match result {
        Ok(()) => JsonRpcResponse::success(&request.id, &serde_json::json!({"success": true})),
        Err(e) => JsonRpcResponse::gateway_error(&request.id, &e),
    }
}
