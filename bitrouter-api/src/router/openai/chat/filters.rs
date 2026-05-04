//! Warp filters for OpenAI Chat Completions compatible endpoints.

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

use crate::fallback::{FallbackDecision, FallbackPolicy, default_fallback_policy};
use crate::router::context::openai_chat;
use warp::Filter;

use crate::error::{BadRequest, BitrouterRejection};
use crate::util::generate_id;

use super::{convert, types::ChatCompletionRequest};

/// Creates a warp filter for the `/v1/chat/completions` endpoint.
///
/// This is the zero-observability variant. For spend tracking and metrics,
/// use [`chat_completions_filter_with_observe`].
pub fn chat_completions_filter<T, R>(
    table: Arc<T>,
    router: Arc<R>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    chat_completions_filter_with_fallback_policy(table, router, default_fallback_policy())
}

/// Creates a warp filter for the `/v1/chat/completions` endpoint with a custom
/// fallback policy.
pub fn chat_completions_filter_with_fallback_policy<T, R>(
    table: Arc<T>,
    router: Arc<R>,
    fallback_policy: Arc<dyn FallbackPolicy>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    warp::path!("v1" / "chat" / "completions")
        .and(warp::post())
        .and(crate::body::json::<ChatCompletionRequest>())
        .and(warp::any().map(move || table.clone()))
        .and(warp::any().map(move || router.clone()))
        .and(warp::any().map(move || fallback_policy.clone()))
        .and_then(handle_chat_completion)
}

/// Like [`chat_completions_filter`], but accepts a per-request hooks filter.
pub fn chat_completions_filter_with_hooks<T, R, H>(
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
    warp::path!("v1" / "chat" / "completions")
        .and(warp::post())
        .and(crate::body::json::<ChatCompletionRequest>())
        .and(warp::any().map(move || table.clone()))
        .and(warp::any().map(move || router.clone()))
        .and(hooks_filter)
        .and_then(handle_chat_completion_with_hooks)
}

/// Creates a warp filter for `/v1/chat/completions` with observation.
///
/// The `observer` receives success/failure events with full request context.
/// The `account_filter` extracts the account ID from the request (or `None`
/// when auth is disabled).
pub fn chat_completions_filter_with_observe<T, R, A>(
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
    chat_completions_filter_with_observe_and_fallback_policy(
        table,
        router,
        observer,
        account_filter,
        default_fallback_policy(),
    )
}

