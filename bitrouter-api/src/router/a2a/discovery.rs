//! Discovery handlers for A2A gateway.

use std::sync::Arc;

use bitrouter_a2a::client::registry::UpstreamAgentRegistry;
use bitrouter_a2a::client::upstream::UpstreamA2aAgent;
use bitrouter_a2a::error::A2aGatewayError;
use tokio::time::Instant;
use warp::Filter;

use super::convert::{gateway_error_response, success_response};
use super::observe::{A2aObserveContext, emit_agent_error, emit_agent_event, emit_agent_success};
use super::types::*;

/// Warp filter for `GET /a2a/{agent_name}/.well-known/agent-card.json`.
pub(crate) fn well_known_filter(
    registry: Option<Arc<UpstreamAgentRegistry>>,
    ctx: Option<A2aObserveContext>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path!(".well-known" / "agent-card.json"))
        .and(warp::get())
        .and(warp::any().map(move || registry.clone()))
        .and(warp::any().map(move || ctx.clone()))
        .then(
            |agent_name: String,
             registry: Option<Arc<UpstreamAgentRegistry>>,
             ctx: Option<A2aObserveContext>| async move {
                let Some(reg) = registry.as_ref() else {
                    return Box::new(warp::reply::with_status(
                        warp::reply::json(
                            &serde_json::json!({"error": "A2A gateway not configured"}),
                        ),
                        warp::http::StatusCode::NOT_FOUND,
                    )) as Box<dyn warp::Reply>;
                };
                let start = Instant::now();
                match reg.rewritten_card(&agent_name).await {
                    Some(card) => {
                        emit_agent_success(
                            &ctx,
                            &agent_name,
                            ".well-known/agent-card.json",
                            start,
                        );
                        let etag = format!("\"{}\"", card.version);
                        let json = warp::reply::json(&card);
                        let reply = warp::reply::with_header(json, "Cache-Control", "max-age=3600");
                        let reply = warp::reply::with_header(reply, "ETag", etag);
                        Box::new(reply) as Box<dyn warp::Reply>
                    }
                    None => {
                        emit_agent_error(
                            &ctx,
                            &agent_name,
                            ".well-known/agent-card.json",
                            start,
                            &A2aGatewayError::AgentNotFound {
                                name: agent_name.clone(),
                            },
                        );
                        Box::new(warp::reply::with_status(
                            warp::reply::json(
                                &serde_json::json!({"error": format!("agent not found: {agent_name}")}),
                            ),
                            warp::http::StatusCode::NOT_FOUND,
                        ))
                    }
                }
            },
        )
}

/// Handle `agent/getAuthenticatedExtendedCard` JSON-RPC method.
pub(crate) async fn dispatch_get_extended(
    request: &JsonRpcRequest,
    agent: &UpstreamA2aAgent,
    agent_name: &str,
    ctx: &Option<A2aObserveContext>,
) -> JsonRpcResponse {
    let start = Instant::now();
    let result = agent.get_extended_agent_card().await;
    emit_agent_event(
        ctx,
        agent_name,
        "agent/getAuthenticatedExtendedCard",
        start,
        &result,
    );
    match result {
        Ok(card) => success_response(&request.id, &card),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}
