//! Warp filter assembly and JSON-RPC dispatch for A2A gateway proxy.
//!
//! All routes are scoped under `/a2a/{agent_name}/...` so that each
//! upstream agent is a fully independent A2A endpoint.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;

use bitrouter_core::api::a2a::gateway::{A2aGateway, A2aProxy};
use bitrouter_core::api::a2a::types::A2aGatewayError;
use bitrouter_core::api::a2a::types::*;
use bitrouter_core::observe::{
    CallerContext, ToolCallFailureEvent, ToolCallSuccessEvent, ToolObserveCallback,
    ToolRequestContext,
};
use futures_core::Stream;
use tokio::time::Instant;
use tokio_stream::StreamExt;
use warp::Filter;

// ── Public entry point ──────────────────────────────────────────────

/// Combined A2A gateway filter: per-agent discovery + JSON-RPC + streaming.
///
/// Routes are prefixed with `/a2a/{agent_name}` so each upstream agent is
/// addressed independently.
///
/// When `observer` and `account_filter` are provided, every handler fires
/// observation events through the observer with caller context.
pub fn a2a_gateway_filter<G, A>(
    registry: Option<Arc<G>>,
    observer: Option<Arc<dyn ToolObserveCallback>>,
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

    well_known_filter(registry.clone(), discovery_ctx.clone())
        .or(jsonrpc_filter(
            registry.clone(),
            observer.clone(),
            account_filter.clone(),
        ))
        .or(streaming_filter(registry, observer, account_filter))
}

// ── Agent lookup helper ─────────────────────────────────────────────

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
    observer: &Option<Arc<dyn ToolObserveCallback>>,
    caller: CallerContext,
) -> Option<A2aObserveContext> {
    observer.as_ref().map(|obs| A2aObserveContext {
        observer: obs.clone(),
        caller,
    })
}

// ── JSON-RPC dispatch ───────────────────────────────────────────────

fn jsonrpc_filter<G: A2aGateway + 'static, A>(
    registry: Option<Arc<G>>,
    observer: Option<Arc<dyn ToolObserveCallback>>,
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
             observer: Option<Arc<dyn ToolObserveCallback>>,
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
                        handle_streaming_jsonrpc(request, agent, &agent_name, &ctx).await
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
        "message/send" => {
            dispatch_observed(&request, agent_name, "message/send", ctx, |req| {
                agent.send_message(req)
            })
            .await
        }
        "tasks/get" => {
            dispatch_observed(&request, agent_name, "tasks/get", ctx, |req| {
                agent.get_task(req)
            })
            .await
        }
        "tasks/cancel" => {
            dispatch_observed(&request, agent_name, "tasks/cancel", ctx, |req| {
                agent.cancel_task(req)
            })
            .await
        }
        "tasks/list" => {
            dispatch_observed(&request, agent_name, "tasks/list", ctx, |req| {
                agent.list_tasks(req)
            })
            .await
        }
        "agent/getAuthenticatedExtendedCard" => {
            dispatch_observed_no_params(
                &request,
                agent_name,
                "agent/getAuthenticatedExtendedCard",
                ctx,
                || agent.get_extended_agent_card(),
            )
            .await
        }
        "tasks/pushNotificationConfig/set" => {
            dispatch_observed(
                &request,
                agent_name,
                "tasks/pushNotificationConfig/set",
                ctx,
                |config| agent.set_push_config(config),
            )
            .await
        }
        "tasks/pushNotificationConfig/get" => {
            dispatch_observed(
                &request,
                agent_name,
                "tasks/pushNotificationConfig/get",
                ctx,
                |req: GetTaskPushNotificationConfigRequest| async move {
                    agent
                        .get_push_config(&req.id, req.push_notification_config_id.as_deref())
                        .await
                },
            )
            .await
        }
        "tasks/pushNotificationConfig/list" => {
            dispatch_observed(
                &request,
                agent_name,
                "tasks/pushNotificationConfig/list",
                ctx,
                |req: ListTaskPushNotificationConfigsRequest| async move {
                    agent.list_push_configs(&req.id).await
                },
            )
            .await
        }
        "tasks/pushNotificationConfig/delete" => {
            dispatch_observed(
                &request,
                agent_name,
                "tasks/pushNotificationConfig/delete",
                ctx,
                |req: DeleteTaskPushNotificationConfigRequest| async move {
                    agent
                        .delete_push_config(&req.id, &req.push_notification_config_id)
                        .await
                        .map(|()| serde_json::json!({"success": true}))
                },
            )
            .await
        }
        _ => JsonRpcResponse::error(
            &request.id,
            -32601,
            format!("method not found: {}", request.method),
        ),
    }
}

// ── SSE streaming ───────────────────────────────────────────────────

fn streaming_filter<G: A2aGateway + 'static, A>(
    registry: Option<Arc<G>>,
    observer: Option<Arc<dyn ToolObserveCallback>>,
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
             observer: Option<Arc<dyn ToolObserveCallback>>,
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
                handle_streaming_jsonrpc(request, agent, &agent_name, &ctx).await
            },
        )
}

