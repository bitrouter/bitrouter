//! Discovery handlers for A2A gateway.

use std::sync::Arc;

use bitrouter_core::api::a2a::error::A2aGatewayError;
use bitrouter_core::api::a2a::gateway::{A2aGateway, A2aProxy};
use tokio::time::Instant;
use warp::Filter;

use super::observe::{A2aObserveContext, emit_agent_failure, emit_agent_success};
use bitrouter_core::api::a2a::types::*;

/// Warp filter for `GET /a2a/{agent_name}/.well-known/agent-card.json`.
pub(crate) fn well_known_filter<G: A2aGateway + 'static>(
    registry: Option<Arc<G>>,
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
             registry: Option<Arc<G>>,
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
                match reg.get_card(&agent_name).await {
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
                        emit_agent_failure(
                            &ctx,
                            &agent_name,
                            ".well-known/agent-card.json",
                            start,
                            &A2aGatewayError::AgentNotFound {
                                name: agent_name.clone(),
                            }
                            .to_string(),
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
    agent: &impl A2aProxy,
    agent_name: &str,
    ctx: &Option<A2aObserveContext>,
) -> JsonRpcResponse {
    let start = Instant::now();
    let result = agent.get_extended_agent_card().await;
    match &result {
        Ok(_) => emit_agent_success(ctx, agent_name, "agent/getAuthenticatedExtendedCard", start),
        Err(e) => emit_agent_failure(
            ctx,
            agent_name,
            "agent/getAuthenticatedExtendedCard",
            start,
            &e.to_string(),
        ),
    }
    match result {
        Ok(card) => JsonRpcResponse::success(&request.id, &card),
        Err(e) => JsonRpcResponse::gateway_error(&request.id, &e),
    }
}
