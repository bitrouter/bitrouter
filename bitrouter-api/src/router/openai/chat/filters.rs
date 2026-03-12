//! Warp filters for OpenAI Chat Completions compatible endpoints.

use std::sync::Arc;
use std::time::Instant;

use bitrouter_core::{
    hooks::{GenerationHook, HookedRouter},
    models::language::language_model::LanguageModel,
    routers::{model_router::LanguageModelRouter, routing_table::RoutingTable},
};
use warp::Filter;

use crate::error::{BadRequest, BitrouterRejection};
use crate::metrics::{MetricsStore, format_endpoint};
use crate::util::generate_id;

use super::{convert, types::ChatCompletionRequest};

/// Creates a warp filter for the `/v1/chat/completions` endpoint.
///
/// The filter accepts POST requests with a JSON body in OpenAI Chat Completions format,
/// routes to the appropriate language model, and returns the response in the same format.
///
/// Both non-streaming and streaming (SSE) modes are supported.
pub fn chat_completions_filter<T, R>(
    table: Arc<T>,
    router: Arc<R>,
    metrics: Arc<MetricsStore>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    warp::path!("v1" / "chat" / "completions")
        .and(warp::post())
        .and(warp::body::json::<ChatCompletionRequest>())
        .and(warp::any().map(move || table.clone()))
        .and(warp::any().map(move || router.clone()))
        .and(warp::any().map(move || metrics.clone()))
        .and_then(handle_chat_completion)
}

/// Like [`chat_completions_filter`], but accepts a per-request hooks filter.
///
/// The `hooks_filter` runs on every incoming request and produces a
/// [`GenerationHook`] slice that is attached to the model for that single
/// request via [`HookedRouter`]. This allows callers to inject per-request
/// context (e.g. billing, auditing) into the generation lifecycle without
/// modifying the shared router.
///
/// When the hooks filter produces an empty slice the wrapper is zero-cost.
pub fn chat_completions_filter_with_hooks<T, R, H>(
    table: Arc<T>,
    router: Arc<R>,
    metrics: Arc<MetricsStore>,
    hooks_filter: H,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
    H: Filter<Extract = (Arc<[Arc<dyn GenerationHook>]>,), Error = warp::Rejection>
        + Clone
        + Send
        + Sync
        + 'static,
{
    warp::path!("v1" / "chat" / "completions")
        .and(warp::post())
        .and(warp::body::json::<ChatCompletionRequest>())
        .and(warp::any().map(move || table.clone()))
        .and(warp::any().map(move || router.clone()))
        .and(warp::any().map(move || metrics.clone()))
        .and(hooks_filter)
        .and_then(handle_chat_completion_with_hooks)
}

async fn handle_chat_completion<T, R>(
    request: ChatCompletionRequest,
    table: Arc<T>,
    router: Arc<R>,
    metrics: Arc<MetricsStore>,
) -> Result<Box<dyn warp::Reply>, warp::Rejection>
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    let is_stream = request.stream.unwrap_or(false);
    let incoming_model = convert::extract_model_name(&request).to_owned();

    // Route the incoming model name to a target provider + model.
    let target = table
        .route(&incoming_model)
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

    let endpoint = format_endpoint(&target.provider_name, &target.model_id);

    // Get the concrete model instance from the router.
    let model = router
        .route_model(target)
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

    let model_id = model.model_id().to_owned();
    let options = convert::to_call_options(request);

    let start = Instant::now();

    if is_stream {
        let result = handle_stream(&model, &model_id, options).await;
        metrics.record_outcome(incoming_model, endpoint, start, result.is_err());
        result
    } else {
        let gen_result = model.generate(options).await;
        match gen_result {
            Ok(result) => {
                let input_tokens = result.usage.input_tokens.total;
                let output_tokens = result.usage.output_tokens.total;
                metrics.record_success(
                    incoming_model,
                    endpoint,
                    start,
                    input_tokens,
                    output_tokens,
                );
                let response = convert::from_generate_result(&model_id, result);
                Ok(Box::new(warp::reply::json(&response)) as Box<dyn warp::Reply>)
            }
            Err(e) => {
                metrics.record_outcome(incoming_model, endpoint, start, true);
                Err(warp::reject::custom(BitrouterRejection(e)))
            }
        }
    }
}

async fn handle_chat_completion_with_hooks<T, R>(
    request: ChatCompletionRequest,
    table: Arc<T>,
    router: Arc<R>,
    metrics: Arc<MetricsStore>,
    hooks: Arc<[Arc<dyn GenerationHook>]>,
) -> Result<Box<dyn warp::Reply>, warp::Rejection>
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    let hooked = Arc::new(HookedRouter::new(router, hooks));
    handle_chat_completion(request, table, hooked, metrics).await
}

async fn handle_stream(
    model: &(impl LanguageModel + ?Sized),
    model_id: &str,
    options: bitrouter_core::models::language::call_options::LanguageModelCallOptions,
) -> Result<Box<dyn warp::Reply>, warp::Rejection> {
    let stream_result = model
        .stream(options)
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

    let stream_id = format!("chatcmpl-{}", generate_id());
    let model_id = model_id.to_owned();

    // Use a channel to convert the non-Sync model stream into a warp-compatible stream.
    let (tx, rx) =
        tokio::sync::mpsc::channel::<Result<warp::sse::Event, std::convert::Infallible>>(32);

    tokio::spawn(async move {
        let mut stream = stream_result.stream;
        use tokio_stream::StreamExt as _;
        while let Some(part) = stream.next().await {
            if let Some(chunk) = convert::stream_part_to_chunk(&model_id, &stream_id, &part) {
                let data = serde_json::to_string(&chunk).unwrap_or_default();
                let event = Ok(warp::sse::Event::default().data(data));
                if tx.send(event).await.is_err() {
                    break;
                }
            }
        }
        let _ = tx
            .send(Ok(warp::sse::Event::default().data("[DONE]")))
            .await;
    });

    let sse_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    Ok(Box::new(warp::sse::reply(sse_stream)))
}

/// Creates a rejection handler that converts [`BitrouterRejection`] and [`BadRequest`]
/// into appropriate HTTP error responses.
pub async fn rejection_handler(
    err: warp::Rejection,
) -> Result<impl warp::Reply, std::convert::Infallible> {
    let (code, message) = if let Some(e) = err.find::<BitrouterRejection>() {
        (warp::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    } else if let Some(e) = err.find::<BadRequest>() {
        (warp::http::StatusCode::BAD_REQUEST, e.to_string())
    } else if err.is_not_found() {
        (warp::http::StatusCode::NOT_FOUND, "not found".to_owned())
    } else {
        (
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            "internal server error".to_owned(),
        )
    };

    let json = warp::reply::json(&serde_json::json!({
        "error": {
            "message": message,
            "type": "server_error",
        }
    }));
    Ok(warp::reply::with_status(json, code))
}
