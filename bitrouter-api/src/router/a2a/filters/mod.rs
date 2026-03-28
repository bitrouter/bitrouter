//! Warp filter assembly and JSON-RPC dispatch for A2A gateway proxy.
//!
//! All routes are scoped under `/a2a/{agent_name}/...` so that each
//! upstream agent is a fully independent A2A endpoint.

use std::sync::Arc;

use bitrouter_core::api::a2a::error::A2aGatewayError;
use bitrouter_core::api::a2a::gateway::{A2aGateway, A2aProxy};
use bitrouter_core::observe::{AgentObserveCallback, CallerContext};
use serde::Deserialize;
use tokio::time::Instant;
use tokio_stream::StreamExt;
use warp::Filter;

mod discovery;
mod messaging;
mod observe;
mod push;
mod tasks;

use super::types::*;
use messaging::{stream_response_to_sse, sync_bridge_with_observe};
use observe::{A2aObserveContext, emit_agent_failure, emit_agent_success};

/// Combined A2A gateway filter: per-agent discovery + JSON-RPC + streaming + REST.
///
/// Routes are prefixed with `/a2a/{agent_name}` so each upstream agent is
/// addressed independently.
///
/// When `observer` and `account_filter` are provided, every handler fires
/// observation events through the observer with caller context.
pub fn a2a_gateway_filter<G, A>(
    registry: Option<Arc<G>>,
    observer: Option<Arc<dyn AgentObserveCallback>>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    G: A2aGateway + 'static,
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    // For discovery endpoints (no auth required), use a static observe context
    // with a default CallerContext.
    let discovery_ctx = observer.as_ref().map(|obs| A2aObserveContext {
        observer: obs.clone(),
        caller: CallerContext::default(),
    });

    discovery::well_known_filter(registry.clone(), discovery_ctx.clone())
        .or(jsonrpc_filter(
            registry.clone(),
            observer.clone(),
            account_filter.clone(),
        ))
        .or(streaming_filter(
            registry.clone(),
            observer.clone(),
            account_filter.clone(),
        ))
        .or(rest_send_filter(
            registry.clone(),
            observer.clone(),
            account_filter.clone(),
        ))
        .or(rest_stream_filter(
            registry.clone(),
            observer.clone(),
            account_filter.clone(),
        ))
        .or(rest_get_task_filter(
            registry.clone(),
            observer.clone(),
            account_filter.clone(),
        ))
        .or(rest_list_tasks_filter(
            registry.clone(),
            observer.clone(),
            account_filter.clone(),
        ))
        .or(rest_cancel_filter(
            registry.clone(),
            observer.clone(),
            account_filter.clone(),
        ))
        .or(rest_subscribe_filter(
            registry.clone(),
            observer.clone(),
            account_filter.clone(),
        ))
        .or(rest_card_filter(registry.clone(), discovery_ctx.clone()))
        .or(rest_extended_card_filter(
            registry.clone(),
            observer.clone(),
            account_filter.clone(),
        ))
        .or(rest_push_create_filter(
            registry.clone(),
            observer.clone(),
            account_filter.clone(),
        ))
        .or(rest_push_get_filter(
            registry.clone(),
            observer.clone(),
            account_filter.clone(),
        ))
        .or(rest_push_list_filter(
            registry.clone(),
            observer.clone(),
            account_filter.clone(),
        ))
        .or(rest_push_delete_filter(registry, observer, account_filter))
}

// -- Agent lookup helper ---------------------------------------------------------

fn require_agent<'a, G: A2aGateway>(
    registry: &'a Option<Arc<G>>,
    agent_name: &str,
) -> Result<&'a G::Agent, A2aGatewayError> {
    let reg = registry
        .as_ref()
        .ok_or_else(|| A2aGatewayError::AgentNotFound {
            name: agent_name.to_string(),
        })?;
    reg.require_agent(agent_name)
}

/// Build an `Option<A2aObserveContext>` from per-request caller and shared observer.
fn make_ctx(
    observer: &Option<Arc<dyn AgentObserveCallback>>,
    caller: CallerContext,
) -> Option<A2aObserveContext> {
    observer.as_ref().map(|obs| A2aObserveContext {
        observer: obs.clone(),
        caller,
    })
}

