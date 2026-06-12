//! The `language_model` pipeline — the four-stage flight pipeline plus the
//! interleaved StreamHook stage.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::{FutureExt, StreamExt};
use futures_core::Stream;
use tracing::Instrument;

use crate::error::{BitrouterError, Result};
use crate::language_model::context::PipelineContext;
use crate::language_model::executor::{Executor, StreamPartStream};
use crate::language_model::hooks::{
    ExecutionHook, FallbackDecision, HookDecision, HopOutcome, ObserveHook, Phase, PreRequestHook,
    RequestOutcome, RouteHook, StreamHook,
};
use crate::language_model::routing::{FallbackPolicy, RoutingPrefs, RoutingTable};
use crate::language_model::settlement::{SettlementContext, SettlementRecorder};
use crate::language_model::stream::{StreamOutcome, StreamProcessor};
use crate::language_model::types::{
    ExecutionResult, PipelineRequest, PipelineResponse, RoutingTarget, StreamPart,
};

/// The default SSE keepalive interval.
pub const DEFAULT_KEEPALIVE: Duration = Duration::from_secs(30);

/// The `language_model` flight pipeline. Holds the registered hooks for every
/// stage plus the routing table, fallback policy and executor. Built via
/// [`crate::language_model::PipelineBuilder`].
pub struct Pipeline {
    pub(crate) pre_request_hooks: Vec<Arc<dyn PreRequestHook>>,
    pub(crate) route_hooks: Vec<Arc<dyn RouteHook>>,
    pub(crate) execution_hooks: Vec<Arc<dyn ExecutionHook>>,
    pub(crate) stream_hooks: Vec<Arc<dyn StreamHook>>,
    pub(crate) settlement_recorders: Vec<Arc<dyn SettlementRecorder>>,
    pub(crate) observe_hooks: Vec<Arc<dyn ObserveHook>>,
    pub(crate) routing_table: Arc<dyn RoutingTable>,
    pub(crate) fallback_policy: Arc<dyn FallbackPolicy>,
    pub(crate) executor: Arc<dyn Executor>,
    pub(crate) keepalive_interval: Duration,
    /// Detached settlement tasks spawned when a streaming client disconnects
    /// (`StreamSettlementGuard::drop` —.5: no lost streaming
    /// settlement). [`Pipeline::drain_pending_settlements`] awaits them all
    /// on graceful shutdown so the process doesn't exit mid-settlement.
    pub(crate) pending_settlements: Arc<std::sync::Mutex<tokio::task::JoinSet<()>>>,
    /// Detached **non-streaming** executions ([`Pipeline::execute_detached`]).
    /// A `TaskTracker` (not the `JoinSet` above) because *every* non-streaming
    /// request runs here, so completed tasks must be reaped automatically
    /// rather than retained until shutdown. `drain_pending_settlements` closes
    /// and awaits it on graceful shutdown so a SIGTERM can't cut a request that
    /// the upstream is still billing us for.
    pub(crate) detached_executions: tokio_util::task::TaskTracker,
}

impl Pipeline {
    /// The routing table backing this pipeline.
    pub fn routing_table(&self) -> &Arc<dyn RoutingTable> {
        &self.routing_table
    }

    /// The SSE keepalive interval — used by the server's streaming path to wrap
    /// the outbound frame stream in [`crate::language_model::SseKeepaliveStream`].
    pub fn keepalive_interval(&self) -> Duration {
        self.keepalive_interval
    }

    /// Wait for every detached client-disconnect settlement task to finish.
    /// Call this from the HTTP server's graceful-shutdown path so a SIGTERM
    /// during heavy streaming traffic doesn't drop receipts.
    /// Returns the number of tasks drained.
    ///
    /// Race note: any new `StreamSettlementGuard::drop` *after* this method
    /// has swapped the JoinSet lands on a fresh empty set and is not awaited
    /// by *this* call. In practice axum's graceful shutdown awaits every
    /// in-flight handler/stream-consumer to drop before returning, so all
    /// Drops fire *before* `drain_pending_settlements` is called — the swap
    /// is correct under that contract.
    pub async fn drain_pending_settlements(&self) -> usize {
        // Take the JoinSet out of the mutex so we can `await` join_next
        // without holding a sync lock across an await.
        let mut taken = {
            let mut guard = match self.pending_settlements.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            std::mem::take(&mut *guard)
        };
        let mut drained = 0;
        while taken.join_next().await.is_some() {
            drained += 1;
        }

        // Also wait for every in-flight detached non-streaming execution
        // (`execute_detached`). `close()` only stops the tracker from blocking
        // `wait()` on tasks spawned *after* this point — already-spawned tasks
        // are still awaited. axum drains in-flight handlers before this runs,
        // so every detached execution has already been spawned.
        drained += self.detached_executions.len();
        self.detached_executions.close();
        self.detached_executions.wait().await;

        drained
    }

