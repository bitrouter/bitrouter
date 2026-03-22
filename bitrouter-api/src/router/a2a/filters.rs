//! Warp filter assembly and JSON-RPC dispatch for A2A gateway proxy.
//!
//! All routes are scoped under `/a2a/{agent_name}/...` so that each
//! upstream agent is a fully independent A2A endpoint.

use std::sync::Arc;

use bitrouter_a2a::client::registry::UpstreamAgentRegistry;
use bitrouter_a2a::client::upstream::UpstreamA2aAgent;
use bitrouter_a2a::error::A2aGatewayError;
use bitrouter_a2a::server::A2aProxy;
use serde::Deserialize;
use tokio_stream::StreamExt;
use warp::Filter;

use super::convert::error_response;
use super::messaging::{stream_response_to_sse, sync_bridge};
use super::types::*;
use super::{discovery, messaging, push, tasks};

/// Combined A2A gateway filter: per-agent discovery + JSON-RPC + streaming + REST.
///
/// Routes are prefixed with `/a2a/{agent_name}` so each upstream agent is
/// addressed independently.
pub fn a2a_gateway_filter(
    registry: Option<Arc<UpstreamAgentRegistry>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    discovery::well_known_filter(registry.clone())
        .or(jsonrpc_filter(registry.clone()))
        .or(streaming_filter(registry.clone()))
        .or(rest_send_filter(registry.clone()))
        .or(rest_stream_filter(registry.clone()))
        .or(rest_get_task_filter(registry.clone()))
        .or(rest_list_tasks_filter(registry.clone()))
        .or(rest_cancel_filter(registry.clone()))
        .or(rest_subscribe_filter(registry.clone()))
        .or(rest_card_filter(registry.clone()))
        .or(rest_extended_card_filter(registry.clone()))
        .or(rest_push_create_filter(registry.clone()))
        .or(rest_push_get_filter(registry.clone()))
        .or(rest_push_list_filter(registry.clone()))
        .or(rest_push_delete_filter(registry))
}

// -- Agent lookup helper ---------------------------------------------------------

fn require_agent<'a>(
    registry: &'a Option<Arc<UpstreamAgentRegistry>>,
    agent_name: &str,
) -> Result<&'a UpstreamA2aAgent, A2aGatewayError> {
    let reg = registry
        .as_ref()
        .ok_or_else(|| A2aGatewayError::AgentNotFound {
            name: agent_name.to_string(),
        })?;
    reg.require_agent(agent_name)
}

// -- JSON-RPC dispatch -----------------------------------------------------------

fn jsonrpc_filter(
    registry: Option<Arc<UpstreamAgentRegistry>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::body::json::<JsonRpcRequest>())
        .and(warp::any().map(move || registry.clone()))
        .then(
            |agent_name: String,
             request: JsonRpcRequest,
             registry: Option<Arc<UpstreamAgentRegistry>>| async move {
                let agent = match require_agent(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => {
                        let resp = error_response(&request.id, -32001, e.to_string());
                        return Box::new(warp::reply::json(&resp)) as Box<dyn warp::Reply>;
                    }
                };

                match request.method.as_str() {
                    "message/stream" | "tasks/resubscribe" => {
                        messaging::handle_streaming_jsonrpc(request, agent).await
                    }
                    _ => {
                        let resp = dispatch_jsonrpc(request, agent).await;
                        Box::new(warp::reply::json(&resp)) as Box<dyn warp::Reply>
                    }
                }
            },
        )
}

async fn dispatch_jsonrpc(request: JsonRpcRequest, agent: &UpstreamA2aAgent) -> JsonRpcResponse {
    match request.method.as_str() {
        "message/send" => messaging::dispatch_send_message(&request, agent).await,
        "tasks/get" => tasks::dispatch_get_task(&request, agent).await,
        "tasks/cancel" => tasks::dispatch_cancel_task(&request, agent).await,
        "tasks/list" => tasks::dispatch_list_tasks(&request, agent).await,
        "agent/getAuthenticatedExtendedCard" => {
            discovery::dispatch_get_extended(&request, agent).await
        }
        "tasks/pushNotificationConfig/set" => push::dispatch_set_push(&request, agent).await,
        "tasks/pushNotificationConfig/get" => push::dispatch_get_push(&request, agent).await,
        "tasks/pushNotificationConfig/list" => push::dispatch_list_push(&request, agent).await,
        "tasks/pushNotificationConfig/delete" => push::dispatch_delete_push(&request, agent).await,
        _ => error_response(
            &request.id,
            -32601,
            format!("method not found: {}", request.method),
        ),
    }
}

