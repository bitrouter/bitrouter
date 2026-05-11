//! Warp filters for OpenAI Responses compatible endpoints.

use std::sync::Arc;
use std::time::Instant;

#[cfg(any(feature = "payments-tempo", feature = "payments-solana"))]
use bitrouter_core::observe::MetadataHook;
#[cfg(any(feature = "payments-tempo", feature = "payments-solana"))]
use bitrouter_core::routers::router::{DynTargetOverlay, TargetOverlay};
use bitrouter_core::{
    auth::access::is_model_allowed,
    errors::BitrouterError,
    hooks::{GenerationHook, HookedRouter},
    models::language::language_model::LanguageModel,
    observe::{
        CallerContext, ObserveCallback, RequestContext, RequestFailureEvent, RequestSuccessEvent,
    },
    routers::{router::LanguageModelRouter, routing_table::RoutingTable},
};
use warp::Filter;

use crate::error::BitrouterRejection;

use super::{convert, types::ResponsesRequest};

use crate::router::context::openai_responses;

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
        .and(crate::body::json::<ResponsesRequest>())
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
        .and(crate::body::json::<ResponsesRequest>())
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
        .and(crate::body::json::<ResponsesRequest>())
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
    let route_ctx = openai_responses::extract(&request);

    let target = table
        .route(&incoming_model, &route_ctx)
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

    let model = router
        .route_model(target.clone())
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

    let model_id = model.model_id().to_owned();
    let options = convert::to_call_options(request);

    if is_stream {
        handle_stream(&model, options, model_id).await
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

/// Creates a warp filter for `/v1/responses` with MPP payment gating.
#[cfg(any(feature = "payments-tempo", feature = "payments-solana"))]
pub fn responses_filter_with_mpp<T, R, A>(
    table: Arc<T>,
    router: Arc<R>,
    observer: Arc<dyn ObserveCallback>,
    mpp_state: Arc<crate::mpp::MppState>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: RoutingTable + crate::mpp::PricingLookup + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    warp::path!("v1" / "responses")
        .and(warp::post())
        .and(account_filter)
        .and(warp::any().map(move || mpp_state.clone()))
        .and(warp::header::optional::<String>("authorization"))
        .and(crate::body::json::<ResponsesRequest>())
        .and(warp::any().map(move || table.clone()))
        .and(warp::any().map(move || router.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and_then(handle_responses_with_mpp)
}

/// Creates a warp filter for `/v1/responses` with a custom [`PaymentGate`].
///
/// Like [`responses_filter_with_mpp`], but accepts any
/// [`crate::mpp::PaymentGate`] implementation instead of requiring
/// [`crate::mpp::MppState`] directly.
#[cfg(any(feature = "payments-tempo", feature = "payments-solana"))]
pub fn responses_filter_with_payment_gate<T, R, A>(
    table: Arc<T>,
    router: Arc<R>,
    observer: Arc<dyn ObserveCallback>,
    payment_gate: Arc<dyn crate::mpp::PaymentGate>,
    metadata_hook: MetadataHook,
    target_overlay: Option<Arc<DynTargetOverlay<'static>>>,
    account_filter: A,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: RoutingTable + crate::mpp::PricingLookup + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    warp::path!("v1" / "responses")
        .and(warp::post())
        .and(account_filter)
        .and(warp::any().map(move || payment_gate.clone()))
        .and(warp::header::optional::<String>("authorization"))
        .and(warp::header::optional::<String>("origin"))
        .and(crate::body::json::<ResponsesRequest>())
        .and(warp::any().map(move || table.clone()))
        .and(warp::any().map(move || router.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and(warp::any().map(move || metadata_hook.clone()))
        .and(warp::any().map(move || target_overlay.clone()))
        .and_then(
            |caller: CallerContext,
             gate: Arc<dyn crate::mpp::PaymentGate>,
             auth_header: Option<String>,
             origin: Option<String>,
             request: ResponsesRequest,
             table: Arc<T>,
             router: Arc<R>,
             observer: Arc<dyn ObserveCallback>,
             metadata_hook: MetadataHook,
             target_overlay: Option<Arc<DynTargetOverlay<'static>>>| {
                let gate_ctx = crate::mpp::GateContext {
                    caller,
                    payment_gate: gate,
                    auth_header,
                    observer,
                    metadata_hook,
                    origin,
                    target_overlay,
                };
                handle_responses_with_gate(gate_ctx, request, table, router)
            },
        )
}

#[cfg(any(feature = "payments-tempo", feature = "payments-solana"))]
async fn handle_responses_with_mpp<T, R>(
    caller: CallerContext,
    mpp_state: Arc<crate::mpp::MppState>,
    auth_header: Option<String>,
    request: ResponsesRequest,
    table: Arc<T>,
    router: Arc<R>,
    observer: Arc<dyn ObserveCallback>,
) -> Result<Box<dyn warp::Reply>, warp::Rejection>
where
    T: RoutingTable + crate::mpp::PricingLookup + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    let payment_gate: Arc<dyn crate::mpp::PaymentGate> = mpp_state;
    let gate_ctx = crate::mpp::GateContext {
        caller,
        payment_gate,
        auth_header,
        observer,
        metadata_hook: bitrouter_core::observe::default_metadata_hook(),
        origin: None,
        // _with_mpp does not expose a TargetOverlay constructor parameter;
        // consumers needing per-request target mutation should use
        // _with_payment_gate instead.
        target_overlay: None,
    };
    handle_responses_with_gate(gate_ctx, request, table, router).await
}

#[cfg(any(feature = "payments-tempo", feature = "payments-solana"))]
async fn handle_responses_with_gate<T, R>(
    gate_ctx: crate::mpp::GateContext,
    request: ResponsesRequest,
    table: Arc<T>,
    router: Arc<R>,
) -> Result<Box<dyn warp::Reply>, warp::Rejection>
where
    T: RoutingTable + crate::mpp::PricingLookup + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    let crate::mpp::GateContext {
        caller,
        payment_gate,
        auth_header,
        observer,
        metadata_hook,
        origin,
        target_overlay,
    } = gate_ctx;
    let mpp_ctx = payment_gate
        .verify_payment(caller.chain.clone(), auth_header)
        .await?;

    if let Some(ref management) = mpp_ctx.management_response {
        let reply = warp::reply::json(management);
        if let Ok(receipt_header) = mpp::format_receipt(&mpp_ctx.receipt) {
            return Ok(Box::new(warp::reply::with_header(
                reply,
                mpp::PAYMENT_RECEIPT_HEADER,
                receipt_header,
            )));
        }
        return Ok(Box::new(reply));
    }

    let is_stream = request.stream.unwrap_or(false);
    let incoming_model = convert::extract_model_name(&request).to_owned();
    let route_ctx = openai_responses::extract(&request);

    if let Some(ref allowed) = caller.models
        && !is_model_allowed(&incoming_model, allowed)
    {
        return Err(warp::reject::custom(BitrouterRejection(
            BitrouterError::AccessDenied {
                message: format!("model '{}' is not in your allowlist", incoming_model),
            },
        )));
    }

    let mut target = table
        .route(&incoming_model, &route_ctx)
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

    if let Some(ref overlay) = target_overlay {
        overlay
            .apply(&mut target, &caller)
            .await
            .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;
    }

    let byok_used = target.api_key_override.is_some();
    let provider_name = target.provider_name.clone();
    let target_model_id = target.service_id.clone();

    let model = router
        .route_model(target.clone())
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

    let model_id = model.model_id().to_owned();
    let options = convert::to_call_options(request);
    let start = Instant::now();
    let request_id = uuid::Uuid::new_v4().to_string();
    let mut metadata = metadata_hook(&caller, &origin);
    if byok_used {
        match metadata {
            serde_json::Value::Object(ref mut map) => {
                map.insert("byok_used".to_string(), serde_json::Value::Bool(true));
            }
            _ => {
                metadata = serde_json::json!({ "byok_used": true });
            }
        }
    }

    if is_stream {
        let stream_result = model
            .stream(options)
            .await
            .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

        let (tx, rx) =
            tokio::sync::mpsc::channel::<Result<warp::sse::Event, std::convert::Infallible>>(32);

        let pricing = table.model_pricing(&provider_name, &target_model_id);
        let tick_cost = crate::mpp::cost_to_micro_units(
            pricing.output_tokens.text.unwrap_or(0.0) / 1_000_000.0,
        );

        let metered = crate::mpp::metered_sse::MeteredSseContext {
            payment_gate: payment_gate.clone(),
            backend_key: mpp_ctx.backend_key.clone(),
            channel_id: mpp_ctx.channel_id.clone(),
            tick_cost: if byok_used { 0 } else { tick_cost },
            skip_deduct: byok_used,
            request_id: Some(request_id.clone()),
        };

        let stream_model_id = model_id.clone();
        tokio::spawn(async move {
            let mut stream = stream_result.stream;
            let mut converter = convert::StreamConverter::new(stream_model_id);
            use tokio_stream::StreamExt as _;
            let mut observation = crate::router::StreamObservation::new();
            let mut client_disconnected = false;
            let mut saw_finish = false;
            while let Some(part) = stream.next().await {
                observation.record_part(&part);
                if matches!(
                    part,
                    bitrouter_core::models::language::stream_part::LanguageModelStreamPart::Finish { .. }
                ) {
                    saw_finish = true;
                }
                let events = converter.convert(&part);
                if !events.is_empty() && !metered.deduct_or_pause(&tx).await {
                    client_disconnected = true;
                    break;
                }
                let mut send_failed = false;
                for event in events {
                    let data = serde_json::to_string(&event).unwrap_or_default();
                    let sse = Ok(warp::sse::Event::default()
                        .event(&event.event_type)
                        .data(data));
                    if tx.send(sse).await.is_err() {
                        send_failed = true;
                        break;
                    }
                }
                if send_failed {
                    client_disconnected = true;
                    break;
                }
            }
            // Ensure the stream closes with `response.completed` per the
            // Responses spec; Codex CLI retries otherwise.
            // https://platform.openai.com/docs/api-reference/responses-streaming
            if !saw_finish && !client_disconnected {
                for event in converter.finish() {
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
            // Responses SSE has no [DONE] terminator.

            let latency_ms = start.elapsed().as_millis() as u64;
            let ctx = RequestContext {
                route: incoming_model,
                provider: provider_name,
                model: target_model_id,
                caller,
                latency_ms,
                request_id,
                metadata,
            };
            match observation.outcome(client_disconnected) {
                Ok(usage) => {
                    observer
                        .on_request_success(RequestSuccessEvent {
                            ctx,
                            executed_target: Some(target.clone()),
                            usage,
                            streamed: true,
                            generation_time_ms: None,
                        })
                        .await;
                }
                Err(error) => {
                    observer
                        .on_request_failure(RequestFailureEvent {
                            ctx,
                            executed_target: Some(target.clone()),
                            error,
                        })
                        .await;
                }
            }
        });

        let sse_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        let reply = crate::router::sse::reply(sse_stream);
        if let Ok(receipt_header) = mpp::format_receipt(&mpp_ctx.receipt) {
            Ok(Box::new(warp::reply::with_header(
                reply,
                mpp::PAYMENT_RECEIPT_HEADER,
                receipt_header,
            )))
        } else {
            Ok(Box::new(reply) as Box<dyn warp::Reply>)
        }
    } else {
        let gen_result = model.generate(options).await;
        match gen_result {
            Ok(result) => {
                let pricing = table.model_pricing(&provider_name, &target_model_id);
                let cost_usd = crate::mpp::calculate_usage_cost(&result.usage, &pricing);
                let micro_units = crate::mpp::cost_to_micro_units(cost_usd);

                if !byok_used
                    && micro_units > 0
                    && let Err(e) = payment_gate
                        .deduct(
                            &mpp_ctx.backend_key,
                            &mpp_ctx.channel_id,
                            micro_units,
                            Some(request_id.as_str()),
                        )
                        .await
                {
                    tracing::warn!(
                        channel_id = %mpp_ctx.channel_id,
                        amount = micro_units,
                        error = %e,
                        "MPP deduction failed after successful generation"
                    );
                }

                let event = RequestSuccessEvent {
                    ctx: RequestContext {
                        route: incoming_model,
                        provider: provider_name,
                        model: target_model_id,
                        caller,
                        latency_ms: start.elapsed().as_millis() as u64,
                        request_id,
                        metadata,
                    },
                    executed_target: Some(target.clone()),
                    usage: result.usage.clone(),
                    streamed: false,
                    generation_time_ms: None,
                };
                tokio::spawn(async move { observer.on_request_success(event).await });

                let response = convert::from_generate_result(&model_id, result);
                let reply = warp::reply::json(&response);

                if let Ok(receipt_header) = mpp::format_receipt(&mpp_ctx.receipt) {
                    Ok(Box::new(warp::reply::with_header(
                        reply,
                        mpp::PAYMENT_RECEIPT_HEADER,
                        receipt_header,
                    )))
                } else {
                    Ok(Box::new(reply) as Box<dyn warp::Reply>)
                }
            }
            Err(e) => {
                let event = RequestFailureEvent {
                    ctx: RequestContext {
                        route: incoming_model,
                        provider: provider_name,
                        model: target_model_id,
                        caller,
                        latency_ms: start.elapsed().as_millis() as u64,
                        request_id,
                        metadata,
                    },
                    executed_target: Some(target.clone()),
                    error: e.clone(),
                };
                tokio::spawn(async move { observer.on_request_failure(event).await });
                Err(warp::reject::custom(BitrouterRejection(e)))
            }
        }
    }
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
    let route_ctx = openai_responses::extract(&request);

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
        .route(&incoming_model, &route_ctx)
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

    let provider_name = target.provider_name.clone();
    let target_model_id = target.service_id.clone();

    let model = router
        .route_model(target.clone())
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

    let model_id = model.model_id().to_owned();
    let options = convert::to_call_options(request);
    let start = Instant::now();

    if is_stream {
        handle_stream_with_observe(
            &model,
            options,
            model_id,
            crate::router::StreamObserveContext {
                observer,
                route: incoming_model,
                provider: provider_name,
                target_model: target_model_id,
                caller,
                start,
                request_id: uuid::Uuid::new_v4().to_string(),
                metadata: serde_json::Value::Null,
                executed_target: Some(target.clone()),
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
                        request_id: uuid::Uuid::new_v4().to_string(),
                        metadata: serde_json::Value::Null,
                    },
                    executed_target: Some(target.clone()),
                    usage: result.usage.clone(),
                    streamed: false,
                    generation_time_ms: None,
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
                        request_id: uuid::Uuid::new_v4().to_string(),
                        metadata: serde_json::Value::Null,
                    },
                    executed_target: Some(target.clone()),
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
    model_id: String,
) -> Result<Box<dyn warp::Reply>, warp::Rejection> {
    let stream_result = model
        .stream(options)
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

    let (tx, rx) =
        tokio::sync::mpsc::channel::<Result<warp::sse::Event, std::convert::Infallible>>(32);

    tokio::spawn(async move {
        let mut stream = stream_result.stream;
        let mut converter = convert::StreamConverter::new(model_id);
        use tokio_stream::StreamExt as _;
        let mut saw_finish = false;
        'outer: while let Some(part) = stream.next().await {
            if matches!(
                part,
                bitrouter_core::models::language::stream_part::LanguageModelStreamPart::Finish { .. }
            ) {
                saw_finish = true;
            }
            for event in converter.convert(&part) {
                let data = serde_json::to_string(&event).unwrap_or_default();
                let sse = Ok(warp::sse::Event::default()
                    .event(&event.event_type)
                    .data(data));
                if tx.send(sse).await.is_err() {
                    break 'outer;
                }
            }
        }
        // Upstream closed without a Finish part — synthesize a final
        // `response.completed` so strict clients (e.g. Codex CLI) don't
        // hit "stream closed before response.completed" and retry.
        // https://platform.openai.com/docs/api-reference/responses-streaming
        if !saw_finish {
            for event in converter.finish() {
                let data = serde_json::to_string(&event).unwrap_or_default();
                let sse = Ok(warp::sse::Event::default()
                    .event(&event.event_type)
                    .data(data));
                if tx.send(sse).await.is_err() {
                    break;
                }
            }
        }
        // Responses SSE has no [DONE] terminator; `response.completed` is
        // the canonical close. Emitting [DONE] here would cause Codex to
        // surface a parse error and retry.
    });

    let sse_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    Ok(Box::new(crate::router::sse::reply(sse_stream)))
}

async fn handle_stream_with_observe(
    model: &(impl LanguageModel + ?Sized),
    options: bitrouter_core::models::language::call_options::LanguageModelCallOptions,
    model_id: String,
    ctx: crate::router::StreamObserveContext,
) -> Result<Box<dyn warp::Reply>, warp::Rejection> {
    let stream_result = model
        .stream(options)
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

    let (tx, rx) =
        tokio::sync::mpsc::channel::<Result<warp::sse::Event, std::convert::Infallible>>(32);

    let crate::router::StreamObserveContext {
        observer,
        route,
        provider,
        target_model,
        caller,
        start,
        request_id,
        metadata,
        executed_target,
    } = ctx;

    tokio::spawn(async move {
        let mut stream = stream_result.stream;
        let mut converter = convert::StreamConverter::new(model_id);
        use tokio_stream::StreamExt as _;
        let mut observation = crate::router::StreamObservation::new();
        let mut client_disconnected = false;
        let mut saw_finish = false;
        while let Some(part) = stream.next().await {
            observation.record_part(&part);
            if matches!(
                part,
                bitrouter_core::models::language::stream_part::LanguageModelStreamPart::Finish { .. }
            ) {
                saw_finish = true;
            }
            let mut send_failed = false;
            for event in converter.convert(&part) {
                let data = serde_json::to_string(&event).unwrap_or_default();
                let sse = Ok(warp::sse::Event::default()
                    .event(&event.event_type)
                    .data(data));
                if tx.send(sse).await.is_err() {
                    send_failed = true;
                    break;
                }
            }
            if send_failed {
                client_disconnected = true;
                break;
            }
        }
        // Synthesize `response.completed` when the upstream stopped without
        // a Finish so Codex CLI sees a well-formed terminator instead of
        // raising "stream closed before response.completed" and retrying.
        // https://platform.openai.com/docs/api-reference/responses-streaming
        if !saw_finish && !client_disconnected {
            for event in converter.finish() {
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
        // Responses SSE has no [DONE] terminator.

        let latency_ms = start.elapsed().as_millis() as u64;
        let ctx = RequestContext {
            route,
            provider,
            model: target_model,
            caller,
            latency_ms,
            request_id,
            metadata,
        };
        match observation.outcome(client_disconnected) {
            Ok(usage) => {
                observer
                    .on_request_success(RequestSuccessEvent {
                        ctx,
                        executed_target: executed_target.clone(),
                        usage,
                        streamed: true,
                        generation_time_ms: None,
                    })
                    .await;
            }
            Err(error) => {
                observer
                    .on_request_failure(RequestFailureEvent {
                        ctx,
                        executed_target,
                        error,
                    })
                    .await;
            }
        }
    });

    let sse_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    Ok(Box::new(crate::router::sse::reply(sse_stream)))
}