    /// Execute a non-streaming request that **always runs to completion**, even
    /// if the caller (an HTTP request handler) is dropped because the client
    /// disconnected.
    ///
    /// axum/hyper drops the request-handler future the instant the client
    /// disconnects, which would otherwise cancel the in-flight upstream call
    /// mid-prefill and skip Stage-4 settlement — the client pays nothing while
    /// the upstream still bills us for the request it accepted. This mirrors
    /// OpenRouter's documented behaviour: for non-streaming requests the model
    /// "will continue processing and you will be billed for the complete
    /// response" (<https://openrouter.ai/docs/api/reference/streaming>).
    ///
    /// The work is spawned onto a shutdown-tracked task, so dropping the
    /// returned future no longer cancels it. The connected caller still awaits
    /// the task's `JoinHandle` and gets the real response. Streaming requests
    /// keep their own cancel-on-disconnect path ([`Pipeline::execute_stream`]).
    pub async fn execute_detached(
        self: Arc<Self>,
        req: PipelineRequest,
    ) -> Result<PipelineResponse> {
        let pipeline = Arc::clone(&self);
        // `tokio::spawn` does not propagate the current tracing span, so attach
        // it explicitly — otherwise the whole request's logs would detach from
        // the handler's request span / trace context.
        let span = tracing::Span::current();
        let handle = self
            .detached_executions
            .spawn(async move { pipeline.execute(req).await }.instrument(span));
        match handle.await {
            Ok(result) => result,
            // The task panicked or was aborted (it is never aborted by us). The
            // settlement inside `execute` already ran or the panic precluded it;
            // surface a clean internal error to the still-connected caller.
            Err(join_err) => Err(BitrouterError::internal(format!(
                "non-streaming execution task failed to complete: {join_err}"
            ))),
        }
    }

    /// Execute a non-streaming request: the four stages, in order.
    pub async fn execute(&self, req: PipelineRequest) -> Result<PipelineResponse> {
        let mut ctx = PipelineContext::new(req);

        // ---- Stage 1: pre-request checks ----
        if let Err(e) = self.run_pre_request(&mut ctx).await {
            log_request_resolve_failed(&ctx, &e);
            self.observe_end(&ctx, RequestOutcome::Failed(e.clone()))
                .await;
            return Err(e);
        }
        self.observe_after(Phase::PreRequest, &ctx).await;

        // ---- Stage 2: route resolution ----
        let chain = match self.resolve_route(&mut ctx).await {
            Ok(chain) => chain,
            Err(e) => {
                log_request_resolve_failed(&ctx, &e);
                self.observe_end(&ctx, RequestOutcome::Failed(e.clone()))
                    .await;
                return Err(e);
            }
        };
        self.observe_after(Phase::Route, &ctx).await;
        log_request_received(&ctx, chain.first(), false);

        // ---- Stage 3: execution with fallback ----
        match self.execute_with_fallback(&chain, &ctx).await {
            Ok(result) => {
                ctx.execution_result = Some(result);
                self.observe_after(Phase::Execution, &ctx).await;
            }
            Err(e) => {
                // Settlement still runs for failed requests (records the error).
                self.run_settlement(&mut ctx, false, Some(e.clone())).await;
                self.observe_after(Phase::Settlement, &ctx).await;
                self.observe_end(&ctx, RequestOutcome::Failed(e.clone()))
                    .await;
                return Err(e);
            }
        }

        // ---- Stage 4: settlement ----
        self.run_settlement(&mut ctx, false, None).await;
        self.observe_after(Phase::Settlement, &ctx).await;
        self.observe_end(&ctx, RequestOutcome::Completed).await;

        Ok(ctx.into_response())
    }

