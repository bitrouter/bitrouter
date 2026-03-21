//! Warp filter definitions for A2A gateway proxy.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;

use futures_core::Stream;
use tokio_stream::StreamExt;
use warp::Filter;

use super::convert::{
    WithId, deserialize_params, error_response, gateway_error_response, success_response,
};
use super::types::*;

/// Combined A2A gateway filter: discovery + JSON-RPC + streaming + REST.
pub fn a2a_gateway_filter<T>(
    gateway: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: A2aGateway + 'static,
{
    well_known_filter(gateway.clone())
        .or(jsonrpc_filter(gateway.clone()))
        .or(streaming_filter(gateway.clone()))
        .or(rest_send_filter(gateway.clone()))
        .or(rest_get_task_filter(gateway.clone()))
        .or(rest_cancel_filter(gateway.clone()))
        .or(rest_push_create_filter(gateway.clone()))
        .or(rest_push_get_filter(gateway.clone()))
        .or(rest_push_list_filter(gateway.clone()))
        .or(rest_push_delete_filter(gateway))
}

// -- Discovery ---------------------------------------------------------------

fn well_known_filter<T>(
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
                    "SendStreamingMessage" | "SubscribeToTask" => {
                        handle_streaming_jsonrpc(request, gw).await
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
        "SendMessage" => dispatch_send_message(&request, &*gw).await,
        "GetTask" => dispatch_get_task(&request, &*gw).await,
        "CancelTask" => dispatch_cancel_task(&request, &*gw).await,
        "ListTasks" => dispatch_list_tasks(&request, &*gw).await,
        "GetExtendedAgentCard" => dispatch_get_extended(&request, &*gw).await,
        "CreateTaskPushNotificationConfig" => dispatch_create_push(&request, &*gw).await,
        "GetTaskPushNotificationConfig" => dispatch_get_push(&request, &*gw).await,
        "ListTaskPushNotificationConfigs" => dispatch_list_push(&request, &*gw).await,
        "DeleteTaskPushNotificationConfig" => dispatch_delete_push(&request, &*gw).await,
        _ => error_response(
            &request.id,
            -32601,
            format!("method not found: {}", request.method),
        ),
    }
}

async fn dispatch_send_message<T: A2aProxy>(request: &JsonRpcRequest, gw: &T) -> JsonRpcResponse {
    let req: SendMessageRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    match gw.send_message(req).await {
        Ok(result) => success_response(&request.id, &result),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

async fn dispatch_get_task<T: A2aProxy>(request: &JsonRpcRequest, gw: &T) -> JsonRpcResponse {
    let req: GetTaskRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    match gw.get_task(req).await {
        Ok(task) => success_response(&request.id, &task),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

async fn dispatch_cancel_task<T: A2aProxy>(request: &JsonRpcRequest, gw: &T) -> JsonRpcResponse {
    let req: CancelTaskRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    match gw.cancel_task(req).await {
        Ok(task) => success_response(&request.id, &task),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

async fn dispatch_list_tasks<T: A2aProxy>(request: &JsonRpcRequest, gw: &T) -> JsonRpcResponse {
    let req: ListTasksRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    match gw.list_tasks(req).await {
        Ok(resp) => success_response(&request.id, &resp),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

async fn dispatch_get_extended<T: A2aProxy>(request: &JsonRpcRequest, gw: &T) -> JsonRpcResponse {
    match gw.get_extended_agent_card().await {
        Ok(card) => success_response(&request.id, &card),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

async fn dispatch_create_push<T: A2aProxy>(request: &JsonRpcRequest, gw: &T) -> JsonRpcResponse {
    let config: TaskPushNotificationConfig = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    match gw.create_push_config(config).await {
        Ok(stored) => success_response(&request.id, &stored),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

async fn dispatch_get_push<T: A2aProxy>(request: &JsonRpcRequest, gw: &T) -> JsonRpcResponse {
    let req: GetTaskPushNotificationConfigRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    match gw.get_push_config(&req.task_id, &req.id).await {
        Ok(config) => success_response(&request.id, &config),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

async fn dispatch_list_push<T: A2aProxy>(request: &JsonRpcRequest, gw: &T) -> JsonRpcResponse {
    let req: ListTaskPushNotificationConfigsRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    match gw.list_push_configs(&req.task_id).await {
        Ok(resp) => success_response(&request.id, &resp),
        Err(e) => gateway_error_response(&request.id, &e),
    }
}

async fn dispatch_delete_push<T: A2aProxy>(request: &JsonRpcRequest, gw: &T) -> JsonRpcResponse {
    let req: DeleteTaskPushNotificationConfigRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };
    match gw.delete_push_config(&req.task_id, &req.id).await {
        Ok(()) => success_response(&request.id, &serde_json::json!({"success": true})),
        Err(e) => gateway_error_response(&request.id, &e),
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
                handle_streaming_jsonrpc(request, gw).await
            },
        )
}

async fn handle_streaming_jsonrpc<T: A2aGateway>(
    request: JsonRpcRequest,
    gw: Arc<T>,
) -> Box<dyn warp::Reply> {
    match request.method.as_str() {
        "SendStreamingMessage" => {
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
            match gw.send_streaming_message(req).await {
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
        "SubscribeToTask" => {
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
            match gw.subscribe_to_task(&req.task_id).await {
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
fn sync_bridge(
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

fn stream_response_to_sse(
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

// -- REST bindings -----------------------------------------------------------

fn error_reply(status: warp::http::StatusCode, message: &str) -> Box<dyn warp::Reply> {
    Box::new(warp::reply::with_status(
        warp::reply::json(&serde_json::json!({"error": message})),
        status,
    ))
}

fn gateway_error_reply(err: &A2aGatewayError) -> Box<dyn warp::Reply> {
    let status = match err {
        A2aGatewayError::AgentNotFound { .. } => warp::http::StatusCode::NOT_FOUND,
        A2aGatewayError::InvalidConfig { .. } => warp::http::StatusCode::BAD_REQUEST,
        _ => warp::http::StatusCode::BAD_GATEWAY,
    };
    error_reply(status, &err.to_string())
}

fn rest_send_filter<T>(
    gateway: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: A2aGateway + 'static,
{
    warp::path!("message:send")
        .and(warp::post())
        .and(warp::body::json::<SendMessageRequest>())
        .and(warp::any().map(move || gateway.clone()))
        .then(
            |body: SendMessageRequest, gateway: Option<Arc<T>>| async move {
                let Some(gw) = gateway else {
                    return error_reply(
                        warp::http::StatusCode::NOT_FOUND,
                        "A2A gateway not configured",
                    );
                };
                match gw.send_message(body).await {
                    Ok(result) => Box::new(warp::reply::json(&result)) as Box<dyn warp::Reply>,
                    Err(e) => gateway_error_reply(&e),
                }
            },
        )
}

fn rest_get_task_filter<T>(
    gateway: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: A2aGateway + 'static,
{
    warp::path!("tasks" / String)
        .and(warp::get())
        .and(warp::any().map(move || gateway.clone()))
        .then(|task_id: String, gateway: Option<Arc<T>>| async move {
            let Some(gw) = gateway else {
                return error_reply(
                    warp::http::StatusCode::NOT_FOUND,
                    "A2A gateway not configured",
                );
            };
            let req = GetTaskRequest {
                id: task_id,
                history_length: None,
                tenant: None,
            };
            match gw.get_task(req).await {
                Ok(task) => Box::new(warp::reply::json(&task)) as Box<dyn warp::Reply>,
                Err(e) => gateway_error_reply(&e),
            }
        })
}

fn rest_cancel_filter<T>(
    gateway: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: A2aGateway + 'static,
{
    warp::path("tasks")
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::any().map(move || gateway.clone()))
        .and_then(
            |task_id_action: String, gateway: Option<Arc<T>>| async move {
                let Some(task_id) = task_id_action.strip_suffix(":cancel") else {
                    return Err(warp::reject::not_found());
                };
                let Some(gw) = gateway else {
                    return Ok(error_reply(
                        warp::http::StatusCode::NOT_FOUND,
                        "A2A gateway not configured",
                    ));
                };
                let req = CancelTaskRequest {
                    id: task_id.to_string(),
                    tenant: None,
                };
                let reply: Box<dyn warp::Reply> = match gw.cancel_task(req).await {
                    Ok(task) => Box::new(warp::reply::json(&task)),
                    Err(e) => gateway_error_reply(&e),
                };
                Ok(reply)
            },
        )
}

fn rest_push_create_filter<T>(
    gateway: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: A2aGateway + 'static,
{
    warp::path!("tasks" / String / "push-notification-configs")
        .and(warp::post())
        .and(warp::body::json::<TaskPushNotificationConfig>())
        .and(warp::any().map(move || gateway.clone()))
        .then(
            |_task_id: String,
             config: TaskPushNotificationConfig,
             gateway: Option<Arc<T>>| async move {
                let Some(gw) = gateway else {
                    return error_reply(
                        warp::http::StatusCode::NOT_FOUND,
                        "A2A gateway not configured",
                    );
                };
                match gw.create_push_config(config).await {
                    Ok(stored) => Box::new(warp::reply::with_status(
                        warp::reply::json(&stored),
                        warp::http::StatusCode::CREATED,
                    )) as Box<dyn warp::Reply>,
                    Err(e) => gateway_error_reply(&e),
                }
            },
        )
}

fn rest_push_get_filter<T>(
    gateway: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: A2aGateway + 'static,
{
    warp::path!("tasks" / String / "push-notification-configs" / String)
        .and(warp::get())
        .and(warp::any().map(move || gateway.clone()))
        .then(
            |task_id: String, config_id: String, gateway: Option<Arc<T>>| async move {
                let Some(gw) = gateway else {
                    return error_reply(
                        warp::http::StatusCode::NOT_FOUND,
                        "A2A gateway not configured",
                    );
                };
                match gw.get_push_config(&task_id, &config_id).await {
                    Ok(config) => Box::new(warp::reply::json(&config)) as Box<dyn warp::Reply>,
                    Err(e) => gateway_error_reply(&e),
                }
            },
        )
}

fn rest_push_list_filter<T>(
    gateway: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: A2aGateway + 'static,
{
    warp::path!("tasks" / String / "push-notification-configs")
        .and(warp::get())
        .and(warp::any().map(move || gateway.clone()))
        .then(|task_id: String, gateway: Option<Arc<T>>| async move {
            let Some(gw) = gateway else {
                return error_reply(
                    warp::http::StatusCode::NOT_FOUND,
                    "A2A gateway not configured",
                );
            };
            match gw.list_push_configs(&task_id).await {
                Ok(resp) => Box::new(warp::reply::json(&resp)) as Box<dyn warp::Reply>,
                Err(e) => gateway_error_reply(&e),
            }
        })
}

fn rest_push_delete_filter<T>(
    gateway: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: A2aGateway + 'static,
{
    warp::path!("tasks" / String / "push-notification-configs" / String)
        .and(warp::delete())
        .and(warp::any().map(move || gateway.clone()))
        .then(
            |task_id: String, config_id: String, gateway: Option<Arc<T>>| async move {
                let Some(gw) = gateway else {
                    return error_reply(
                        warp::http::StatusCode::NOT_FOUND,
                        "A2A gateway not configured",
                    );
                };
                match gw.delete_push_config(&task_id, &config_id).await {
                    Ok(()) => Box::new(warp::reply::with_status(
                        warp::reply::json(&serde_json::json!({"success": true})),
                        warp::http::StatusCode::OK,
                    )) as Box<dyn warp::Reply>,
                    Err(e) => gateway_error_reply(&e),
                }
            },
        )
}
