//! Warp filter assembly and JSON-RPC dispatch for A2A gateway proxy.
//!
//! All routes are scoped under `/a2a/{agent_name}/...` so that each
//! upstream agent is a fully independent A2A endpoint.

use std::sync::Arc;

use bitrouter_core::api::a2a::error::A2aGatewayError;
use bitrouter_core::api::a2a::gateway::{A2aGateway, A2aProxy};
use bitrouter_core::observe::{AgentObserveCallback, CallerContext};
use warp::Filter;

mod discovery;
mod messaging;
mod observe;
mod push;
mod tasks;

use bitrouter_core::api::a2a::types::*;
use observe::A2aObserveContext;

/// Combined A2A gateway filter: per-agent discovery + JSON-RPC + streaming.
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
        .or(streaming_filter(registry, observer, account_filter))
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
