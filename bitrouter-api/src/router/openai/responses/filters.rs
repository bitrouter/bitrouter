//! Warp filters for OpenAI Responses compatible endpoints.

use std::sync::Arc;
use std::time::Instant;

use bitrouter_core::{
    auth::access::is_model_allowed,
    errors::BitrouterError,
    hooks::{GenerationHook, HookedRouter},
    models::language::language_model::LanguageModel,
    observe::{
        CallerContext, ObserveCallback, RequestContext, RequestFailureEvent, RequestSuccessEvent,
    },
    routers::{model_router::LanguageModelRouter, routing_table::RoutingTable},
};
use warp::Filter;

use crate::error::BitrouterRejection;

use super::{convert, types::ResponsesRequest};

/// Creates a warp filter for the `/v1/responses` endpoint.
///
/// This is the zero-observability variant. For spend tracking and metrics,
/// use [`responses_filter_with_observe`].
pub fn responses_filter<T, R>(
    table: Arc<T>,
    router: Arc<R>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    warp::path!("v1" / "responses")
        .and(warp::post())
        .and(warp::body::json::<ResponsesRequest>())
        .and(warp::any().map(move || table.clone()))
        .and(warp::any().map(move || router.clone()))
        .and_then(handle_responses)
}

/// Like [`responses_filter`], but accepts a per-request hooks filter.
pub fn responses_filter_with_hooks<T, R, H>(
    table: Arc<T>,
    router: Arc<R>,
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
    warp::path!("v1" / "responses")
        .and(warp::post())
        .and(warp::body::json::<ResponsesRequest>())
        .and(warp::any().map(move || table.clone()))
        .and(warp::any().map(move || router.clone()))
        .and(hooks_filter)
        .and_then(handle_responses_with_hooks)
}

/// Creates a warp filter for `/v1/responses` with observation.
pub fn responses_filter_with_observe<T, R, A>(
    table: Arc<T>,
    router: Arc<R>,
    observer: Arc<dyn ObserveCallback>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    warp::path!("v1" / "responses")
        .and(warp::post())
        .and(warp::body::json::<ResponsesRequest>())
        .and(warp::any().map(move || table.clone()))
        .and(warp::any().map(move || router.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and(account_filter)
        .and_then(handle_responses_with_observe)
}

async fn handle_responses<T, R>(
    request: ResponsesRequest,
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
        handle_stream(&model, options).await
    } else {
        let result = model
            .generate(options)
            .await
            .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;
        let response = convert::from_generate_result(&model_id, result);
        Ok(Box::new(warp::reply::json(&response)) as Box<dyn warp::Reply>)
    }
}

async fn handle_responses_with_hooks<T, R>(
    request: ResponsesRequest,
    table: Arc<T>,
    router: Arc<R>,
    hooks: Arc<[Arc<dyn GenerationHook>]>,
) -> Result<Box<dyn warp::Reply>, warp::Rejection>
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    let hooked = Arc::new(HookedRouter::new(router, hooks));
    handle_responses(request, table, hooked).await
}

async fn handle_responses_with_observe<T, R>(
    request: ResponsesRequest,
    table: Arc<T>,
    router: Arc<R>,
    observer: Arc<dyn ObserveCallback>,
    caller: CallerContext,
) -> Result<Box<dyn warp::Reply>, warp::Rejection>
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    let is_stream = request.stream.unwrap_or(false);
    let incoming_model = convert::extract_model_name(&request).to_owned();

    if let Some(ref allowed) = caller.models
        && !is_model_allowed(&incoming_model, allowed)
    {
        return Err(warp::reject::custom(BitrouterRejection(
            BitrouterError::AccessDenied {
                message: format!("model '{}' is not in your allowlist", incoming_model),
            },
        )));
    }

    let target = table
        .route(&incoming_model)
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

    let provider_name = target.provider_name.clone();
    let target_model_id = target.model_id.clone();

    let model = router
        .route_model(target)
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

    let model_id = model.model_id().to_owned();
    let options = convert::to_call_options(request);
    let start = Instant::now();

    if is_stream {
        handle_stream_with_observe(
            &model,
            options,
            observer,
            incoming_model,
            provider_name,
            target_model_id,
            caller,
            start,
        )
        .await
    } else {
        let gen_result = model.generate(options).await;
        match gen_result {
            Ok(result) => {
                let event = RequestSuccessEvent {
                    ctx: RequestContext {
                        route: incoming_model,
                        provider: provider_name,
                        model: target_model_id,
                        caller,
                        latency_ms: start.elapsed().as_millis() as u64,
                    },
                    usage: result.usage.clone(),
                };
                tokio::spawn(async move { observer.on_request_success(event).await });
                let response = convert::from_generate_result(&model_id, result);
                Ok(Box::new(warp::reply::json(&response)) as Box<dyn warp::Reply>)
            }
            Err(e) => {
                let event = RequestFailureEvent {
                    ctx: RequestContext {
                        route: incoming_model,
                        provider: provider_name,
                        model: target_model_id,
                        caller,
                        latency_ms: start.elapsed().as_millis() as u64,
                    },
                    error: e.clone(),
                };
                tokio::spawn(async move { observer.on_request_failure(event).await });
                Err(warp::reject::custom(BitrouterRejection(e)))
            }
        }
    }
}