    /// Execute a streaming request: Stages 1–3 run eagerly (so pre-stream
    /// failures are real errors), then the canonical `StreamPart` stream flows
    /// through the StreamHook stage; Settlement runs once the stream terminates.
    pub async fn execute_stream(
        self: Arc<Self>,
        req: PipelineRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamPart>> + Send>>> {
        let mut ctx = PipelineContext::new(req);

        if let Err(e) = self.run_pre_request(&mut ctx).await {
            log_request_resolve_failed(&ctx, &e);
            return Err(e);
        }
        self.observe_after(Phase::PreRequest, &ctx).await;

        let chain = match self.resolve_route(&mut ctx).await {
            Ok(chain) => chain,
            Err(e) => {
                log_request_resolve_failed(&ctx, &e);
                return Err(e);
            }
        };
        self.observe_after(Phase::Route, &ctx).await;
        log_request_received(&ctx, chain.first(), true);

        let upstream = self.execute_stream_with_fallback(&chain, &ctx).await?;
        // A placeholder execution result so Settlement has provider/model ids;
        // usage is folded in from the StreamContext at stream end.
        let head = chain.first().cloned();
        if let Some(target) = &head {
            ctx.execution_result = Some(ExecutionResult {
                provider_id: target.provider_name.clone(),
                model_id: target.service_id.clone(),
                account_label: target.account_label.clone(),
                result: crate::language_model::types::GenerateResult {
                    content: Vec::new(),
                    usage: None,
                    finish_reason: None,
                    response_id: None,
                    stop_details: None,
                    provider_metadata: Default::default(),
                },
                latency_ms: 0,
                generation_time_ms: 0,
            });
        }
        self.observe_after(Phase::Execution, &ctx).await;

        let processor = StreamProcessor::new(
            self.stream_hooks.clone(),
            self.observe_hooks.clone(),
            ctx.stream_context(),
        );

        // The guard owns the processor + context. Whatever happens to the
        // returned stream — drained to completion, errored, or **dropped early
        // by the client** — `on_stream_end` and Settlement run exactly once
        // so streaming settlement is never lost.
        let guard = StreamSettlementGuard {
            pipeline: self.clone(),
            state: Some((processor, ctx)),
        };

        Ok(Box::pin(self.drive_stream(upstream, guard)))
    }

    /// The streaming driver: feeds upstream parts through the guard's
    /// `StreamProcessor`, yields the (possibly rewritten) parts, and on a
    /// normal/errored/aborted termination finalises via the guard. If the
    /// consumer drops the stream early, the guard's `Drop` impl finalises with
    /// `ClientDisconnected` instead.
    fn drive_stream(
        self: Arc<Self>,
        mut upstream: StreamPartStream,
        mut guard: StreamSettlementGuard,
    ) -> impl Stream<Item = Result<StreamPart>> + Send {
        async_stream::stream! {
            // Set on every break path below; the loop only exits via `break`.
            let outcome: StreamOutcome;
            'pump: loop {
                let processor = guard.processor();
                match upstream.next().await {
                    Some(Ok(part)) => {
                        let is_finish = part.is_terminal();
                        match processor.process_part(part).await {
                            Ok(parts) => {
                                for p in parts {
                                    yield Ok(p);
                                }
                            }
                            Err(abort_err) => {
                                outcome = StreamOutcome::Aborted(abort_err.clone());
                                yield Err(abort_err);
                                break 'pump;
                            }
                        }
                        if is_finish {
                            outcome = StreamOutcome::Completed;
                            break 'pump;
                        }
                    }
                    Some(Err(e)) => {
                        outcome = StreamOutcome::UpstreamError(e.clone());
                        yield Err(e);
                        break 'pump;
                    }
                    None => {
                        outcome = StreamOutcome::Completed;
                        break 'pump;
                    }
                }
            }
            // Normal/errored/aborted termination — finalise inline.
            guard.finalize(outcome).await;
        }
    }

    // ===== stage helpers =====

    async fn run_pre_request(&self, ctx: &mut PipelineContext) -> Result<()> {
        for hook in &self.pre_request_hooks {
            match hook.check(ctx).await? {
                HookDecision::Allow => continue,
                HookDecision::Deny(reason) => return Err(reason.into()),
            }
        }
        Ok(())
    }

