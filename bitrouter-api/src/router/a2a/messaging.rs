//! Message send/stream handlers for A2A gateway.

use std::convert::Infallible;
use std::pin::Pin;

use bitrouter_a2a::client::upstream::UpstreamA2aAgent;
use bitrouter_a2a::server::A2aProxy;
use futures_core::Stream;
use tokio_stream::StreamExt;

use super::convert::{
    WithId, deserialize_params, error_response, gateway_error_response, success_response,
};
use super::types::*;

/// Handle `message/send` JSON-RPC method.
pub(crate) async fn dispatch_send_message(
    request: &JsonRpcRequest,
    agent: &UpstreamA2aAgent,
) -> JsonRpcResponse {
    let req: SendMessageRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    match agent.send_message(req).await {
        Ok(result) => success_response(&request.id, &result),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

/// Handle streaming JSON-RPC methods (`message/stream`, `tasks/resubscribe`).
pub(crate) async fn handle_streaming_jsonrpc(
    request: JsonRpcRequest,
    agent: &UpstreamA2aAgent,
) -> Box<dyn warp::Reply> {
    match request.method.as_str() {
        "message/stream" => {
            let req: SendMessageRequest = match deserialize_params(&request.params) {
                Ok(r) => r,
                Err(resp) => {
                    let resp = (*resp).with_id(&request.id);
                    return Box::new(warp::reply::with_status(
                        warp::reply::json(&resp),
                        warp::http::StatusCode::BAD_REQUEST,
                    ));
                }
            };
            match agent.send_streaming_message(req).await {
                Ok(stream) => {
                    let request_id = request.id.clone();
                    let event_stream = sync_bridge(stream)
                        .map(move |item| stream_response_to_sse(&request_id, &item));
                    Box::new(warp::sse::reply(event_stream))
                }
                Err(e) => {
                    let resp = gateway_error_response(&request.id, &e);
                    Box::new(warp::reply::with_status(
                        warp::reply::json(&resp),
                        warp::http::StatusCode::INTERNAL_SERVER_ERROR,
                    ))
                }
            }
        }
        "tasks/resubscribe" => {
            let req: SubscribeToTaskRequest = match deserialize_params(&request.params) {
                Ok(r) => r,
                Err(resp) => {
                    let resp = (*resp).with_id(&request.id);
                    return Box::new(warp::reply::with_status(
                        warp::reply::json(&resp),
                        warp::http::StatusCode::BAD_REQUEST,
                    ));
                }
            };
            match agent.subscribe_to_task(&req.task_id).await {
                Ok(stream) => {
                    let request_id = request.id.clone();
                    let event_stream = sync_bridge(stream)
                        .map(move |item| stream_response_to_sse(&request_id, &item));
                    Box::new(warp::sse::reply(event_stream))
                }
                Err(e) => {
                    let resp = gateway_error_response(&request.id, &e);
                    Box::new(warp::reply::with_status(
                        warp::reply::json(&resp),
                        warp::http::StatusCode::INTERNAL_SERVER_ERROR,
                    ))
                }
            }
        }
        _ => {
            let resp = error_response(
                &request.id,
                -32601,
                format!("method not found: {}", request.method),
            );
            Box::new(warp::reply::with_status(
                warp::reply::json(&resp),
                warp::http::StatusCode::BAD_REQUEST,
            ))
        }
    }
}

/// Bridge a `Send`-only stream into a `Send + Sync` stream via a channel.
///
/// `warp::sse::reply` requires `Send + Sync` but our trait returns
/// `Pin<Box<dyn Stream + Send>>`. This spawns a forwarding task.
pub(crate) fn sync_bridge(
    source: Pin<Box<dyn Stream<Item = StreamResponse> + Send>>,
) -> tokio_stream::wrappers::ReceiverStream<StreamResponse> {
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    tokio::spawn(async move {
        tokio::pin!(source);
        while let Some(item) = source.next().await {
            if tx.send(item).await.is_err() {
                break;
            }
        }
    });
    tokio_stream::wrappers::ReceiverStream::new(rx)
}

pub(crate) fn stream_response_to_sse(
    request_id: &str,
    item: &StreamResponse,
) -> Result<warp::sse::Event, Infallible> {
    let result = serde_json::to_value(item).unwrap_or_default();
    let envelope = serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": result
    });
    let data = serde_json::to_string(&envelope).unwrap_or_default();
    Ok(warp::sse::Event::default().data(data))
}