async fn handle_stream(
    model: &(impl LanguageModel + ?Sized),
    options: bitrouter_core::models::language::call_options::LanguageModelCallOptions,
) -> Result<Box<dyn warp::Reply>, warp::Rejection> {
    let stream_result = model
        .stream(options)
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

    let (tx, rx) =
        tokio::sync::mpsc::channel::<Result<warp::sse::Event, std::convert::Infallible>>(32);

    tokio::spawn(async move {
        let mut stream = stream_result.stream;
        use tokio_stream::StreamExt as _;
        while let Some(part) = stream.next().await {
            if let Some(event) = convert::stream_part_to_event(&part) {
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

#[allow(clippy::too_many_arguments)]
async fn handle_stream_with_observe(
    model: &(impl LanguageModel + ?Sized),
    options: bitrouter_core::models::language::call_options::LanguageModelCallOptions,
    observer: Arc<dyn ObserveCallback>,
    route: String,
    provider: String,
    target_model: String,
    caller: CallerContext,
    start: Instant,
) -> Result<Box<dyn warp::Reply>, warp::Rejection> {
    let stream_result = model
        .stream(options)
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

    let (tx, rx) =
        tokio::sync::mpsc::channel::<Result<warp::sse::Event, std::convert::Infallible>>(32);

    tokio::spawn(async move {
        let mut stream = stream_result.stream;
        use bitrouter_core::models::language::stream_part::LanguageModelStreamPart;
        use tokio_stream::StreamExt as _;
        let mut usage = None;
        let mut client_disconnected = false;
        while let Some(part) = stream.next().await {
            if let LanguageModelStreamPart::Finish {
                usage: ref finish_usage,
                ..
            } = part
            {
                usage = Some(finish_usage.clone());
            }
            if let Some(event) = convert::stream_part_to_event(&part) {
                let data = serde_json::to_string(&event).unwrap_or_default();
                let sse = Ok(warp::sse::Event::default()
                    .event(&event.event_type)
                    .data(data));
                if tx.send(sse).await.is_err() {
                    client_disconnected = true;
                    break;
                }
            }
        }
        let _ = tx
            .send(Ok(warp::sse::Event::default().data("[DONE]")))
            .await;

        let latency_ms = start.elapsed().as_millis() as u64;
        let ctx = RequestContext {
            route,
            provider,
            model: target_model,
            caller,
            latency_ms,
        };
        if let Some(usage) = usage {
            observer
                .on_request_success(RequestSuccessEvent { ctx, usage })
                .await;
        } else if client_disconnected {
            observer
                .on_request_failure(RequestFailureEvent {
                    ctx,
                    error: bitrouter_core::errors::BitrouterError::cancelled(
                        None,
                        "client disconnected during stream",
                    ),
                })
                .await;
        } else {
            observer
                .on_request_failure(RequestFailureEvent {
                    ctx,
                    error: bitrouter_core::errors::BitrouterError::stream_protocol(
                        None,
                        "stream completed without finish event",
                        None,
                    ),
                })
                .await;
        }
    });

    let sse_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    Ok(Box::new(warp::sse::reply(sse_stream)))
}