    async fn resolve_route(&self, ctx: &mut PipelineContext) -> Result<Vec<RoutingTarget>> {
        // Stage-0 preset overrides — `@careful` etc. inject a
        // system prompt / sampling defaults that the request can still
        // override. Done before the cascade so the upstream call sees them.
        let overrides = self.routing_table.preset_overrides(ctx.model()).await?;
        ctx.apply_preset_overrides(&overrides);

        // Restrict the chain to providers that advertise every capability this
        // request actually uses (e.g. structured outputs). Empty for plain
        // requests, so those route unchanged.
        let prefs = RoutingPrefs {
            require_capabilities: ctx.prompt().required_capabilities(),
            // Carry the inbound protocol so the table can prefer a native,
            // same-protocol upstream for each chosen target.
            inbound_protocol: ctx.inbound_protocol(),
            ..RoutingPrefs::default()
        };
        let mut chain = self
            .routing_table
            .route_chain(ctx.model(), &prefs, ctx.caller())
            .await?;
        for hook in &self.route_hooks {
            hook.resolve(&mut chain, ctx).await?;
        }
        if chain.is_empty() {
            return Err(BitrouterError::NotFound(format!(
                "no route for model '{}'",
                ctx.model()
            )));
        }
        ctx.route_chain = Some(chain.clone());
        Ok(chain)
    }

    async fn execute_with_fallback(
        &self,
        chain: &[RoutingTarget],
        ctx: &PipelineContext,
    ) -> Result<ExecutionResult> {
        let mut last_error: Option<BitrouterError> = None;
        for target in chain {
            self.observe_hop_start(ctx, target).await;
            let outcome = self.executor.execute(target, ctx.prompt(), ctx).await;
            match &outcome {
                Ok(result) => {
                    self.observe_hop_end(ctx, target, HopOutcome::Generated(result))
                        .await;
                }
                Err(e) => {
                    self.observe_hop_end(ctx, target, HopOutcome::Failed(e))
                        .await;
                }
            }
            match outcome {
                Ok(result) => {
                    for hook in &self.execution_hooks {
                        hook.on_success(ctx, &result).await?;
                    }
                    return Ok(result);
                }
                Err(e) => match self.classify_failure(ctx, &e, target).await {
                    FallbackDecision::TryNext => {
                        last_error = Some(e);
                        continue;
                    }
                    FallbackDecision::Fail(e) => return Err(e),
                },
            }
        }
        Err(last_error
            .unwrap_or_else(|| BitrouterError::NotFound("empty routing chain".to_string())))
    }

    async fn execute_stream_with_fallback(
        &self,
        chain: &[RoutingTarget],
        ctx: &PipelineContext,
    ) -> Result<StreamPartStream> {
        let mut last_error: Option<BitrouterError> = None;
        for target in chain {
            self.observe_hop_start(ctx, target).await;
            let outcome = self
                .executor
                .execute_stream(target, ctx.prompt(), ctx)
                .await;
            match &outcome {
                Ok(_) => {
                    self.observe_hop_end(ctx, target, HopOutcome::StreamStarted)
                        .await;
                }
                Err(e) => {
                    self.observe_hop_end(ctx, target, HopOutcome::Failed(e))
                        .await;
                }
            }
            match outcome {
                // Once the stream starts, the SSE response is committed — no
                // more fallback.
                Ok(stream) => return Ok(stream),
                Err(e) => match self.classify_failure(ctx, &e, target).await {
                    FallbackDecision::TryNext => {
                        last_error = Some(e);
                        continue;
                    }
                    FallbackDecision::Fail(e) => return Err(e),
                },
            }
        }
        Err(last_error
            .unwrap_or_else(|| BitrouterError::NotFound("empty routing chain".to_string())))
    }

    /// Decide fallback after an upstream failure. Any registered execution hook
    /// voting `Fail` short-circuits to Fail; otherwise the `FallbackPolicy`
    /// decides. A previous version of this method consulted the
    /// FallbackPolicy *only* when `execution_hooks` was empty, which silently
    /// disabled `FallbackPolicy::Fail` on 4xx upstream errors as soon as any
    /// hook (even an observe-only one) was registered.
    async fn classify_failure(
        &self,
        ctx: &PipelineContext,
        err: &BitrouterError,
        target: &RoutingTarget,
    ) -> FallbackDecision {
        for hook in &self.execution_hooks {
            if let FallbackDecision::Fail(e) = hook.on_failure(ctx, err).await {
                return FallbackDecision::Fail(e);
            }
        }
        self.fallback_policy.classify(err, target)
    }