/// Creates a warp filter for `/v1/chat/completions` with observation and a
/// custom fallback policy.
pub fn chat_completions_filter_with_observe_and_fallback_policy<T, R, A>(
    table: Arc<T>,
    router: Arc<R>,
    observer: Arc<dyn ObserveCallback>,
    account_filter: A,
    fallback_policy: Arc<dyn FallbackPolicy>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
    A: Filter<Extract = (CallerContext,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    warp::path!("v1" / "chat" / "completions")
        .and(warp::post())
        .and(crate::body::json::<ChatCompletionRequest>())
        .and(warp::any().map(move || table.clone()))
        .and(warp::any().map(move || router.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and(account_filter)
        .and(warp::any().map(move || fallback_policy.clone()))
        .and_then(handle_chat_completion_with_observe)
}

async fn handle_chat_completion<T, R>(
    request: ChatCompletionRequest,
    table: Arc<T>,
    router: Arc<R>,
    fallback_policy: Arc<dyn FallbackPolicy>,
) -> Result<Box<dyn warp::Reply>, warp::Rejection>
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    let is_stream = request.stream.unwrap_or(false);
    let incoming_model = convert::extract_model_name(&request).to_owned();
    let route_ctx = openai_chat::extract(&request);

    let chain = table
        .route_chain(&incoming_model, &route_ctx)
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;
    let options = convert::to_call_options(request);

    let mut last_err = None;
    for target in chain {
        let model = router
            .route_model(target.clone())
            .await
            .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;

        let model_id = model.model_id().to_owned();
        if is_stream {
            match model.stream(options.clone()).await {
                Ok(stream_result) => return handle_stream_result(stream_result, &model_id).await,
                Err(e) if fallback_policy.classify(&e, &target) == FallbackDecision::Fallback => {
                    last_err = Some(e);
                }
                Err(e) => return Err(warp::reject::custom(BitrouterRejection(e))),
            }
        } else {
            match model.generate(options.clone()).await {
                Ok(result) => {
                    let response = convert::from_generate_result(&model_id, result);
                    return Ok(Box::new(warp::reply::json(&response)) as Box<dyn warp::Reply>);
                }
                Err(e) if fallback_policy.classify(&e, &target) == FallbackDecision::Fallback => {
                    last_err = Some(e);
                }
                Err(e) => return Err(warp::reject::custom(BitrouterRejection(e))),
            }
        }
    }

    let error = last_err.unwrap_or_else(|| {
        BitrouterError::invalid_request(
            None,
            format!("no routing targets resolved for model: {incoming_model}"),
            None,
        )
    });
    Err(warp::reject::custom(BitrouterRejection(error)))
}

async fn handle_chat_completion_with_hooks<T, R>(
    request: ChatCompletionRequest,
    table: Arc<T>,
    router: Arc<R>,
    hooks: Arc<[Arc<dyn GenerationHook>]>,
) -> Result<Box<dyn warp::Reply>, warp::Rejection>
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    let hooked = Arc::new(HookedRouter::new(router, hooks));
    handle_chat_completion(request, table, hooked, default_fallback_policy()).await
}

#[cfg(any(feature = "payments-tempo", feature = "payments-solana"))]
async fn handle_chat_completion_with_mpp<T, R>(
    caller: CallerContext,
    mpp_state: Arc<crate::mpp::MppState>,
    auth_header: Option<String>,
    request: ChatCompletionRequest,
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
    handle_chat_completion_with_gate(gate_ctx, request, table, router).await
}

#[cfg(any(feature = "payments-tempo", feature = "payments-solana"))]
async fn handle_chat_completion_with_gate<T, R>(
    gate_ctx: crate::mpp::GateContext,
    request: ChatCompletionRequest,
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

    // Management actions (channel open/topUp/close) short-circuit request processing.
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

    // Guard closes the payment channel on-chain when the request finishes.
    // Moved into the streaming task or dropped at handler scope end.
    let _close_guard = crate::mpp::SessionCloseGuard::new(
        payment_gate.clone(),
        mpp_ctx.backend_key.clone(),
        mpp_ctx.channel_id.clone(),
    );

    let is_stream = request.stream.unwrap_or(false);
    let incoming_model = convert::extract_model_name(&request).to_owned();
    let route_ctx = openai_chat::extract(&request);

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

        let stream_id = format!("chatcmpl-{}", generate_id());

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

        tokio::spawn(async move {
            // Hold the close guard inside the task; channel is closed when the
            // task ends (success, error, or disconnect).
            let _close_guard = _close_guard;

            let mut stream = stream_result.stream;
            let mut converter = convert::StreamConverter::new(model_id, stream_id);
            use tokio_stream::StreamExt as _;
            let mut observation = crate::router::StreamObservation::new();
            let mut client_disconnected = false;
            while let Some(part) = stream.next().await {
                observation.record_part(&part);
                if let Some(chunk) = converter.convert(&part) {
                    if !metered.deduct_or_pause(&tx).await {
                        client_disconnected = true;
                        break;
                    }
                    let data = serde_json::to_string(&chunk).unwrap_or_default();
                    let event = Ok(warp::sse::Event::default().data(data));
                    if tx.send(event).await.is_err() {
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
                // Compute cost and deduct from channel.
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

/// Creates a warp filter for `/v1/chat/completions` with MPP payment gating.
///
/// Requires JWT authentication. The JWT `chain` claim selects the payment
/// backend. Requests must also carry a `Payment` credential in the
/// `Authorization` header.
#[cfg(any(feature = "payments-tempo", feature = "payments-solana"))]
pub fn chat_completions_filter_with_mpp<T, R, A>(
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
    warp::path!("v1" / "chat" / "completions")
        .and(warp::post())
        .and(account_filter)
        .and(warp::any().map(move || mpp_state.clone()))
        .and(warp::header::optional::<String>("authorization"))
        .and(crate::body::json::<ChatCompletionRequest>())
        .and(warp::any().map(move || table.clone()))
        .and(warp::any().map(move || router.clone()))
        .and(warp::any().map(move || observer.clone()))
        .and_then(handle_chat_completion_with_mpp)
}

/// Creates a warp filter for `/v1/chat/completions` with a custom [`PaymentGate`].
///
/// Like [`chat_completions_filter_with_mpp`], but accepts any
/// [`crate::mpp::PaymentGate`] implementation instead of requiring
/// [`crate::mpp::MppState`] directly. This allows downstream crates to
/// provide custom payment logic (e.g. charge-based balance management)
/// while reusing the full routing, streaming, and observation pipeline.
#[cfg(any(feature = "payments-tempo", feature = "payments-solana"))]
pub fn chat_completions_filter_with_payment_gate<T, R, A>(
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
    warp::path!("v1" / "chat" / "completions")
        .and(warp::post())
        .and(account_filter)
        .and(warp::any().map(move || payment_gate.clone()))
        .and(warp::header::optional::<String>("authorization"))
        .and(warp::header::optional::<String>("origin"))
        .and(crate::body::json::<ChatCompletionRequest>())
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
             request: ChatCompletionRequest,
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
                handle_chat_completion_with_gate(gate_ctx, request, table, router)
            },
        )
}

async fn handle_chat_completion_with_observe<T, R>(
    request: ChatCompletionRequest,
    table: Arc<T>,
    router: Arc<R>,
    observer: Arc<dyn ObserveCallback>,
    caller: CallerContext,
    fallback_policy: Arc<dyn FallbackPolicy>,
) -> Result<Box<dyn warp::Reply>, warp::Rejection>
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    let is_stream = request.stream.unwrap_or(false);
    let incoming_model = convert::extract_model_name(&request).to_owned();
    let route_ctx = openai_chat::extract(&request);

    if let Some(ref allowed) = caller.models
        && !is_model_allowed(&incoming_model, allowed)
    {
        return Err(warp::reject::custom(BitrouterRejection(
            BitrouterError::AccessDenied {
                message: format!("model '{}' is not in your allowlist", incoming_model),
            },
        )));
    }

    let chain = table
        .route_chain(&incoming_model, &route_ctx)
        .await
        .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;
    let options = convert::to_call_options(request);
    let start = Instant::now();
    let request_id = uuid::Uuid::new_v4().to_string();
    let metadata = serde_json::Value::Null;
    let mut last_err = None;
    let mut last_target = None;

    for target in chain {
        let provider_name = target.provider_name.clone();
        let target_model_id = target.service_id.clone();
        let model = router
            .route_model(target.clone())
            .await
            .map_err(|e| warp::reject::custom(BitrouterRejection(e)))?;
        let model_id = model.model_id().to_owned();

        if is_stream {
            match model.stream(options.clone()).await {
                Ok(stream_result) => {
                    return handle_stream_with_observe_result(
                        stream_result,
                        &model_id,
                        crate::router::StreamObserveContext {
                            observer,
                            route: incoming_model,
                            provider: provider_name,
                            target_model: target_model_id,
                            caller,
                            start,
                            request_id,
                            metadata,
                            executed_target: Some(target),
                        },
                    )
                    .await;
                }
                Err(e) if fallback_policy.classify(&e, &target) == FallbackDecision::Fallback => {
                    last_target = Some(target);
                    last_err = Some(e);
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
                        executed_target: Some(target),
                        error: e.clone(),
                    };
                    tokio::spawn(async move { observer.on_request_failure(event).await });
                    return Err(warp::reject::custom(BitrouterRejection(e)));
                }
            }
        } else {
            match model.generate(options.clone()).await {
                Ok(result) => {
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
                        executed_target: Some(target),
                        usage: result.usage.clone(),
                        streamed: false,
                        generation_time_ms: None,
                    };
                    tokio::spawn(async move { observer.on_request_success(event).await });
                    let response = convert::from_generate_result(&model_id, result);
                    return Ok(Box::new(warp::reply::json(&response)) as Box<dyn warp::Reply>);
                }
                Err(e) if fallback_policy.classify(&e, &target) == FallbackDecision::Fallback => {
                    last_target = Some(target);
                    last_err = Some(e);
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
                        executed_target: Some(target),
                        error: e.clone(),
                    };
                    tokio::spawn(async move { observer.on_request_failure(event).await });
                    return Err(warp::reject::custom(BitrouterRejection(e)));
                }
            }
        }
    }

    let error = last_err.unwrap_or_else(|| {
        BitrouterError::invalid_request(
            None,
            format!("no routing targets resolved for model: {incoming_model}"),
            None,
        )
    });
    let executed_target = last_target;
    let (provider, model) = executed_target
        .as_ref()
        .map(|target| (target.provider_name.clone(), target.service_id.clone()))
        .unwrap_or_else(|| ("unknown".to_owned(), "unknown".to_owned()));
    let event = RequestFailureEvent {
        ctx: RequestContext {
            route: incoming_model,
            provider,
            model,
            caller,
            latency_ms: start.elapsed().as_millis() as u64,
            request_id,
            metadata,
        },
        executed_target,
        error: error.clone(),
    };
    tokio::spawn(async move { observer.on_request_failure(event).await });
    Err(warp::reject::custom(BitrouterRejection(error)))
}

async fn handle_stream_result(
    stream_result: bitrouter_core::models::language::stream_result::LanguageModelStreamResult,
    model_id: &str,
) -> Result<Box<dyn warp::Reply>, warp::Rejection> {
    let stream_id = format!("chatcmpl-{}", generate_id());
    let model_id = model_id.to_owned();

    let (tx, rx) =
        tokio::sync::mpsc::channel::<Result<warp::sse::Event, std::convert::Infallible>>(32);

    tokio::spawn(async move {
        let mut stream = stream_result.stream;
        let mut converter = convert::StreamConverter::new(model_id, stream_id);
        use tokio_stream::StreamExt as _;
        while let Some(part) = stream.next().await {
            if let Some(chunk) = converter.convert(&part) {
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
    Ok(Box::new(crate::router::sse::reply(sse_stream)))
}

async fn handle_stream_with_observe_result(
    stream_result: bitrouter_core::models::language::stream_result::LanguageModelStreamResult,
    model_id: &str,
    ctx: crate::router::StreamObserveContext,
) -> Result<Box<dyn warp::Reply>, warp::Rejection> {
    let stream_id = format!("chatcmpl-{}", generate_id());
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
        request_id,
        metadata,
        executed_target,
    } = ctx;

    tokio::spawn(async move {
        let mut stream = stream_result.stream;
        let mut converter = convert::StreamConverter::new(model_id, stream_id);
        use tokio_stream::StreamExt as _;
        let mut observation = crate::router::StreamObservation::new();
        let mut client_disconnected = false;
        while let Some(part) = stream.next().await {
            observation.record_part(&part);
            if let Some(chunk) = converter.convert(&part) {
                let data = serde_json::to_string(&chunk).unwrap_or_default();
                let event = Ok(warp::sse::Event::default().data(data));
                if tx.send(event).await.is_err() {
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
