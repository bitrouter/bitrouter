//! Warp filters for OpenAI Chat Completions compatible endpoints.

use std::sync::Arc;

use bitrouter_core::{
    models::language::language_model::LanguageModel,
    routers::{model_router::LanguageModelRouter, routing_table::RoutingTable},
};
use warp::Filter;

use crate::error::{BadRequest, BitrouterRejection};

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
        .and_then(handle_chat_completion)
}

async fn handle_chat_completion<T, R>(
    request: ChatCompletionRequest,
    table: Arc<T>,
    router: Arc<R>,
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

    // Get the concrete model instance from the router.
    let model = router
        .route_model(target)
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

    let model_id = model.model_id().to_owned();
    let options = convert::to_call_options(request);

    if is_stream {
        handle_stream(&model, &model_id, options).await
    } else {
        handle_generate(&model, &model_id, options).await
    }
}

async fn handle_generate(
    model: &(impl LanguageModel + ?Sized),
    model_id: &str,
    options: bitrouter_core::models::language::call_options::LanguageModelCallOptions,
) -> Result<Box<dyn warp::Reply>, warp::Rejection> {
    let result = model
        .generate(options)
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

    let response = convert::from_generate_result(model_id, result);
    Ok(Box::new(warp::reply::json(&response)))
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

fn generate_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:x}")
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