    /// Stage 4 — Settlement. Every `SettlementRecorder` runs in registration
    /// order against the immutable [`SettlementContext`]. Deployments that
    /// need exclusive-charging semantics implement them inside a single
    /// recorder (the SDK no longer enforces a charge chain).
    async fn run_settlement(
        &self,
        ctx: &mut PipelineContext,
        streamed: bool,
        error: Option<BitrouterError>,
    ) {
        let mut settle = ctx.settlement_context();
        settle.streamed = streamed;
        settle.error = error;

        // Emit the canonical "request finished" line before recorders
        // run. Two-line model (received + finished) matches v0's
        // operator-facing log shape — see `bitrouter-observe`'s
        // `ModelSpendObserver` in the v0 tree.
        log_request_finished(&settle);

        for recorder in &self.settlement_recorders {
            if let Err(e) = recorder.record(&mut settle).await {
                tracing::error!(error = %e, "SettlementRecorder failed");
            }
        }

        ctx.absorb_settlement(settle);
    }

    // ===== observe helpers (read-only, swallow errors AND panics) =====

    async fn observe_after(&self, phase: Phase, ctx: &PipelineContext) {
        for hook in &self.observe_hooks {
            let fut = std::panic::AssertUnwindSafe(hook.after_phase(phase, ctx));
            if fut.catch_unwind().await.is_err() {
                tracing::warn!(?phase, "ObserveHook::after_phase panicked; swallowed");
            }
        }
    }

    async fn observe_hop_start(&self, ctx: &PipelineContext, target: &RoutingTarget) {
        for hook in &self.observe_hooks {
            let fut = std::panic::AssertUnwindSafe(hook.on_hop_start(ctx, target));
            if fut.catch_unwind().await.is_err() {
                tracing::warn!("ObserveHook::on_hop_start panicked; swallowed");
            }
        }
    }

    async fn observe_hop_end(
        &self,
        ctx: &PipelineContext,
        target: &RoutingTarget,
        outcome: HopOutcome<'_>,
    ) {
        for hook in &self.observe_hooks {
            let fut = std::panic::AssertUnwindSafe(hook.on_hop_end(ctx, target, outcome));
            if fut.catch_unwind().await.is_err() {
                tracing::warn!("ObserveHook::on_hop_end panicked; swallowed");
            }
        }
    }

    async fn observe_end(&self, ctx: &PipelineContext, outcome: RequestOutcome) {
        for hook in &self.observe_hooks {
            let fut = std::panic::AssertUnwindSafe(hook.on_request_end(ctx, &outcome));
            if fut.catch_unwind().await.is_err() {
                tracing::warn!("ObserveHook::on_request_end panicked; swallowed");
            }
        }
    }
}

/// Owns the streaming `StreamProcessor` + `PipelineContext` for the lifetime of
/// a streaming response, and guarantees the StreamHook stage's `on_stream_end`
/// plus Stage-4 Settlement run **exactly once** — whether the stream completes
/// normally or the client drops it early.
struct StreamSettlementGuard {
    pipeline: Arc<Pipeline>,
    /// `Some` until finalised; `take`n by `finalize` or `drop`, whichever fires
    /// first, so finalisation is exactly-once.
    state: Option<(StreamProcessor, PipelineContext)>,
}

impl StreamSettlementGuard {
    /// Mutable access to the in-flight processor (the stream driver feeds parts
    /// through it). Panics only if called after finalisation, which the driver
    /// never does.
    fn processor(&mut self) -> &mut StreamProcessor {
        &mut self
            .state
            .as_mut()
            .expect("stream guard used after finalisation")
            .0
    }

    /// Finalise inline on a normal/errored/aborted termination.
    async fn finalize(&mut self, outcome: StreamOutcome) {
        if let Some((mut processor, mut ctx)) = self.state.take() {
            processor.finish(outcome).await;
            ctx.absorb_stream(processor.into_context());
            self.pipeline.run_settlement(&mut ctx, true, None).await;
            self.pipeline.observe_after(Phase::Settlement, &ctx).await;
            self.pipeline
                .observe_end(&ctx, RequestOutcome::Completed)
                .await;
        }
    }
}