// -- SSE streaming ---------------------------------------------------------------

fn streaming_filter(
    registry: Option<Arc<UpstreamAgentRegistry>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("stream"))
        .and(warp::post())
        .and(warp::body::json::<JsonRpcRequest>())
        .and(warp::any().map(move || registry.clone()))
        .then(
            |agent_name: String,
             request: JsonRpcRequest,
             registry: Option<Arc<UpstreamAgentRegistry>>| async move {
                let agent = match require_agent(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => {
                        let resp = error_response(&request.id, -32001, e.to_string());
                        return Box::new(warp::reply::with_status(
                            warp::reply::json(&resp),
                            warp::http::StatusCode::NOT_FOUND,
                        )) as Box<dyn warp::Reply>;
                    }
                };
                messaging::handle_streaming_jsonrpc(request, agent).await
            },
        )
}

// -- REST endpoints --------------------------------------------------------------

fn rest_error_reply(status: warp::http::StatusCode, message: &str) -> Box<dyn warp::Reply> {
    Box::new(warp::reply::with_status(
        warp::reply::json(&serde_json::json!({"error": message})),
        status,
    ))
}

fn rest_gateway_error_reply(err: &A2aGatewayError) -> Box<dyn warp::Reply> {
    let status = match err {
        A2aGatewayError::AgentNotFound { .. } => warp::http::StatusCode::NOT_FOUND,
        A2aGatewayError::InvalidConfig { .. } => warp::http::StatusCode::BAD_REQUEST,
        _ => warp::http::StatusCode::BAD_GATEWAY,
    };
    rest_error_reply(status, &err.to_string())
}

fn rest_send_filter(
    registry: Option<Arc<UpstreamAgentRegistry>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("message:send"))
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::body::json::<SendMessageRequest>())
        .and(warp::any().map(move || registry.clone()))
        .then(
            |agent_name: String,
             body: SendMessageRequest,
             registry: Option<Arc<UpstreamAgentRegistry>>| async move {
                let agent = match require_agent(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return rest_gateway_error_reply(&e),
                };
                match agent.send_message(body).await {
                    Ok(result) => Box::new(warp::reply::json(&result)) as Box<dyn warp::Reply>,
                    Err(e) => rest_gateway_error_reply(&e),
                }
            },
        )
}

fn rest_stream_filter(
    registry: Option<Arc<UpstreamAgentRegistry>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("message:stream"))
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::body::json::<SendMessageRequest>())
        .and(warp::any().map(move || registry.clone()))
        .then(
            |agent_name: String,
             body: SendMessageRequest,
             registry: Option<Arc<UpstreamAgentRegistry>>| async move {
                let agent = match require_agent(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return rest_gateway_error_reply(&e),
                };
                match agent.send_streaming_message(body).await {
                    Ok(stream) => {
                        let event_stream =
                            sync_bridge(stream).map(|item| stream_response_to_sse("rest", &item));
                        Box::new(warp::sse::reply(event_stream)) as Box<dyn warp::Reply>
                    }
                    Err(e) => rest_gateway_error_reply(&e),
                }
            },
        )
}

fn rest_get_task_filter(
    registry: Option<Arc<UpstreamAgentRegistry>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("tasks"))
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(warp::get())
        .and(warp::any().map(move || registry.clone()))
        .then(
            |agent_name: String,
             task_id: String,
             registry: Option<Arc<UpstreamAgentRegistry>>| async move {
                let agent = match require_agent(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return rest_gateway_error_reply(&e),
                };
                let req = GetTaskRequest {
                    id: task_id,
                    history_length: None,
                };
                match agent.get_task(req).await {
                    Ok(task) => Box::new(warp::reply::json(&task)) as Box<dyn warp::Reply>,
                    Err(e) => rest_gateway_error_reply(&e),
                }
            },
        )
}

