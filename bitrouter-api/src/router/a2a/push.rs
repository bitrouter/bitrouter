//! Push notification config handlers for A2A gateway.

use bitrouter_a2a::client::upstream::UpstreamA2aAgent;
use tokio::time::Instant;

use super::convert::{WithId, deserialize_params, gateway_error_response, success_response};
use super::observe::{A2aObserveContext, emit_agent_event};
use super::types::*;

/// Handle `tasks/pushNotificationConfig/set` JSON-RPC method.
pub(crate) async fn dispatch_set_push(
    request: &JsonRpcRequest,
    agent: &UpstreamA2aAgent,
    agent_name: &str,
    ctx: &Option<A2aObserveContext>,
) -> JsonRpcResponse {
    let config: TaskPushNotificationConfig = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    let start = Instant::now();
    let result = agent.set_push_config(config).await;
    emit_agent_event(
        ctx,
        agent_name,
        "tasks/pushNotificationConfig/set",
        start,
        &result,
    );
    match result {
        Ok(stored) => success_response(&request.id, &stored),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

/// Handle `tasks/pushNotificationConfig/get` JSON-RPC method.
pub(crate) async fn dispatch_get_push(
    request: &JsonRpcRequest,
    agent: &UpstreamA2aAgent,
    agent_name: &str,
    ctx: &Option<A2aObserveContext>,
) -> JsonRpcResponse {
    let req: GetTaskPushNotificationConfigRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    let start = Instant::now();
    let result = agent
        .get_push_config(&req.id, req.push_notification_config_id.as_deref())
        .await;
    emit_agent_event(
        ctx,
        agent_name,
        "tasks/pushNotificationConfig/get",
        start,
        &result,
    );
    match result {
        Ok(config) => success_response(&request.id, &config),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

/// Handle `tasks/pushNotificationConfig/list` JSON-RPC method.
pub(crate) async fn dispatch_list_push(
    request: &JsonRpcRequest,
    agent: &UpstreamA2aAgent,
    agent_name: &str,
    ctx: &Option<A2aObserveContext>,
) -> JsonRpcResponse {
    let req: ListTaskPushNotificationConfigsRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    let start = Instant::now();
    let result = agent.list_push_configs(&req.id).await;
    emit_agent_event(
        ctx,
        agent_name,
        "tasks/pushNotificationConfig/list",
        start,
        &result,
    );
    match result {
        Ok(resp) => success_response(&request.id, &resp),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

/// Handle `tasks/pushNotificationConfig/delete` JSON-RPC method.
pub(crate) async fn dispatch_delete_push(
    request: &JsonRpcRequest,
    agent: &UpstreamA2aAgent,
    agent_name: &str,
    ctx: &Option<A2aObserveContext>,
) -> JsonRpcResponse {
    let req: DeleteTaskPushNotificationConfigRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    let start = Instant::now();
    let result = agent
        .delete_push_config(&req.id, &req.push_notification_config_id)
        .await;
    emit_agent_event(
        ctx,
        agent_name,
        "tasks/pushNotificationConfig/delete",
        start,
        &result,
    );
    match result {
        Ok(()) => success_response(&request.id, &serde_json::json!({"success": true})),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}
