//! The `language_model` pipeline — the four-stage flight pipeline plus the
//! interleaved StreamHook stage. See design doc 003 §3.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::{FutureExt, StreamExt};
use futures_core::Stream;

use crate::error::{BitrouterError, Result};
use crate::language_model::context::PipelineContext;
use crate::language_model::executor::{Executor, StreamPartStream};
use crate::language_model::hooks::{
    ExecutionHook, FallbackDecision, HookDecision, ObserveHook, Phase, PreRequestHook,
    RequestOutcome, RouteHook, StreamHook,
};
use crate::language_model::routing::{FallbackPolicy, RoutingPrefs, RoutingTable};
use crate::language_model::settlement::{ChargeOutcome, ChargeStrategy, SettlementRecorder};
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
    pub(crate) charge_strategies: Vec<Arc<dyn ChargeStrategy>>,
    pub(crate) settlement_recorders: Vec<Arc<dyn SettlementRecorder>>,
    pub(crate) observe_hooks: Vec<Arc<dyn ObserveHook>>,
    pub(crate) routing_table: Arc<dyn RoutingTable>,
    pub(crate) fallback_policy: Arc<dyn FallbackPolicy>,
    pub(crate) executor: Arc<dyn Executor>,
    pub(crate) keepalive_interval: Duration,
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

    /// Execute a non-streaming request: the four stages, in order.
    pub async fn execute(&self, req: PipelineRequest) -> Result<PipelineResponse> {
        let mut ctx = PipelineContext::new(req);

        // ---- Stage 1: pre-request checks ----
        if let Err(e) = self.run_pre_request(&mut ctx).await {
            self.observe_end(&ctx, RequestOutcome::Failed(e.clone()))
                .await;
            return Err(e);
        }
        self.observe_after(Phase::PreRequest, &ctx).await;

        // ---- Stage 2: route resolution ----
        let chain = match self.resolve_route(&mut ctx).await {
            Ok(chain) => chain,
            Err(e) => {
                self.observe_end(&ctx, RequestOutcome::Failed(e.clone()))
                    .await;
                return Err(e);
            }
        };
        self.observe_after(Phase::Route, &ctx).await;

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

        self.run_pre_request(&mut ctx).await?;
        self.observe_after(Phase::PreRequest, &ctx).await;

        let chain = self.resolve_route(&mut ctx).await?;
        self.observe_after(Phase::Route, &ctx).await;

        let upstream = self.execute_stream_with_fallback(&chain, &ctx).await?;
        // A placeholder execution result so Settlement has provider/model ids;
        // usage is folded in from the StreamContext at stream end.
        let head = chain.first().cloned();
        if let Some(target) = &head {
            ctx.execution_result = Some(ExecutionResult {
                provider_id: target.provider_name.clone(),
                model_id: target.service_id.clone(),
                result: crate::language_model::types::GenerateResult {
                    content: Vec::new(),
                    usage: None,
                    finish_reason: None,
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

        Ok(Box::pin(self.drive_stream(ctx, upstream, processor)))
    }

    /// The streaming driver: feeds upstream parts through the `StreamProcessor`,
    /// yields the (possibly rewritten) parts, and on termination finalises the
    /// StreamHook stage and runs Settlement. `on_stream_end` is guaranteed for
    /// every termination path.
    fn drive_stream(
        self: Arc<Self>,
        mut ctx: PipelineContext,
        mut upstream: StreamPartStream,
        mut processor: StreamProcessor,
    ) -> impl Stream<Item = Result<StreamPart>> + Send {
        async_stream::stream! {
            // Set on every break path below; the loop only exits via `break`.
            let outcome: StreamOutcome;
            'pump: loop {
                match upstream.next().await {
                    Some(Ok(part)) => {
                        let is_finish = matches!(part, StreamPart::Finish { .. });
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

            // StreamHook stage termination — `on_stream_end` fires for every hook.
            processor.finish(outcome).await;
            let stream_ctx = processor.into_context();
            ctx.absorb_stream(stream_ctx);

            // Stage 4 — Settlement runs once the stream is done.
            self.run_settlement(&mut ctx, true, None).await;
            self.observe_after(Phase::Settlement, &ctx).await;
            self.observe_end(&ctx, RequestOutcome::Completed).await;
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
        // Phase 1: default prefs. Preset/variant stripping arrives in Phase 4.
        let prefs = RoutingPrefs::default();
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
            match self.executor.execute(target, ctx.prompt()).await {
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
            match self.executor.execute_stream(target, ctx.prompt()).await {
                // Once the stream starts, the SSE response is committed — no
                // more fallback (003 §8.3).
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

    /// Decide fallback after an upstream failure. With execution hooks
    /// registered, any hook voting `Fail` fails the request; otherwise the
    /// `FallbackPolicy` decides.
    async fn classify_failure(
        &self,
        ctx: &PipelineContext,
        err: &BitrouterError,
        target: &RoutingTarget,
    ) -> FallbackDecision {
        if self.execution_hooks.is_empty() {
            return self.fallback_policy.classify(err, target);
        }
        for hook in &self.execution_hooks {
            if let FallbackDecision::Fail(e) = hook.on_failure(ctx, err).await {
                return FallbackDecision::Fail(e);
            }
        }
        FallbackDecision::TryNext
    }

    /// Stage 4 — Settlement. The `ChargeStrategy` chain is mutually exclusive
    /// (first `Claimed` `break`s); every `SettlementRecorder` always runs.
    async fn run_settlement(
        &self,
        ctx: &mut PipelineContext,
        streamed: bool,
        error: Option<BitrouterError>,
    ) {
        let mut settle = ctx.settlement_context();
        settle.streamed = streamed;
        settle.error = error;

        // 4a — charge decision: first claim wins, chain stops.
        for strategy in &self.charge_strategies {
            match strategy.try_charge(&mut settle).await {
                Ok(ChargeOutcome::Claimed) => break,
                Ok(ChargeOutcome::Pass) => continue,
                Err(e) => {
                    tracing::error!(error = %e, "ChargeStrategy failed; treating as unsettled");
                    break;
                }
            }
        }

        // 4b — bookkeeping: every recorder runs.
        for recorder in &self.settlement_recorders {
            if let Err(e) = recorder.record(&settle).await {
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

    async fn observe_end(&self, ctx: &PipelineContext, outcome: RequestOutcome) {
        for hook in &self.observe_hooks {
            let fut = std::panic::AssertUnwindSafe(hook.on_request_end(ctx, &outcome));
            if fut.catch_unwind().await.is_err() {
                tracing::warn!("ObserveHook::on_request_end panicked; swallowed");
            }
        }
    }
}
