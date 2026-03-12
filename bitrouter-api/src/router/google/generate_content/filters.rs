//! Warp filters for Google Generative AI compatible endpoints.

use std::sync::Arc;
use std::time::Instant;

use bitrouter_core::{
    models::language::language_model::LanguageModel,
    routers::{model_router::LanguageModelRouter, routing_table::RoutingTable},
};
use warp::Filter;

use crate::error::BitrouterRejection;
use crate::metrics::{MetricsStore, RequestMetrics};

use super::{convert, types::GenerateContentRequest};

/// Creates a warp filter for the `/v1beta/models/:model::generateContent` endpoint.
///
/// The filter accepts POST requests with a JSON body in Google Generative AI format,
/// routes to the appropriate language model, and returns the response in the same format.
///
/// Both non-streaming and streaming (SSE) modes are supported.
pub fn generate_content_filter<T, R>(
    table: Arc<T>,
    router: Arc<R>,
    metrics: Arc<MetricsStore>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    warp::path!("v1beta" / "models" / String)
        .and(warp::post())
        .and(warp::body::json::<GenerateContentRequest>())
        .and(warp::any().map(move || table.clone()))
        .and(warp::any().map(move || router.clone()))
        .and(warp::any().map(move || metrics.clone()))
        .and_then(handle_generate_content)
}

async fn handle_generate_content<T, R>(
    model_action: String,
    request: GenerateContentRequest,
    table: Arc<T>,
    router: Arc<R>,
    metrics: Arc<MetricsStore>,
) -> Result<Box<dyn warp::Reply>, warp::Rejection>
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    // The path segment is like "gemini-2.0-flash:generateContent" or
    // "gemini-2.0-flash:streamGenerateContent"
    let action = model_action.rsplit_once(':').map(|(_, a)| a).unwrap_or("");
    let is_stream = request.stream.unwrap_or(false) || action == "streamGenerateContent";

    // Extract model name from the path segment (everything before the last colon)
    let model_from_path = model_action
        .rsplit_once(':')
        .map(|(before, _)| before)
        .unwrap_or(&model_action);

    // Use the model from the request body if specified, otherwise use path
    let incoming_model = if request.model.is_empty() {
        model_from_path.to_owned()
    } else {
        convert::extract_model_name(&request).to_owned()
    };

    let target = table
        .route(&incoming_model)
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

    let endpoint = format!("{}:{}", target.provider_name, target.model_id);

    let model = router
        .route_model(target)
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

    let model_id = model.model_id().to_owned();
    let options = convert::to_call_options(request);

    let start = Instant::now();

    if is_stream {
        let result = handle_stream(&model, &model_id, options).await;
        let latency_ms = start.elapsed().as_millis() as u64;
        metrics.record(RequestMetrics {
            route: incoming_model,
            endpoint,
            latency_ms,
            is_error: result.is_err(),
            input_tokens: None,
            output_tokens: None,
        });
        result
    } else {
        let gen_result = model.generate(options).await;
        let latency_ms = start.elapsed().as_millis() as u64;
        match gen_result {
            Ok(result) => {
                let input_tokens = result.usage.input_tokens.total;
                let output_tokens = result.usage.output_tokens.total;
                metrics.record(RequestMetrics {
                    route: incoming_model,
                    endpoint,
                    latency_ms,
                    is_error: false,
                    input_tokens,
                    output_tokens,
                });
                let response = convert::from_generate_result(&model_id, result);
                Ok(Box::new(warp::reply::json(&response)))
            }
            Err(e) => {
                metrics.record(RequestMetrics {
                    route: incoming_model,
                    endpoint,
                    latency_ms,
                    is_error: true,
                    input_tokens: None,
                    output_tokens: None,
                });
                Err(warp::reject::custom(BitrouterRejection(e)))
            }
        }
    }
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
            if let Some(chunk) = convert::stream_part_to_chunk(&model_id, &part) {
                let data = serde_json::to_string(&chunk).unwrap_or_default();
                let sse = Ok(warp::sse::Event::default().data(data));
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