/// Query parameters for `GET /a2a/{agent}/tasks`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListTasksQueryParams {
    #[serde(default)]
    context_id: Option<String>,
    #[serde(default)]
    status: Option<bitrouter_a2a::types::TaskState>,
    #[serde(default)]
    status_timestamp_after: Option<String>,
    #[serde(default)]
    page_size: Option<u32>,
    #[serde(default)]
    page_token: Option<String>,
    #[serde(default)]
    history_length: Option<u32>,
    #[serde(default)]
    include_artifacts: Option<bool>,
}

fn rest_list_tasks_filter(
    registry: Option<Arc<UpstreamAgentRegistry>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("tasks"))
        .and(warp::path::end())
        .and(warp::get())
        .and(
            warp::query::<ListTasksQueryParams>()
                .or(warp::any().map(|| ListTasksQueryParams {
                    context_id: None,
                    status: None,
                    status_timestamp_after: None,
                    page_size: None,
                    page_token: None,
                    history_length: None,
                    include_artifacts: None,
                }))
                .unify(),
        )
        .and(warp::any().map(move || registry.clone()))
        .then(
            |agent_name: String,
             params: ListTasksQueryParams,
             registry: Option<Arc<UpstreamAgentRegistry>>| async move {
                let agent = match require_agent(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return rest_gateway_error_reply(&e),
                };
                let req = ListTasksRequest {
                    context_id: params.context_id,
                    status: params.status,
                    status_timestamp_after: params.status_timestamp_after,
                    page_size: params.page_size,
                    page_token: params.page_token,
                    history_length: params.history_length,
                    include_artifacts: params.include_artifacts,
                };
                match agent.list_tasks(req).await {
                    Ok(resp) => Box::new(warp::reply::json(&resp)) as Box<dyn warp::Reply>,
                    Err(e) => rest_gateway_error_reply(&e),
                }
            },
        )
}

fn rest_cancel_filter(
    registry: Option<Arc<UpstreamAgentRegistry>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("tasks"))
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::any().map(move || registry.clone()))
        .and_then(
            |agent_name: String,
             task_id_action: String,
             registry: Option<Arc<UpstreamAgentRegistry>>| async move {
                let Some(task_id) = task_id_action.strip_suffix(":cancel") else {
                    return Err(warp::reject::not_found());
                };
                let agent = match require_agent(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return Ok(rest_gateway_error_reply(&e)),
                };
                let req = CancelTaskRequest {
                    id: task_id.to_string(),
                };
                let reply: Box<dyn warp::Reply> = match agent.cancel_task(req).await {
                    Ok(task) => Box::new(warp::reply::json(&task)),
                    Err(e) => rest_gateway_error_reply(&e),
                };
                Ok(reply)
            },
        )
}

fn rest_subscribe_filter(
    registry: Option<Arc<UpstreamAgentRegistry>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("tasks"))
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::any().map(move || registry.clone()))
        .and_then(
            |agent_name: String,
             task_id_action: String,
             registry: Option<Arc<UpstreamAgentRegistry>>| async move {
                let Some(task_id) = task_id_action.strip_suffix(":subscribe") else {
                    return Err(warp::reject::not_found());
                };
                let agent = match require_agent(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return Ok(rest_gateway_error_reply(&e)),
                };
                let reply: Box<dyn warp::Reply> = match agent.subscribe_to_task(task_id).await {
                    Ok(stream) => {
                        let event_stream =
                            sync_bridge(stream).map(|item| stream_response_to_sse("rest", &item));
                        Box::new(warp::sse::reply(event_stream))
                    }
                    Err(e) => rest_gateway_error_reply(&e),
                };
                Ok(reply)
            },
        )
}

fn rest_card_filter(
    registry: Option<Arc<UpstreamAgentRegistry>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("card"))
        .and(warp::path::end())
        .and(warp::get())
        .and(warp::any().map(move || registry.clone()))
        .then(
            |agent_name: String, registry: Option<Arc<UpstreamAgentRegistry>>| async move {
                let reg = match registry.as_ref() {
                    Some(r) => r,
                    None => {
                        return rest_error_reply(
                            warp::http::StatusCode::NOT_FOUND,
                            "A2A gateway not configured",
                        );
                    }
                };
                match reg.rewritten_card(&agent_name).await {
                    Some(card) => Box::new(warp::reply::json(&card)) as Box<dyn warp::Reply>,
                    None => rest_error_reply(
                        warp::http::StatusCode::NOT_FOUND,
                        &format!("agent not found: {agent_name}"),
                    ),
                }
            },
        )
}