// ── Discovery handlers ──────────────────────────────────────────────

/// Warp filter for `GET /a2a/{agent_name}/.well-known/agent-card.json`.
fn well_known_filter<G: A2aGateway + 'static>(
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

/// Generic dispatch helper: deserialize params, time the call, emit observation,
/// and convert the result to a JSON-RPC response.
async fn dispatch_observed<P, R, F, Fut>(
    request: &JsonRpcRequest,
    agent_name: &str,
    method: &str,
    ctx: &Option<A2aObserveContext>,
    call: F,
) -> JsonRpcResponse
where
    P: serde::de::DeserializeOwned,
    R: serde::Serialize,
    F: FnOnce(P) -> Fut,
    Fut: std::future::Future<Output = Result<R, A2aGatewayError>>,
{
    let params: P = match request.deserialize_params() {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let start = Instant::now();
    let result = call(params).await;
    match &result {
        Ok(_) => emit_agent_success(ctx, agent_name, method, start),
        Err(e) => emit_agent_failure(ctx, agent_name, method, start, &e.to_string()),
    }
    match result {
        Ok(r) => JsonRpcResponse::success(&request.id, &r),
        Err(e) => JsonRpcResponse::gateway_error(&request.id, &e),
    }
}

/// Variant for methods that take no params (e.g. `agent/getAuthenticatedExtendedCard`).
async fn dispatch_observed_no_params<R, F, Fut>(
    request: &JsonRpcRequest,
    agent_name: &str,
    method: &str,
    ctx: &Option<A2aObserveContext>,
    call: F,
) -> JsonRpcResponse
where
    R: serde::Serialize,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<R, A2aGatewayError>>,
{
    let start = Instant::now();
    let result = call().await;
    match &result {
        Ok(_) => emit_agent_success(ctx, agent_name, method, start),
        Err(e) => emit_agent_failure(ctx, agent_name, method, start, &e.to_string()),
    }
    match result {
        Ok(r) => JsonRpcResponse::success(&request.id, &r),
        Err(e) => JsonRpcResponse::gateway_error(&request.id, &e),
    }
}

/// Handle streaming JSON-RPC methods (`message/stream`, `tasks/resubscribe`).
async fn handle_streaming_jsonrpc(
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
fn sync_bridge_with_observe(
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

fn stream_response_to_sse(
    request_id: &str,
    item: &StreamResponse,
) -> Result<warp::sse::Event, Infallible> {
    let result = match serde_json::to_value(item) {
        Ok(v) => v,
        Err(e) => {
            let error_data = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32603, "message": format!("serialization error: {e}")}
            });
            let data = serde_json::to_string(&error_data).unwrap_or_else(|_| {
                format!(r#"{{"jsonrpc":"2.0","id":"{}","error":{{"code":-32603,"message":"serialization error"}}}}"#, request_id)
            });
            return Ok(warp::sse::Event::default().data(data));
        }
    };
    let envelope = serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": result
    });
    let data = match serde_json::to_string(&envelope) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize SSE envelope");
            return Ok(warp::sse::Event::default().data(
                format!(r#"{{"jsonrpc":"2.0","id":"{}","error":{{"code":-32603,"message":"serialization error"}}}}"#, request_id)
            ));
        }
    };
    Ok(warp::sse::Event::default().data(data))
}

// ── Observation helpers ─────────────────────────────────────────────

/// Shared context threaded through A2A gateway filters for observation.
#[derive(Clone)]
struct A2aObserveContext {
    observer: Arc<dyn ToolObserveCallback>,
    caller: CallerContext,
}

/// Fire a success [`ToolCallSuccessEvent`] for a completed A2A operation.
///
/// The event is spawned as an async task so it never blocks the response path.
fn emit_agent_success(
    ctx: &Option<A2aObserveContext>,
    agent_name: &str,
    method: &str,
    start: Instant,
) {
    let Some(ctx) = ctx else { return };
    let event = ToolCallSuccessEvent {
        ctx: ToolRequestContext {
            provider: agent_name.to_string(),
            operation: method.to_string(),
            caller: ctx.caller.clone(),
            latency_ms: start.elapsed().as_millis() as u64,
        },
    };
    let obs = ctx.observer.clone();
    tokio::spawn(async move { obs.on_tool_call_success(event).await });
}

/// Fire a failure [`ToolCallFailureEvent`] from an error description.
fn emit_agent_failure(
    ctx: &Option<A2aObserveContext>,
    agent_name: &str,
    method: &str,
    start: Instant,
    error: &str,
) {
    let Some(ctx) = ctx else { return };
    let event = ToolCallFailureEvent {
        ctx: ToolRequestContext {
            provider: agent_name.to_string(),
            operation: method.to_string(),
            caller: ctx.caller.clone(),
            latency_ms: start.elapsed().as_millis() as u64,
        },
        error: error.to_string(),
    };
    let obs = ctx.observer.clone();
    tokio::spawn(async move { obs.on_tool_call_failure(event).await });
}
