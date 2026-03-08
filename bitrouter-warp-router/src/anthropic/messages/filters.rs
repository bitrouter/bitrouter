//! Warp filters for Anthropic Messages compatible endpoints.

use std::sync::Arc;

use bitrouter_core::{
    models::language::language_model::LanguageModel,
    routers::{model_router::LanguageModelRouter, routing_table::RoutingTable},
};
use warp::Filter;

use crate::error::BitrouterRejection;

use super::{convert, types::MessagesRequest};

/// Creates a warp filter for the `/v1/messages` endpoint.
///
/// The filter accepts POST requests with a JSON body in Anthropic Messages format,
/// routes to the appropriate language model, and returns the response in the same format.
///
/// Both non-streaming and streaming (SSE) modes are supported.
pub fn messages_filter<T, R>(
    table: Arc<T>,
    router: Arc<R>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    warp::path!("v1" / "messages")
        .and(warp::post())
        .and(warp::body::json::<MessagesRequest>())
        .and(warp::any().map(move || table.clone()))
        .and(warp::any().map(move || router.clone()))
        .and_then(handle_messages)
}

async fn handle_messages<T, R>(
    request: MessagesRequest,
    table: Arc<T>,
    router: Arc<R>,
) -> Result<Box<dyn warp::Reply>, warp::Rejection>
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    let is_stream = request.stream.unwrap_or(false);
    let incoming_model = convert::extract_model_name(&request).to_owned();

    let target = table
        .route(&incoming_model)
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

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

    let model_id = model_id.to_owned();

    let (tx, rx) =
        tokio::sync::mpsc::channel::<Result<warp::sse::Event, std::convert::Infallible>>(32);

    tokio::spawn(async move {
        let mut stream = stream_result.stream;
        use tokio_stream::StreamExt as _;
        while let Some(part) = stream.next().await {
            if let Some(event) = convert::stream_part_to_event(&model_id, &part) {
                let data = serde_json::to_string(&event).unwrap_or_default();
                let sse = Ok(warp::sse::Event::default()
                    .event(&event.event_type)
                    .data(data));
                if tx.send(sse).await.is_err() {
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
