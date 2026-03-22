//! Discovery handlers for A2A gateway.

use std::sync::Arc;

use bitrouter_a2a::client::registry::UpstreamAgentRegistry;
use bitrouter_a2a::client::upstream::UpstreamA2aAgent;
use warp::Filter;

use super::convert::{gateway_error_response, success_response};
use super::types::*;

/// Warp filter for `GET /a2a/{agent_name}/.well-known/agent-card.json`.
pub(crate) fn well_known_filter(
    registry: Option<Arc<UpstreamAgentRegistry>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    warp::path("a2a")
        .and(warp::path::param::<String>())
        .and(warp::path!(".well-known" / "agent-card.json"))
        .and(warp::get())
        .and(warp::any().map(move || registry.clone()))
        .then(
            |agent_name: String, registry: Option<Arc<UpstreamAgentRegistry>>| async move {
                let Some(reg) = registry.as_ref() else {
                    return Box::new(warp::reply::with_status(
                        warp::reply::json(
                            &serde_json::json!({"error": "A2A gateway not configured"}),
                        ),
                        warp::http::StatusCode::NOT_FOUND,
                    )) as Box<dyn warp::Reply>;
                };
                match reg.rewritten_card(&agent_name).await {
                    Some(card) => {
                        let etag = format!("\"{}\"", card.version);
                        let json = warp::reply::json(&card);
                        let reply = warp::reply::with_header(json, "Cache-Control", "max-age=3600");
                        let reply = warp::reply::with_header(reply, "ETag", etag);
                        Box::new(reply) as Box<dyn warp::Reply>
                    }
                    None => Box::new(warp::reply::with_status(
                        warp::reply::json(
                            &serde_json::json!({"error": format!("agent not found: {agent_name}")}),
                        ),
                        warp::http::StatusCode::NOT_FOUND,
                    )),
                }
            },
        )
}

/// Handle `agent/getAuthenticatedExtendedCard` JSON-RPC method.
pub(crate) async fn dispatch_get_extended(
    request: &JsonRpcRequest,
    agent: &UpstreamA2aAgent,
) -> JsonRpcResponse {
    match agent.get_extended_agent_card().await {
        Ok(card) => success_response(&request.id, &card),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}
