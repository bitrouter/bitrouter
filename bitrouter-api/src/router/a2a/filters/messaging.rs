//! Message send/stream handlers for A2A gateway.

use std::convert::Infallible;
use std::pin::Pin;

use bitrouter_core::api::a2a::gateway::A2aProxy;
use futures_core::Stream;
use tokio::time::Instant;
use tokio_stream::StreamExt;

use super::observe::{A2aObserveContext, emit_agent_failure, emit_agent_success};
use bitrouter_core::api::a2a::types::*;

/// Handle `message/send` JSON-RPC method.
pub(crate) async fn dispatch_send_message(
    request: &JsonRpcRequest,
    agent: &impl A2aProxy,
    agent_name: &str,
    ctx: &Option<A2aObserveContext>,
) -> JsonRpcResponse {
    let req: SendMessageRequest = match request.deserialize_params() {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let start = Instant::now();
    let result = agent.send_message(req).await;
    match &result {
        Ok(_) => emit_agent_success(ctx, agent_name, "message/send", start),
        Err(e) => emit_agent_failure(ctx, agent_name, "message/send", start, &e.to_string()),
    }
    match result {
        Ok(r) => JsonRpcResponse::success(&request.id, &r),
        Err(e) => JsonRpcResponse::gateway_error(&request.id, &e),
    }
}

/// Handle streaming JSON-RPC methods (`message/stream`, `tasks/resubscribe`).
pub(crate) async fn handle_streaming_jsonrpc(
    request: JsonRpcRequest,
    agent: &impl A2aProxy,
    agent_name: &str,
    ctx: &Option<A2aObserveContext>,
) -> Box<dyn warp::Reply> {
    match request.method.as_str() {
        "message/stream" => {
            let req: SendMessageRequest = match request.deserialize_params() {
                Ok(r) => r,
                Err(resp) => {
                    return Box::new(warp::reply::with_status(
                        warp::reply::json(&resp),
                        warp::http::StatusCode::BAD_REQUEST,
                    ));
                }
            };
            let start = Instant::now();
            match agent.send_streaming_message(req).await {
                Ok(stream) => {
                    let request_id = request.id.clone();
                    let event_stream = sync_bridge_with_observe(
                        stream,
                        agent_name.to_string(),
                        "message/stream".to_string(),
                        start,
                        ctx.clone(),
                    )
                    .map(move |item| stream_response_to_sse(&request_id, &item));
                    Box::new(warp::sse::reply(event_stream))
                }
                Err(ref e) => {
                    emit_agent_failure(ctx, agent_name, "message/stream", start, &e.to_string());
                    let resp = JsonRpcResponse::gateway_error(&request.id, e);
                    Box::new(warp::reply::with_status(
                        warp::reply::json(&resp),
                        warp::http::StatusCode::INTERNAL_SERVER_ERROR,
                    ))
                }
            }
        }
        "tasks/resubscribe" => {
            let req: SubscribeToTaskRequest = match request.deserialize_params() {
                Ok(r) => r,
                Err(resp) => {
                    return Box::new(warp::reply::with_status(
                        warp::reply::json(&resp),
                        warp::http::StatusCode::BAD_REQUEST,
                    ));
                }
            };
            let start = Instant::now();
            match agent.subscribe_to_task(&req.task_id).await {
                Ok(stream) => {
                    let request_id = request.id.clone();
                    let event_stream = sync_bridge_with_observe(
                        stream,
                        agent_name.to_string(),
                        "tasks/resubscribe".to_string(),
                        start,
                        ctx.clone(),
                    )
                    .map(move |item| stream_response_to_sse(&request_id, &item));
                    Box::new(warp::sse::reply(event_stream))
                }
                Err(ref e) => {
                    emit_agent_failure(ctx, agent_name, "tasks/resubscribe", start, &e.to_string());
                    let resp = JsonRpcResponse::gateway_error(&request.id, e);
                    Box::new(warp::reply::with_status(
                        warp::reply::json(&resp),
                        warp::http::StatusCode::INTERNAL_SERVER_ERROR,
                    ))
                }
            }
        }
        _ => {
            let resp = JsonRpcResponse::error(
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

/// Bridge a `Send`-only stream into a `Send + Sync` stream via a channel,
/// emitting an observation event after the stream
/// completes or the client disconnects.
pub(crate) fn sync_bridge_with_observe(
    source: Pin<Box<dyn Stream<Item = StreamResponse> + Send>>,
    agent_name: String,
    method: String,
    start: Instant,
    ctx: Option<A2aObserveContext>,
) -> tokio_stream::wrappers::ReceiverStream<StreamResponse> {
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    tokio::spawn(async move {
        tokio::pin!(source);
        let mut client_disconnected = false;
        while let Some(item) = source.next().await {
            if tx.send(item).await.is_err() {
                client_disconnected = true;
                break;
            }
        }
        if client_disconnected {
            emit_agent_failure(
                &ctx,
                &agent_name,
                &method,
                start,
                "client disconnected during stream",
            );
        } else {
            emit_agent_success(&ctx, &agent_name, &method, start);
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
