//! SSE streaming endpoint for A2A v1.0.
//!
//! Provides `POST /a2a/stream` for `SendStreamingMessage` and `SubscribeToTask`.

pub(crate) mod handler;

use std::sync::Arc;

use warp::Filter;

use bitrouter_a2a::jsonrpc::JsonRpcRequest;
use bitrouter_a2a::server::{AgentExecutor, TaskStore};

/// Creates a warp filter for SSE streaming `POST /a2a/stream` for
/// `SendStreamingMessage` and `SubscribeToTask`.
pub fn streaming_jsonrpc_filter<E, S>(
    executor: Arc<E>,
    task_store: Arc<S>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    E: AgentExecutor + 'static,
    S: TaskStore + 'static,
{
    warp::path("a2a")
        .and(warp::path("stream"))
        .and(warp::post())
        .and(warp::body::json::<JsonRpcRequest>())
        .and(warp::any().map(move || executor.clone()))
        .and(warp::any().map(move || task_store.clone()))
        .then(
            |request: JsonRpcRequest, executor: Arc<E>, task_store: Arc<S>| async move {
                handler::handle_streaming_request(request, executor, task_store).await
            },
        )
}