fn rest_extended_card_filter(
    registry: Option<Arc<UpstreamAgentRegistry>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("extendedAgentCard"))
        .and(warp::path::end())
        .and(warp::get())
        .and(warp::any().map(move || registry.clone()))
        .then(
            |agent_name: String, registry: Option<Arc<UpstreamAgentRegistry>>| async move {
                let agent = match require_agent(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return rest_gateway_error_reply(&e),
                };
                match agent.get_extended_agent_card().await {
                    Ok(card) => Box::new(warp::reply::json(&card)) as Box<dyn warp::Reply>,
                    Err(e) => rest_gateway_error_reply(&e),
                }
            },
        )
}

fn rest_push_create_filter(
    registry: Option<Arc<UpstreamAgentRegistry>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("tasks"))
        .and(warp::path::param::<String>())
        .and(warp::path("push-notification-configs"))
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::body::json::<TaskPushNotificationConfig>())
        .and(warp::any().map(move || registry.clone()))
        .then(
            |agent_name: String,
             _task_id: String,
             config: TaskPushNotificationConfig,
             registry: Option<Arc<UpstreamAgentRegistry>>| async move {
                let agent = match require_agent(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return rest_gateway_error_reply(&e),
                };
                match agent.set_push_config(config).await {
                    Ok(stored) => Box::new(warp::reply::with_status(
                        warp::reply::json(&stored),
                        warp::http::StatusCode::CREATED,
                    )) as Box<dyn warp::Reply>,
                    Err(e) => rest_gateway_error_reply(&e),
                }
            },
        )
}

fn rest_push_get_filter(
    registry: Option<Arc<UpstreamAgentRegistry>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("tasks"))
        .and(warp::path::param::<String>())
        .and(warp::path("push-notification-configs"))
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(warp::get())
        .and(warp::any().map(move || registry.clone()))
        .then(
            |agent_name: String,
             task_id: String,
             config_id: String,
             registry: Option<Arc<UpstreamAgentRegistry>>| async move {
                let agent = match require_agent(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return rest_gateway_error_reply(&e),
                };
                match agent.get_push_config(&task_id, Some(&config_id)).await {
                    Ok(config) => Box::new(warp::reply::json(&config)) as Box<dyn warp::Reply>,
                    Err(e) => rest_gateway_error_reply(&e),
                }
            },
        )
}

fn rest_push_list_filter(
    registry: Option<Arc<UpstreamAgentRegistry>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("tasks"))
        .and(warp::path::param::<String>())
        .and(warp::path("push-notification-configs"))
        .and(warp::path::end())
        .and(warp::get())
        .and(warp::any().map(move || registry.clone()))
        .then(
            |agent_name: String,
             task_id: String,
             registry: Option<Arc<UpstreamAgentRegistry>>| async move {
                let agent = match require_agent(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return rest_gateway_error_reply(&e),
                };
                match agent.list_push_configs(&task_id).await {
                    Ok(resp) => Box::new(warp::reply::json(&resp)) as Box<dyn warp::Reply>,
                    Err(e) => rest_gateway_error_reply(&e),
                }
            },
        )
}

fn rest_push_delete_filter(
    registry: Option<Arc<UpstreamAgentRegistry>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("tasks"))
        .and(warp::path::param::<String>())
        .and(warp::path("push-notification-configs"))
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(warp::delete())
        .and(warp::any().map(move || registry.clone()))
        .then(
            |agent_name: String,
             task_id: String,
             config_id: String,
             registry: Option<Arc<UpstreamAgentRegistry>>| async move {
                let agent = match require_agent(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return rest_gateway_error_reply(&e),
                };
                match agent.delete_push_config(&task_id, &config_id).await {
                    Ok(()) => Box::new(warp::reply::with_status(
                        warp::reply::json(&serde_json::json!({"success": true})),
                        warp::http::StatusCode::OK,
                    )) as Box<dyn warp::Reply>,
                    Err(e) => rest_gateway_error_reply(&e),
                }
            },
        )
}