impl Drop for StreamSettlementGuard {
    fn drop(&mut self) {
        // If `state` is still `Some`, the consumer dropped the stream before it
        // terminated — settle the delivered tokens on a detached task with a
        // `ClientDisconnected` outcome so no streaming settlement is lost.
        //
        // The task is held on the pipeline's `pending_settlements` JoinSet so
        // `Pipeline::drain_pending_settlements` can await every in-flight
        // detached settlement during graceful shutdown — otherwise SIGTERM
        // could cut a settlement task mid-await and the receipt would be lost.
        if let Some((mut processor, mut ctx)) = self.state.take() {
            let pipeline = self.pipeline.clone();
            let fut = async move {
                processor.finish(StreamOutcome::ClientDisconnected).await;
                ctx.absorb_stream(processor.into_context());
                pipeline.run_settlement(&mut ctx, true, None).await;
                pipeline
                    .observe_end(&ctx, RequestOutcome::ClientDisconnected)
                    .await;
            };
            // The mutex is held only long enough to call `spawn` (which is
            // synchronous); no `.await` is held across the lock. If the lock
            // is poisoned we fall through to a bare `tokio::spawn` so the
            // settlement still runs (the unhappy case beats losing it).
            match self.pipeline.pending_settlements.lock() {
                Ok(mut set) => {
                    set.spawn(fut);
                }
                Err(_poisoned) => {
                    tokio::spawn(fut);
                }
            }
        }
    }
}

// ===== request lifecycle logging =====
//
// Two canonical INFO lines per request, mirroring v0's operator log:
//
//     "request received"  — once route resolution succeeds.
//     "request finished"  — once settlement runs (success or failure).
//
// An additional "request received (resolution failed)" line covers the
// pre-request / route-resolution failure path so the operator sees the
// request even when it never reaches a provider. Fields stay flat
// (no nested objects) so structured-log collectors can index them.

/// Emit the "request received" log line. Called from `execute` and
/// `execute_stream` after the route chain is non-empty.
fn log_request_received(ctx: &PipelineContext, head: Option<&RoutingTarget>, stream: bool) {
    let (provider, model) = head
        .map(|t| (t.provider_name.as_str(), t.service_id.as_str()))
        .unwrap_or(("-", "-"));
    // The account of the *primary* target — for a multi-account
    // provider this is the one routing will try first; failover may
    // land on a different one (see the "request finished" line).
    let account = head.and_then(|t| t.account_label.as_deref()).unwrap_or("-");
    tracing::info!(
        request_id = %ctx.request_id(),
        user_id = ctx.caller().user_id(),
        route = ctx.model(),
        provider,
        model,
        account,
        stream,
        "request received"
    );
}

/// Emit the "request received (resolution failed)" log line — the
/// pre-request or route-resolution stage rejected the request, so we
/// never made it to a provider. Recorded so a stream of `info!`s
/// always carries the request even if it doesn't have a counterpart
/// "finished" line.
fn log_request_resolve_failed(ctx: &PipelineContext, error: &BitrouterError) {
    tracing::info!(
        request_id = %ctx.request_id(),
        user_id = ctx.caller().user_id(),
        route = ctx.model(),
        error = %error,
        "request received (resolution failed)"
    );
}

/// Emit the "request finished" log line from a [`SettlementContext`].
/// Carries the canonical accounting fields — token counts, latency,
/// streamed flag — plus `status` (200 / inferred from error) so a
/// log-collector can build dashboards without parsing the message.
fn log_request_finished(settle: &SettlementContext) {
    // The account that actually served the request — for a
    // multi-account provider this reflects any failover hop, so it can
    // differ from the "request received" line's primary account.
    let account = settle.account_label.as_deref().unwrap_or("-");
    match &settle.error {
        None => tracing::info!(
            request_id = %settle.request_id,
            user_id = settle.caller.user_id(),
            provider = %settle.provider_id,
            model = %settle.model_id,
            account,
            stream = settle.streamed,
            status = 200,
            latency_ms = settle.latency_ms,
            input_tokens = settle.prompt_tokens,
            output_tokens = settle.completion_tokens,
            "request finished"
        ),
        Some(err) => tracing::info!(
            request_id = %settle.request_id,
            user_id = settle.caller.user_id(),
            provider = %settle.provider_id,
            model = %settle.model_id,
            account,
            stream = settle.streamed,
            latency_ms = settle.latency_ms,
            error = %err,
            "request finished"
        ),
    }
}