// -- JSON-RPC dispatch -----------------------------------------------------------

fn jsonrpc_filter<G: A2aGateway + 'static, A>(
    registry: Option<Arc<G>>,
    observer: Option<Arc<dyn AgentObserveCallback>>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::body::json::<JsonRpcRequest>())
        .and(warp::any().map(move || registry.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and(account_filter)
        .then(
            |agent_name: String,
             request: JsonRpcRequest,
             registry: Option<Arc<G>>,
             observer: Option<Arc<dyn AgentObserveCallback>>,
             caller: CallerContext| async move {
                let ctx = make_ctx(&observer, caller);
                let agent = match require_agent::<G>(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => {
                        let resp = JsonRpcResponse::error(&request.id, -32001, e.to_string());
                        return Box::new(warp::reply::json(&resp)) as Box<dyn warp::Reply>;
                    }
                };

                match request.method.as_str() {
                    "message/stream" | "tasks/resubscribe" => {
                        messaging::handle_streaming_jsonrpc(request, agent, &agent_name, &ctx).await
                    }
                    _ => {
                        let resp = dispatch_jsonrpc(request, agent, &agent_name, &ctx).await;
                        Box::new(warp::reply::json(&resp)) as Box<dyn warp::Reply>
                    }
                }
            },
        )
}

async fn dispatch_jsonrpc(
    request: JsonRpcRequest,
    agent: &impl A2aProxy,
    agent_name: &str,
    ctx: &Option<A2aObserveContext>,
) -> JsonRpcResponse {
    match request.method.as_str() {
        "message/send" => messaging::dispatch_send_message(&request, agent, agent_name, ctx).await,
        "tasks/get" => tasks::dispatch_get_task(&request, agent, agent_name, ctx).await,
        "tasks/cancel" => tasks::dispatch_cancel_task(&request, agent, agent_name, ctx).await,
        "tasks/list" => tasks::dispatch_list_tasks(&request, agent, agent_name, ctx).await,
        "agent/getAuthenticatedExtendedCard" => {
            discovery::dispatch_get_extended(&request, agent, agent_name, ctx).await
        }
        "tasks/pushNotificationConfig/set" => {
            push::dispatch_set_push(&request, agent, agent_name, ctx).await
        }
        "tasks/pushNotificationConfig/get" => {
            push::dispatch_get_push(&request, agent, agent_name, ctx).await
        }
        "tasks/pushNotificationConfig/list" => {
            push::dispatch_list_push(&request, agent, agent_name, ctx).await
        }
        "tasks/pushNotificationConfig/delete" => {
            push::dispatch_delete_push(&request, agent, agent_name, ctx).await
        }
        _ => JsonRpcResponse::error(
            &request.id,
            -32601,
            format!("method not found: {}", request.method),
        ),
    }
}

// -- SSE streaming ---------------------------------------------------------------

fn streaming_filter<G: A2aGateway + 'static, A>(
    registry: Option<Arc<G>>,
    observer: Option<Arc<dyn AgentObserveCallback>>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("stream"))
        .and(warp::post())
        .and(warp::body::json::<JsonRpcRequest>())
        .and(warp::any().map(move || registry.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and(account_filter)
        .then(
            |agent_name: String,
             request: JsonRpcRequest,
             registry: Option<Arc<G>>,
             observer: Option<Arc<dyn AgentObserveCallback>>,
             caller: CallerContext| async move {
                let ctx = make_ctx(&observer, caller);
                let agent = match require_agent::<G>(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => {
                        let resp = JsonRpcResponse::error(&request.id, -32001, e.to_string());
                        return Box::new(warp::reply::with_status(
                            warp::reply::json(&resp),
                            warp::http::StatusCode::NOT_FOUND,
                        )) as Box<dyn warp::Reply>;
                    }
                };
                messaging::handle_streaming_jsonrpc(request, agent, &agent_name, &ctx).await
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

fn rest_send_filter<G: A2aGateway + 'static, A>(
    registry: Option<Arc<G>>,
    observer: Option<Arc<dyn AgentObserveCallback>>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("message:send"))
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::body::json::<SendMessageRequest>())
        .and(warp::any().map(move || registry.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and(account_filter)
        .then(
            |agent_name: String,
             body: SendMessageRequest,
             registry: Option<Arc<G>>,
             observer: Option<Arc<dyn AgentObserveCallback>>,
             caller: CallerContext| async move {
                let ctx = make_ctx(&observer, caller);
                let agent = match require_agent::<G>(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return rest_gateway_error_reply(&e),
                };
                let start = Instant::now();
                let result = agent.send_message(body).await;
                match &result {
                    Ok(_) => emit_agent_success(&ctx, &agent_name, "message/send", start),
                    Err(e) => {
                        emit_agent_failure(&ctx, &agent_name, "message/send", start, &e.to_string())
                    }
                }
                match result {
                    Ok(r) => Box::new(warp::reply::json(&r)) as Box<dyn warp::Reply>,
                    Err(e) => rest_gateway_error_reply(&e),
                }
            },
        )
}

fn rest_stream_filter<G: A2aGateway + 'static, A>(
    registry: Option<Arc<G>>,
    observer: Option<Arc<dyn AgentObserveCallback>>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("message:stream"))
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::body::json::<SendMessageRequest>())
        .and(warp::any().map(move || registry.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and(account_filter)
        .then(
            |agent_name: String,
             body: SendMessageRequest,
             registry: Option<Arc<G>>,
             observer: Option<Arc<dyn AgentObserveCallback>>,
             caller: CallerContext| async move {
                let ctx = make_ctx(&observer, caller);
                let agent = match require_agent::<G>(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return rest_gateway_error_reply(&e),
                };
                let start = Instant::now();
                let result = agent.send_streaming_message(body).await;
                match result {
                    Ok(stream) => {
                        let event_stream = sync_bridge_with_observe(
                            stream,
                            agent_name,
                            "message/stream".to_string(),
                            start,
                            ctx,
                        )
                        .map(|item| stream_response_to_sse("rest", &item));
                        Box::new(warp::sse::reply(event_stream)) as Box<dyn warp::Reply>
                    }
                    Err(ref e) => {
                        emit_agent_failure(
                            &ctx,
                            &agent_name,
                            "message/stream",
                            start,
                            &e.to_string(),
                        );
                        rest_gateway_error_reply(e)
                    }
                }
            },
        )
}

fn rest_get_task_filter<G: A2aGateway + 'static, A>(
    registry: Option<Arc<G>>,
    observer: Option<Arc<dyn AgentObserveCallback>>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("tasks"))
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(warp::get())
        .and(warp::any().map(move || registry.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and(account_filter)
        .then(
            |agent_name: String,
             task_id: String,
             registry: Option<Arc<G>>,
             observer: Option<Arc<dyn AgentObserveCallback>>,
             caller: CallerContext| async move {
                let ctx = make_ctx(&observer, caller);
                let agent = match require_agent::<G>(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return rest_gateway_error_reply(&e),
                };
                let req = GetTaskRequest {
                    id: task_id,
                    history_length: None,
                };
                let start = Instant::now();
                let result = agent.get_task(req).await;
                match &result {
                    Ok(_) => emit_agent_success(&ctx, &agent_name, "tasks/get", start),
                    Err(e) => {
                        emit_agent_failure(&ctx, &agent_name, "tasks/get", start, &e.to_string())
                    }
                }
                match result {
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
    status: Option<bitrouter_core::api::a2a::types::TaskState>,
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

fn rest_list_tasks_filter<G: A2aGateway + 'static, A>(
    registry: Option<Arc<G>>,
    observer: Option<Arc<dyn AgentObserveCallback>>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
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
        .and(warp::any().map(move || observer.clone()))
        .and(account_filter)
        .then(
            |agent_name: String,
             params: ListTasksQueryParams,
             registry: Option<Arc<G>>,
             observer: Option<Arc<dyn AgentObserveCallback>>,
             caller: CallerContext| async move {
                let ctx = make_ctx(&observer, caller);
                let agent = match require_agent::<G>(&registry, &agent_name) {
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
                let start = Instant::now();
                let result = agent.list_tasks(req).await;
                match &result {
                    Ok(_) => emit_agent_success(&ctx, &agent_name, "tasks/list", start),
                    Err(e) => {
                        emit_agent_failure(&ctx, &agent_name, "tasks/list", start, &e.to_string())
                    }
                }
                match result {
                    Ok(resp) => Box::new(warp::reply::json(&resp)) as Box<dyn warp::Reply>,
                    Err(e) => rest_gateway_error_reply(&e),
                }
            },
        )
}

fn rest_cancel_filter<G: A2aGateway + 'static, A>(
    registry: Option<Arc<G>>,
    observer: Option<Arc<dyn AgentObserveCallback>>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("tasks"))
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::any().map(move || registry.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and(account_filter)
        .and_then(
            |agent_name: String,
             task_id_action: String,
             registry: Option<Arc<G>>,
             observer: Option<Arc<dyn AgentObserveCallback>>,
             caller: CallerContext| async move {
                let Some(task_id) = task_id_action.strip_suffix(":cancel") else {
                    return Err(warp::reject::not_found());
                };
                let ctx = make_ctx(&observer, caller);
                let agent = match require_agent::<G>(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return Ok(rest_gateway_error_reply(&e)),
                };
                let req = CancelTaskRequest {
                    id: task_id.to_string(),
                };
                let start = Instant::now();
                let result = agent.cancel_task(req).await;
                match &result {
                    Ok(_) => emit_agent_success(&ctx, &agent_name, "tasks/cancel", start),
                    Err(e) => {
                        emit_agent_failure(&ctx, &agent_name, "tasks/cancel", start, &e.to_string())
                    }
                }
                let reply: Box<dyn warp::Reply> = match result {
                    Ok(task) => Box::new(warp::reply::json(&task)),
                    Err(e) => rest_gateway_error_reply(&e),
                };
                Ok(reply)
            },
        )
}

fn rest_subscribe_filter<G: A2aGateway + 'static, A>(
    registry: Option<Arc<G>>,
    observer: Option<Arc<dyn AgentObserveCallback>>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("tasks"))
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::any().map(move || registry.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and(account_filter)
        .and_then(
            |agent_name: String,
             task_id_action: String,
             registry: Option<Arc<G>>,
             observer: Option<Arc<dyn AgentObserveCallback>>,
             caller: CallerContext| async move {
                let Some(task_id) = task_id_action.strip_suffix(":subscribe") else {
                    return Err(warp::reject::not_found());
                };
                let ctx = make_ctx(&observer, caller);
                let agent = match require_agent::<G>(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return Ok(rest_gateway_error_reply(&e)),
                };
                let start = Instant::now();
                let result = agent.subscribe_to_task(task_id).await;
                let reply: Box<dyn warp::Reply> = match result {
                    Ok(stream) => {
                        let event_stream = sync_bridge_with_observe(
                            stream,
                            agent_name,
                            "tasks/resubscribe".to_string(),
                            start,
                            ctx,
                        )
                        .map(|item| stream_response_to_sse("rest", &item));
                        Box::new(warp::sse::reply(event_stream))
                    }
                    Err(ref e) => {
                        emit_agent_failure(
                            &ctx,
                            &agent_name,
                            "tasks/resubscribe",
                            start,
                            &e.to_string(),
                        );
                        rest_gateway_error_reply(e)
                    }
                };
                Ok(reply)
            },
        )
}

fn rest_card_filter<G: A2aGateway + 'static>(
    registry: Option<Arc<G>>,
    ctx: Option<A2aObserveContext>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("card"))
        .and(warp::path::end())
        .and(warp::get())
        .and(warp::any().map(move || registry.clone()))
        .and(warp::any().map(move || ctx.clone()))
        .then(
            |agent_name: String,
             registry: Option<Arc<G>>,
             ctx: Option<A2aObserveContext>| async move {
                let reg = match registry.as_ref() {
                    Some(r) => r,
                    None => {
                        return rest_error_reply(
                            warp::http::StatusCode::NOT_FOUND,
                            "A2A gateway not configured",
                        );
                    }
                };
                let start = Instant::now();
                let result = reg.get_card(&agent_name).await;
                match result {
                    Some(card) => {
                        emit_agent_success(&ctx, &agent_name, "card/get", start);
                        Box::new(warp::reply::json(&card)) as Box<dyn warp::Reply>
                    }
                    None => {
                        emit_agent_failure(
                            &ctx,
                            &agent_name,
                            "card/get",
                            start,
                            &A2aGatewayError::AgentNotFound {
                                name: agent_name.clone(),
                            }
                            .to_string(),
                        );
                        rest_error_reply(
                            warp::http::StatusCode::NOT_FOUND,
                            &format!("agent not found: {agent_name}"),
                        )
                    }
                }
            },
        )
}

fn rest_extended_card_filter<G: A2aGateway + 'static, A>(
    registry: Option<Arc<G>>,
    observer: Option<Arc<dyn AgentObserveCallback>>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("extendedAgentCard"))
        .and(warp::path::end())
        .and(warp::get())
        .and(warp::any().map(move || registry.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and(account_filter)
        .then(
            |agent_name: String,
             registry: Option<Arc<G>>,
             observer: Option<Arc<dyn AgentObserveCallback>>,
             caller: CallerContext| async move {
                let ctx = make_ctx(&observer, caller);
                let agent = match require_agent::<G>(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return rest_gateway_error_reply(&e),
                };
                let start = Instant::now();
                let result = agent.get_extended_agent_card().await;
                match &result {
                    Ok(_) => emit_agent_success(
                        &ctx,
                        &agent_name,
                        "agent/getAuthenticatedExtendedCard",
                        start,
                    ),
                    Err(e) => emit_agent_failure(
                        &ctx,
                        &agent_name,
                        "agent/getAuthenticatedExtendedCard",
                        start,
                        &e.to_string(),
                    ),
                }
                match result {
                    Ok(card) => Box::new(warp::reply::json(&card)) as Box<dyn warp::Reply>,
                    Err(e) => rest_gateway_error_reply(&e),
                }
            },
        )
}

fn rest_push_create_filter<G: A2aGateway + 'static, A>(
    registry: Option<Arc<G>>,
    observer: Option<Arc<dyn AgentObserveCallback>>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("tasks"))
        .and(warp::path::param::<String>())
        .and(warp::path("push-notification-configs"))
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::body::json::<TaskPushNotificationConfig>())
        .and(warp::any().map(move || registry.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and(account_filter)
        .then(
            |agent_name: String,
             _task_id: String,
             config: TaskPushNotificationConfig,
             registry: Option<Arc<G>>,
             observer: Option<Arc<dyn AgentObserveCallback>>,
             caller: CallerContext| async move {
                let ctx = make_ctx(&observer, caller);
                let agent = match require_agent::<G>(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return rest_gateway_error_reply(&e),
                };
                let start = Instant::now();
                let result = agent.set_push_config(config).await;
                match &result {
                    Ok(_) => emit_agent_success(
                        &ctx,
                        &agent_name,
                        "tasks/pushNotificationConfig/set",
                        start,
                    ),
                    Err(e) => emit_agent_failure(
                        &ctx,
                        &agent_name,
                        "tasks/pushNotificationConfig/set",
                        start,
                        &e.to_string(),
                    ),
                }
                match result {
                    Ok(stored) => Box::new(warp::reply::with_status(
                        warp::reply::json(&stored),
                        warp::http::StatusCode::CREATED,
                    )) as Box<dyn warp::Reply>,
                    Err(e) => rest_gateway_error_reply(&e),
                }
            },
        )
}

fn rest_push_get_filter<G: A2aGateway + 'static, A>(
    registry: Option<Arc<G>>,
    observer: Option<Arc<dyn AgentObserveCallback>>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("tasks"))
        .and(warp::path::param::<String>())
        .and(warp::path("push-notification-configs"))
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(warp::get())
        .and(warp::any().map(move || registry.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and(account_filter)
        .then(
            |agent_name: String,
             task_id: String,
             config_id: String,
             registry: Option<Arc<G>>,
             observer: Option<Arc<dyn AgentObserveCallback>>,
             caller: CallerContext| async move {
                let ctx = make_ctx(&observer, caller);
                let agent = match require_agent::<G>(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return rest_gateway_error_reply(&e),
                };
                let start = Instant::now();
                let result = agent.get_push_config(&task_id, Some(&config_id)).await;
                match &result {
                    Ok(_) => emit_agent_success(
                        &ctx,
                        &agent_name,
                        "tasks/pushNotificationConfig/get",
                        start,
                    ),
                    Err(e) => emit_agent_failure(
                        &ctx,
                        &agent_name,
                        "tasks/pushNotificationConfig/get",
                        start,
                        &e.to_string(),
                    ),
                }
                match result {
                    Ok(config) => Box::new(warp::reply::json(&config)) as Box<dyn warp::Reply>,
                    Err(e) => rest_gateway_error_reply(&e),
                }
            },
        )
}

fn rest_push_list_filter<G: A2aGateway + 'static, A>(
    registry: Option<Arc<G>>,
    observer: Option<Arc<dyn AgentObserveCallback>>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("tasks"))
        .and(warp::path::param::<String>())
        .and(warp::path("push-notification-configs"))
        .and(warp::path::end())
        .and(warp::get())
        .and(warp::any().map(move || registry.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and(account_filter)
        .then(
            |agent_name: String,
             task_id: String,
             registry: Option<Arc<G>>,
             observer: Option<Arc<dyn AgentObserveCallback>>,
             caller: CallerContext| async move {
                let ctx = make_ctx(&observer, caller);
                let agent = match require_agent::<G>(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return rest_gateway_error_reply(&e),
                };
                let start = Instant::now();
                let result = agent.list_push_configs(&task_id).await;
                match &result {
                    Ok(_) => emit_agent_success(
                        &ctx,
                        &agent_name,
                        "tasks/pushNotificationConfig/list",
                        start,
                    ),
                    Err(e) => emit_agent_failure(
                        &ctx,
                        &agent_name,
                        "tasks/pushNotificationConfig/list",
                        start,
                        &e.to_string(),
                    ),
                }
                match result {
                    Ok(resp) => Box::new(warp::reply::json(&resp)) as Box<dyn warp::Reply>,
                    Err(e) => rest_gateway_error_reply(&e),
                }
            },
        )
}

fn rest_push_delete_filter<G: A2aGateway + 'static, A>(
    registry: Option<Arc<G>>,
    observer: Option<Arc<dyn AgentObserveCallback>>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path("tasks"))
        .and(warp::path::param::<String>())
        .and(warp::path("push-notification-configs"))
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(warp::delete())
        .and(warp::any().map(move || registry.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and(account_filter)
        .then(
            |agent_name: String,
             task_id: String,
             config_id: String,
             registry: Option<Arc<G>>,
             observer: Option<Arc<dyn AgentObserveCallback>>,
             caller: CallerContext| async move {
                let ctx = make_ctx(&observer, caller);
                let agent = match require_agent::<G>(&registry, &agent_name) {
                    Ok(a) => a,
                    Err(e) => return rest_gateway_error_reply(&e),
                };
                let start = Instant::now();
                let result = agent.delete_push_config(&task_id, &config_id).await;
                match &result {
                    Ok(_) => emit_agent_success(
                        &ctx,
                        &agent_name,
                        "tasks/pushNotificationConfig/delete",
                        start,
                    ),
                    Err(e) => emit_agent_failure(
                        &ctx,
                        &agent_name,
                        "tasks/pushNotificationConfig/delete",
                        start,
                        &e.to_string(),
                    ),
                }
                match result {
                    Ok(()) => Box::new(warp::reply::with_status(
                        warp::reply::json(&serde_json::json!({"success": true})),
                        warp::http::StatusCode::OK,
                    )) as Box<dyn warp::Reply>,
                    Err(e) => rest_gateway_error_reply(&e),
                }
            },
        )
}
