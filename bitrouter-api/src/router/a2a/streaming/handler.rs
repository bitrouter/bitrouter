//! Streaming request handlers for A2A SSE responses.

use std::convert::Infallible;
use std::sync::Arc;

use tokio_stream::StreamExt;

use bitrouter_a2a::jsonrpc::JsonRpcRequest;
use bitrouter_a2a::request::{SendMessageRequest, SubscribeToTaskRequest};
use bitrouter_a2a::server::{AgentExecutor, ExecutorContext, TaskStore};
use bitrouter_a2a::stream::StreamResponse;

use crate::router::a2a::jsonrpc::convert::generate_streaming_id;

pub(crate) async fn handle_streaming_request<E, S>(
    request: JsonRpcRequest,
    executor: Arc<E>,
    task_store: Arc<S>,
) -> Box<dyn warp::Reply>
where
    E: AgentExecutor + 'static,
    S: TaskStore + 'static,
{
    match request.method.as_str() {
        "SendStreamingMessage" => {
            handle_send_streaming_message(&request, executor, task_store).await
        }
        "SubscribeToTask" => handle_subscribe_to_task(&request, executor).await,
        _ => {
            let error = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request.id,
                "error": {"code": -32601, "message": format!("method not found: {}", request.method)}
            });
            Box::new(warp::reply::with_status(
                warp::reply::json(&error),
                warp::http::StatusCode::BAD_REQUEST,
            ))
        }
    }
}

async fn handle_send_streaming_message<E, S>(
    request: &JsonRpcRequest,
    executor: Arc<E>,
    task_store: Arc<S>,
) -> Box<dyn warp::Reply>
where
    E: AgentExecutor + 'static,
    S: TaskStore + 'static,
{
    let send_req: SendMessageRequest = match serde_json::from_value(request.params.clone()) {
        Ok(r) => r,
        Err(e) => {
            let error = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request.id,
                "error": {"code": -32602, "message": format!("invalid params: {e}")}
            });
            return Box::new(warp::reply::with_status(
                warp::reply::json(&error),
                warp::http::StatusCode::BAD_REQUEST,
            ));
        }
    };

    let task_id = send_req
        .message
        .task_id
        .clone()
        .unwrap_or_else(|| generate_streaming_id("task"));
    let context_id = send_req
        .message
        .context_id
        .clone()
        .unwrap_or_else(|| generate_streaming_id("ctx"));

    let ctx = ExecutorContext {
        message: send_req.message,
        task_id,
        context_id,
        configuration: send_req.configuration,
    };

    let request_id = request.id.clone();
    match executor.execute_streaming(&ctx).await {
        Ok(stream) => {
            let task_store = task_store.clone();
            let event_stream = stream.map(move |item| {
                // If the item is a completed task, store it best-effort.
                if let StreamResponse::Task(ref task) = item {
                    let _ = task_store.create(task);
                }
                stream_response_to_sse(&request_id, &item)
            });
            Box::new(warp::sse::reply(event_stream))
        }
        Err(e) => {
            let error = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request.id,
                "error": {"code": -32000, "message": e.to_string()}
            });
            Box::new(warp::reply::with_status(
                warp::reply::json(&error),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            ))
        }
    }
}

async fn handle_subscribe_to_task<E>(
    request: &JsonRpcRequest,
    executor: Arc<E>,
) -> Box<dyn warp::Reply>
where
    E: AgentExecutor + 'static,
{
    let req: SubscribeToTaskRequest = match serde_json::from_value(request.params.clone()) {
        Ok(r) => r,
        Err(e) => {
            let error = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request.id,
                "error": {"code": -32602, "message": format!("invalid params: {e}")}
            });
            return Box::new(warp::reply::with_status(
                warp::reply::json(&error),
                warp::http::StatusCode::BAD_REQUEST,
            ));
        }
    };

    let request_id = request.id.clone();
    match executor.subscribe(&req.task_id).await {
        Ok(stream) => {
            let event_stream = stream.map(move |item| stream_response_to_sse(&request_id, &item));
            Box::new(warp::sse::reply(event_stream))
        }
        Err(e) => {
            let error = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request.id,
                "error": {"code": -32000, "message": e.to_string()}
            });
            Box::new(warp::reply::with_status(
                warp::reply::json(&error),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            ))
        }
    }
}

fn stream_response_to_sse(
    request_id: &str,
    item: &StreamResponse,
) -> Result<warp::sse::Event, Infallible> {
    // Wrap each streaming item in a JSON-RPC 2.0 response envelope,
    // matching the A2A v1.0 wire format used by the reference Go impl.
    let result = serde_json::to_value(item).unwrap_or_default();
    let envelope = serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": result
    });
    let data = serde_json::to_string(&envelope).unwrap_or_default();
    Ok(warp::sse::Event::default().data(data))
}
