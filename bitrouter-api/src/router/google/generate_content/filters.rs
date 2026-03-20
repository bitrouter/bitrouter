//! Warp filters for Google Generative AI compatible endpoints.

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

use super::{convert, types::GenerateContentRequest};

/// Extracts the incoming model name and stream flag from the Google
/// `v1beta/models/{model_action}` path segment and the request body.
fn parse_google_request(model_action: &str, request: &GenerateContentRequest) -> (String, bool) {
    let action = model_action.rsplit_once(':').map(|(_, a)| a).unwrap_or("");
    let is_stream = request.stream.unwrap_or(false) || action == "streamGenerateContent";

    let model_from_path = model_action
        .rsplit_once(':')
        .map(|(before, _)| before)
        .unwrap_or(model_action);

    let incoming_model = if request.model.is_empty() {
        model_from_path.to_owned()
    } else {
        convert::extract_model_name(request).to_owned()
    };

    (incoming_model, is_stream)
}

/// Creates a warp filter for the `/v1beta/models/:model::generateContent` endpoint.
///
/// This is the zero-observability variant. For spend tracking and metrics,
/// use [`generate_content_filter_with_observe`].
pub fn generate_content_filter<T, R>(
    table: Arc<T>,
    router: Arc<R>,
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
        .and_then(handle_generate_content)
}

/// Like [`generate_content_filter`], but accepts a per-request hooks filter.
pub fn generate_content_filter_with_hooks<T, R, H>(
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
    warp::path!("v1beta" / "models" / String)
        .and(warp::post())
        .and(warp::body::json::<GenerateContentRequest>())
        .and(warp::any().map(move || table.clone()))
        .and(warp::any().map(move || router.clone()))
        .and(hooks_filter)
        .and_then(handle_generate_content_with_hooks)
}

/// Creates a warp filter for `/v1beta/models/:model` with observation.
pub fn generate_content_filter_with_observe<T, R, A>(
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
    warp::path!("v1beta" / "models" / String)
        .and(warp::post())
        .and(warp::body::json::<GenerateContentRequest>())
        .and(warp::any().map(move || table.clone()))
        .and(warp::any().map(move || router.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and(account_filter)
        .and_then(handle_generate_content_with_observe)
}

async fn handle_generate_content<T, R>(
    model_action: String,
    request: GenerateContentRequest,
    table: Arc<T>,
    router: Arc<R>,
) -> Result<Box<dyn warp::Reply>, warp::Rejection>
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    let (incoming_model, is_stream) = parse_google_request(&model_action, &request);

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
        let result = model
            .generate(options)
            .await
            .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;
        let response = convert::from_generate_result(&model_id, result);
        Ok(Box::new(warp::reply::json(&response)) as Box<dyn warp::Reply>)
    }
}

async fn handle_generate_content_with_hooks<T, R>(
    model_action: String,
    request: GenerateContentRequest,
    table: Arc<T>,
    router: Arc<R>,
    hooks: Arc<[Arc<dyn GenerationHook>]>,
) -> Result<Box<dyn warp::Reply>, warp::Rejection>
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    let hooked = Arc::new(HookedRouter::new(router, hooks));
    handle_generate_content(model_action, request, table, hooked).await
}

async fn handle_generate_content_with_observe<T, R>(
    model_action: String,
    request: GenerateContentRequest,
    table: Arc<T>,
    router: Arc<R>,
    observer: Arc<dyn ObserveCallback>,
    caller: CallerContext,
) -> Result<Box<dyn warp::Reply>, warp::Rejection>
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    let (incoming_model, is_stream) = parse_google_request(&model_action, &request);

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
            &model_id,
            options,
            crate::router::StreamObserveContext {
                observer,
                route: incoming_model,
                provider: provider_name,
                target_model: target_model_id,
                caller,
                start,
            },
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
        let mut converter = convert::StreamConverter::new(model_id);
        use tokio_stream::StreamExt as _;
        while let Some(part) = stream.next().await {
            if let Some(chunk) = converter.convert(&part) {
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

async fn handle_stream_with_observe(
    model: &(impl LanguageModel + ?Sized),
    model_id: &str,
    options: bitrouter_core::models::language::call_options::LanguageModelCallOptions,
    ctx: crate::router::StreamObserveContext,
) -> Result<Box<dyn warp::Reply>, warp::Rejection> {
    let stream_result = model
        .stream(options)
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

    let model_id = model_id.to_owned();

    let (tx, rx) =
        tokio::sync::mpsc::channel::<Result<warp::sse::Event, std::convert::Infallible>>(32);

    let crate::router::StreamObserveContext {
        observer,
        route,
        provider,
        target_model,
        caller,
        start,
    } = ctx;

    tokio::spawn(async move {
        let mut stream = stream_result.stream;
        let mut converter = convert::StreamConverter::new(model_id);
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
            if let Some(chunk) = converter.convert(&part) {
                let data = serde_json::to_string(&chunk).unwrap_or_default();
                let sse = Ok(warp::sse::Event::default().data(data));
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
