//! Warp filter assembly and JSON-RPC dispatch for A2A gateway proxy.

use std::sync::Arc;

use warp::Filter;

use super::convert::error_response;
use super::types::*;
use super::{discovery, messaging, push, rest, tasks};

/// Combined A2A gateway filter: discovery + JSON-RPC + streaming + REST.
pub fn a2a_gateway_filter<T>(
    gateway: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: A2aGateway + 'static,
{
    discovery::well_known_filter(gateway.clone())
        .or(jsonrpc_filter(gateway.clone()))
        .or(streaming_filter(gateway.clone()))
        .or(rest::rest_send_filter(gateway.clone()))
        .or(rest::rest_get_task_filter(gateway.clone()))
        .or(rest::rest_cancel_filter(gateway.clone()))
        .or(rest::rest_push_create_filter(gateway.clone()))
        .or(rest::rest_push_get_filter(gateway.clone()))
        .or(rest::rest_push_list_filter(gateway.clone()))
        .or(rest::rest_push_delete_filter(gateway))
}

// -- JSON-RPC dispatch -------------------------------------------------------

fn jsonrpc_filter<T>(
    gateway: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: A2aGateway + 'static,
{
    warp::path("a2a")
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::body::json::<JsonRpcRequest>())
        .and(warp::any().map(move || gateway.clone()))
        .then(
            |request: JsonRpcRequest, gateway: Option<Arc<T>>| async move {
                let Some(gw) = gateway else {
                    let resp = error_response(
                        &request.id,
                        -32000,
                        "A2A gateway not configured".to_string(),
                    );
                    return Box::new(warp::reply::json(&resp)) as Box<dyn warp::Reply>;
                };

                match request.method.as_str() {
                    "message/stream" | "tasks/resubscribe" => {
                        messaging::handle_streaming_jsonrpc(request, gw).await
                    }
                    _ => {
                        let resp = dispatch_jsonrpc(request, gw).await;
                        Box::new(warp::reply::json(&resp)) as Box<dyn warp::Reply>
                    }
                }
            },
        )
}

async fn dispatch_jsonrpc<T: A2aGateway>(request: JsonRpcRequest, gw: Arc<T>) -> JsonRpcResponse {
    match request.method.as_str() {
        "message/send" => messaging::dispatch_send_message(&request, &*gw).await,
        "tasks/get" => tasks::dispatch_get_task(&request, &*gw).await,
        "tasks/cancel" => tasks::dispatch_cancel_task(&request, &*gw).await,
        "tasks/list" => tasks::dispatch_list_tasks(&request, &*gw).await,
        "agent/getAuthenticatedExtendedCard" => {
            discovery::dispatch_get_extended(&request, &*gw).await
        }
        "tasks/pushNotificationConfig/set" => push::dispatch_set_push(&request, &*gw).await,
        "tasks/pushNotificationConfig/get" => push::dispatch_get_push(&request, &*gw).await,
        "tasks/pushNotificationConfig/list" => push::dispatch_list_push(&request, &*gw).await,
        "tasks/pushNotificationConfig/delete" => push::dispatch_delete_push(&request, &*gw).await,
        _ => error_response(
            &request.id,
            -32601,
            format!("method not found: {}", request.method),
        ),
    }
}

// -- SSE streaming -----------------------------------------------------------

fn streaming_filter<T>(
    gateway: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: A2aGateway + 'static,
{
    warp::path("a2a")
        .and(warp::path("stream"))
        .and(warp::post())
        .and(warp::body::json::<JsonRpcRequest>())
        .and(warp::any().map(move || gateway.clone()))
        .then(
            |request: JsonRpcRequest, gateway: Option<Arc<T>>| async move {
                let Some(gw) = gateway else {
                    let resp = error_response(
                        &request.id,
                        -32000,
                        "A2A gateway not configured".to_string(),
                    );
                    return Box::new(warp::reply::with_status(
                        warp::reply::json(&resp),
                        warp::http::StatusCode::NOT_FOUND,
                    )) as Box<dyn warp::Reply>;
                };
                messaging::handle_streaming_jsonrpc(request, gw).await
            },
        )
}
