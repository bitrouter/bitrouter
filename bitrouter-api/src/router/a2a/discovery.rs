//! Discovery handlers for A2A gateway.

use std::sync::Arc;

use warp::Filter;

use super::convert::{gateway_error_response, success_response};
use super::types::*;

/// Warp filter for `GET /.well-known/agent-card.json`.
pub(crate) fn well_known_filter<T>(
    gateway: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: A2aGateway + 'static,
{
    warp::path!(".well-known" / "agent-card.json")
        .and(warp::get())
        .and(warp::any().map(move || gateway.clone()))
        .then(|gateway: Option<Arc<T>>| async move {
            let Some(gw) = gateway else {
                return Box::new(warp::reply::with_status(
                    warp::reply::json(&serde_json::json!({"error": "A2A gateway not configured"})),
                    warp::http::StatusCode::NOT_FOUND,
                )) as Box<dyn warp::Reply>;
            };
            match gw.get_agent_card().await {
                Some(card) => {
                    let etag = format!("\"{}\"", card.version);
                    let json = warp::reply::json(&card);
                    let reply = warp::reply::with_header(json, "Cache-Control", "max-age=3600");
                    let reply = warp::reply::with_header(reply, "ETag", etag);
                    Box::new(reply) as Box<dyn warp::Reply>
                }
                None => Box::new(warp::reply::with_status(
                    warp::reply::json(&serde_json::json!({"error": "no agent card available"})),
                    warp::http::StatusCode::NOT_FOUND,
                )),
            }
        })
}

/// Handle `agent/getAuthenticatedExtendedCard` JSON-RPC method.
pub(crate) async fn dispatch_get_extended<T: A2aProxy>(
    request: &JsonRpcRequest,
    gw: &T,
) -> JsonRpcResponse {
    match gw.get_extended_agent_card().await {
        Ok(card) => success_response(&request.id, &card),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}
