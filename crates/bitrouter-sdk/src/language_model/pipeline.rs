//! The `language_model` pipeline — the four-stage flight pipeline plus the
//! interleaved StreamHook stage.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use std::{future::Future, mem};

use async_trait::async_trait;
use futures::{FutureExt, StreamExt};
use futures_core::Stream;
use tracing::Instrument;

use crate::error::{BitrouterError, Result};
use crate::language_model::context::PipelineContext;
use crate::language_model::executor::{Executor, StreamPartStream};
use crate::language_model::hooks::{
    ExecutionHook, FallbackDecision, HookDecision, HopOutcome, ObserveHook, Phase, PreRequestHook,
    RequestOutcome, RouteHook, StreamHook, StreamHopOutcome,
};
use crate::language_model::routing::{FallbackPolicy, RoutingTable};
use crate::language_model::server_tools::loop_controller::{ServerToolLoop, UpstreamTurn};
use crate::language_model::server_tools::stream::UpstreamStream;
use crate::language_model::server_tools::toolset::ToolContext;
use crate::language_model::settlement::{
    DeliveryAcknowledgement, RequiredDeliveryHandshake, RequiredFinalizationContext,
    RequiredFinalizer, SettlementContext, SettlementRecorder,
};
use crate::language_model::stream::{StreamOutcome, StreamProcessor};
use crate::language_model::types::{
    ExecutionResult, PipelineRequest, PipelineResponse, Prompt, RoutingTarget, StreamPart,
};

/// The default SSE keepalive interval.
pub const DEFAULT_KEEPALIVE: Duration = Duration::from_secs(30);

struct StreamingExecution {
    stream: StreamPartStream,
    target: RoutingTarget,
    provider_started_at: Instant,
}

pub(crate) struct DeliveryPermit {
    ready: tokio::sync::oneshot::Receiver<Result<()>>,
    acknowledged: Option<tokio::sync::oneshot::Sender<DeliveryAcknowledgement>>,
}

impl DeliveryPermit {
    pub(crate) async fn deliver(mut self) -> Result<()> {
        match self.ready.await {
            Ok(Ok(())) => {
                // No await follows this send. The same poll that observes the
                // activation-ready signal authorizes the payload returned by
                // the caller immediately after this future becomes Ready.
                let acknowledged = self
                    .acknowledged
                    .take()
                    .is_some_and(|ack| ack.send(DeliveryAcknowledgement::Delivered).is_ok());
                if acknowledged {
                    Ok(())
                } else {
                    Err(BitrouterError::internal(
                        "delivery authorization closed before acknowledgment",
                    ))
                }
            }
            Ok(Err(error)) => Err(error),
            Err(error) => Err(BitrouterError::internal(format!(
                "delivery authorization ended unexpectedly: {error}"
            ))),
        }
    }

    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub(crate) async fn fail(mut self, failure: BitrouterError) -> Result<()> {
        match self.ready.await {
            Ok(Ok(())) => {
                let acknowledged = self
                    .acknowledged
                    .take()
                    .is_some_and(|ack| ack.send(DeliveryAcknowledgement::Failed(failure)).is_ok());
                if acknowledged {
                    Ok(())
                } else {
                    Err(BitrouterError::internal(
                        "delivery authorization closed before failure acknowledgment",
                    ))
                }
            }
            Ok(Err(error)) => Err(error),
            Err(error) => Err(BitrouterError::internal(format!(
                "delivery authorization ended unexpectedly: {error}"
            ))),
        }
    }
}

pub(crate) struct PreparedStreamPart {
    pub(crate) part: StreamPart,
    pub(crate) delivery: Option<DeliveryPermit>,
}

pub(crate) struct PreparedPipelineResponse {
    pub(crate) response: PipelineResponse,
    pub(crate) delivery: DeliveryPermit,
}

enum DeliveryAuthorizationOutcome {
    Delivered,
    Disconnected,
    Failed(BitrouterError),
}

fn delivery_handshake() -> (DeliveryPermit, RequiredDeliveryHandshake) {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (acknowledged_tx, acknowledged_rx) = tokio::sync::oneshot::channel();
    (
        DeliveryPermit {
            ready: ready_rx,
            acknowledged: Some(acknowledged_tx),
        },
        RequiredDeliveryHandshake::new(ready_tx, acknowledged_rx),
    )
}

struct ObservedUpstreamStream {
    inner: StreamPartStream,
    hooks: Vec<Arc<dyn ObserveHook>>,
    request_id: String,
    target: RoutingTarget,
    provider_started_at: Instant,
    terminal: bool,
}

impl ObservedUpstreamStream {
    fn notify(&mut self, outcome: StreamHopOutcome<'_>) {
        if self.terminal {
            return;
        }
        self.terminal = true;
        let duration_ms = crate::language_model::timing::elapsed_millis(self.provider_started_at);
        for hook in &self.hooks {
            let callback = std::panic::AssertUnwindSafe(|| {
                hook.on_stream_hop_end(&self.request_id, &self.target, outcome, duration_ms)
            });
            if std::panic::catch_unwind(callback).is_err() {
                tracing::warn!("ObserveHook::on_stream_hop_end panicked; swallowed");
            }
        }
    }
}

impl Stream for ObservedUpstreamStream {
    type Item = Result<StreamPart>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let item = self.inner.as_mut().poll_next(cx);
        match &item {
            std::task::Poll::Ready(None) => self.notify(StreamHopOutcome::Completed),
            std::task::Poll::Ready(Some(Err(error))) => {
                self.notify(StreamHopOutcome::Failed(error));
            }
            std::task::Poll::Ready(Some(Ok(part))) if part.is_terminal() => {
                self.notify(StreamHopOutcome::Completed);
            }
            std::task::Poll::Ready(Some(Ok(_))) | std::task::Poll::Pending => {}
        }
        item
    }
}

impl Drop for ObservedUpstreamStream {
    fn drop(&mut self) {
        self.notify(StreamHopOutcome::Dropped);
    }
}

#[derive(Clone)]
struct StreamAttempt {
    target: RoutingTarget,
    provider_started_at: Instant,
}

/// The `language_model` flight pipeline. Holds the registered hooks for every
/// stage plus the routing table, fallback policy and executor. Built via
/// [`crate::language_model::PipelineBuilder`].
pub struct Pipeline {
    pub(crate) pre_request_hooks: Vec<Arc<dyn PreRequestHook>>,
    pub(crate) route_hooks: Vec<Arc<dyn RouteHook>>,
    pub(crate) model_selectors: Vec<Arc<dyn crate::language_model::routing::ModelSelector>>,
    pub(crate) execution_hooks: Vec<Arc<dyn ExecutionHook>>,
    pub(crate) stream_hooks: Vec<Arc<dyn StreamHook>>,
    pub(crate) settlement_recorders: Vec<Arc<dyn SettlementRecorder>>,
    pub(crate) required_finalizers: Vec<Arc<dyn RequiredFinalizer>>,
    pub(crate) observe_hooks: Vec<Arc<dyn ObserveHook>>,
    pub(crate) routing_table: Arc<dyn RoutingTable>,
    pub(crate) fallback_policy: Arc<dyn FallbackPolicy>,
    pub(crate) executor: Arc<dyn Executor>,
    /// When set, non-streaming execution runs through the server-side tool loop
    /// (`server_tools`): router tools are injected, the model's calls to them
    /// are executed by BitRouter, and the upstream is re-called until the model
    /// stops calling them — all behind one caller response. `None` keeps the
    /// pipeline strictly single-shot.
    pub(crate) server_tool_loop: Option<Arc<ServerToolLoop>>,
    pub(crate) keepalive_interval: Duration,
    /// Opt-in bounded delay schedule between retryable fallback candidates.
    /// The final entry repeats for longer chains.
    pub(crate) fallback_backoff: Vec<Duration>,
    /// Detached stream-finalization tasks. Every terminal stream moves its
    /// settlement here before awaiting it, so a client disconnect cannot cancel
    /// recorders after the terminal SSE frame has already been delivered.
    /// [`Pipeline::drain_pending_settlements`] awaits them on graceful shutdown.
    pub(crate) pending_settlements: Arc<std::sync::Mutex<tokio::task::JoinSet<()>>>,
    /// Detached **non-streaming** executions ([`Pipeline::execute_detached`]).
    /// A `TaskTracker` (not the `JoinSet` above) because *every* non-streaming
    /// request runs here, so completed tasks must be reaped automatically
    /// rather than retained until shutdown. `drain_pending_settlements` closes
    /// and awaits it on graceful shutdown so a SIGTERM can't cut a request that
    /// the upstream is still billing us for.
    pub(crate) detached_executions: tokio_util::task::TaskTracker,
}

/// Adapts the pipeline's fallback execution into an [`UpstreamTurn`] so the
/// server-side tool loop can re-call the upstream for each iteration.
struct PipelineUpstream<'a> {
    pipeline: &'a Pipeline,
    chain: &'a [RoutingTarget],
    ctx: &'a PipelineContext,
}

#[async_trait]
impl UpstreamTurn for PipelineUpstream<'_> {
    async fn run(&self, prompt: &Prompt) -> Result<ExecutionResult> {
        self.pipeline
            .execute_with_fallback(self.chain, prompt, self.ctx)
            .await
    }
}

/// Drives one upstream **streaming** turn for the server-side tool loop. Each
/// iteration uses the pipeline's standard fallback policy over the routing
/// `chain` until one stream starts. A fresh throwaway [`PipelineContext`] keeps
/// per-turn fallback/observation state separate while the real request context
/// stays owned by the settlement guard.
struct PipelineStreamUpstream {
    pipeline: Arc<Pipeline>,
    chain: Vec<RoutingTarget>,
    context: PipelineContext,
    latest_attempt: SharedStreamAttempt,
}

#[async_trait]
impl UpstreamStream for PipelineStreamUpstream {
    async fn run(&self, prompt: &Prompt) -> Result<StreamPartStream> {
        let ctx = self.context.fork_for_prompt(prompt.clone());
        let execution = self
            .pipeline
            .execute_stream_with_fallback(&self.chain, &ctx)
            .await?;
        store_stream_attempt(
            &self.latest_attempt,
            StreamAttempt {
                target: execution.target,
                provider_started_at: execution.provider_started_at,
            },
        );
        Ok(execution.stream)
    }
}

type SharedStreamAttempt = Arc<std::sync::Mutex<Option<StreamAttempt>>>;

fn store_stream_attempt(slot: &SharedStreamAttempt, attempt: StreamAttempt) {
    match slot.lock() {
        Ok(mut current) => *current = Some(attempt),
        Err(poisoned) => *poisoned.into_inner() = Some(attempt),
    }
}

fn load_stream_attempt(slot: &SharedStreamAttempt) -> Option<StreamAttempt> {
    match slot.lock() {
        Ok(current) => current.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

fn sync_execution_target(ctx: &mut PipelineContext, slot: &SharedStreamAttempt) {
    let Some(attempt) = load_stream_attempt(slot) else {
        return;
    };
    ctx.set_successful_target(attempt.target.clone());
    let Some(execution) = ctx.execution_result.as_mut() else {
        return;
    };
    execution.provider_id = attempt.target.provider_name;
    execution.model_id = attempt.target.service_id;
    execution.account_label = attempt.target.account_label;
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
        // Detached non-stream execution may itself enqueue finalizer rollback
        // and delivery-observation tasks. Drain it first so those tasks are in
        // the JoinSet before we take that set below.
        let mut drained = self.detached_executions.len();
        self.detached_executions.close();
        self.detached_executions.wait().await;

        // Take the JoinSet out of the mutex so we can `await` join_next
        // without holding a sync lock across an await.
        let mut taken = {
            let mut guard = match self.pending_settlements.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            mem::take(&mut *guard)
        };
        while taken.join_next().await.is_some() {
            drained += 1;
        }

        drained
    }

    fn spawn_stream_finalization<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let future = future.instrument(tracing::Span::current());
        match self.pending_settlements.lock() {
            Ok(mut set) => {
                while set.try_join_next().is_some() {}
                set.spawn(future);
            }
            Err(poisoned) => {
                let mut set = poisoned.into_inner();
                while set.try_join_next().is_some() {}
                set.spawn(future);
            }
        }
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
        let prepared = self.execute_detached_prepared(req).await?;
        prepared.delivery.deliver().await?;
        Ok(prepared.response)
    }

    pub(crate) async fn execute_detached_prepared(
        self: Arc<Self>,
        req: PipelineRequest,
    ) -> Result<PreparedPipelineResponse> {
        let pipeline = Arc::clone(&self);
        // `tokio::spawn` does not propagate the current tracing span, so attach
        // it explicitly — otherwise the whole request's logs would detach from
        // the handler's request span / trace context.
        let span = tracing::Span::current();
        let (prepared_tx, prepared_rx) = tokio::sync::oneshot::channel();
        self.detached_executions.spawn(
            async move {
                let result = pipeline.execute_prepared(req).await;
                // A dropped handler drops the prepared delivery permit here;
                // upstream execution and settlement have nevertheless run on
                // this shutdown-tracked task.
                let _ = prepared_tx.send(result);
            }
            .instrument(span),
        );
        prepared_rx.await.map_err(|error| {
            BitrouterError::internal(format!(
                "non-streaming execution task failed to complete: {error}"
            ))
        })?
    }

    /// Execute a non-streaming request: the four stages, in order.
    pub async fn execute(&self, req: PipelineRequest) -> Result<PipelineResponse> {
        let prepared = self.execute_prepared(req).await?;
        prepared.delivery.deliver().await?;
        Ok(prepared.response)
    }

    async fn execute_prepared(&self, req: PipelineRequest) -> Result<PreparedPipelineResponse> {
        let mut ctx = PipelineContext::new(req);
        self.observe_start(&ctx).await;

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
                self.run_settlement(&mut ctx, false, Some(e.clone())).await;
                self.observe_after(Phase::Settlement, &ctx).await;
                self.observe_end(&ctx, RequestOutcome::Failed(e.clone()))
                    .await;
                return Err(e);
            }
        };
        self.observe_after(Phase::Route, &ctx).await;
        log_request_received(&ctx, chain.first(), false);

        // ---- Stage 3: execution (with the server-side tool loop when configured) ----
        let exec_outcome = match &self.server_tool_loop {
            Some(server_loop) => {
                let tool_ctx = ToolContext::from_pipeline(&ctx);
                let upstream = PipelineUpstream {
                    pipeline: self,
                    chain: &chain,
                    ctx: &ctx,
                };
                server_loop
                    .run_with_provenance(ctx.prompt(), &tool_ctx, &upstream)
                    .await
                    .map(|outcome| (outcome.result, outcome.provider_terminal_exposed))
            }
            None => self
                .execute_with_fallback(&chain, ctx.prompt(), &ctx)
                .await
                .map(|result| (result, true)),
        };
        match exec_outcome {
            Ok((result, provider_terminal_exposed)) => {
                let native_responses_terminal_invalid = provider_terminal_exposed
                    && ctx.successful_target().is_some_and(|target| {
                        target.api_protocol == crate::language_model::ApiProtocol::Responses
                    })
                    && (result
                        .result
                        .response_id
                        .as_deref()
                        .is_none_or(str::is_empty)
                        || !matches!(
                            result.result.finish_reason.as_ref(),
                            Some(
                                crate::language_model::FinishReason::Stop
                                    | crate::language_model::FinishReason::Length
                            )
                        ));
                if native_responses_terminal_invalid {
                    let error = BitrouterError::UpstreamInvalidResponse {
                        message: "native Responses result has no valid successful terminal"
                            .to_string(),
                    };
                    ctx.execution_result = Some(result);
                    self.run_settlement(&mut ctx, false, Some(error.clone()))
                        .await;
                    self.observe_after(Phase::Settlement, &ctx).await;
                    self.observe_end(&ctx, RequestOutcome::Failed(error.clone()))
                        .await;
                    return Err(error);
                }
                let native_response_completed = provider_terminal_exposed
                    && ctx.successful_target().is_some_and(|target| {
                        target.api_protocol == crate::language_model::ApiProtocol::Responses
                    })
                    && result
                        .result
                        .response_id
                        .as_deref()
                        .is_some_and(|id| !id.is_empty())
                    && matches!(
                        result.result.finish_reason.as_ref(),
                        Some(
                            crate::language_model::FinishReason::Stop
                                | crate::language_model::FinishReason::Length
                        )
                    );
                ctx.set_nonstream_native_response_completed(native_response_completed);
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
        let mut finalization = match self.prepare_required_finalizers(&ctx, false).await {
            Ok(finalization) => finalization,
            Err(error) => {
                self.run_settlement(&mut ctx, false, Some(error.clone()))
                    .await;
                self.observe_after(Phase::Settlement, &ctx).await;
                self.observe_end(&ctx, RequestOutcome::Failed(error.clone()))
                    .await;
                return Err(error);
            }
        };
        self.run_settlement(&mut ctx, false, None).await;
        self.observe_after(Phase::Settlement, &ctx).await;
        let response = ctx.response();
        let (delivery, authorization) = finalization.begin_delivery();
        let observe_hooks = self.observe_hooks.clone();
        self.spawn_stream_finalization(async move {
            let outcome = match authorization.await {
                Ok(DeliveryAuthorizationOutcome::Delivered) => RequestOutcome::Completed,
                Ok(DeliveryAuthorizationOutcome::Disconnected) => {
                    RequestOutcome::ClientDisconnected
                }
                Ok(DeliveryAuthorizationOutcome::Failed(error)) => RequestOutcome::Failed(error),
                Err(error) => RequestOutcome::Failed(BitrouterError::internal(format!(
                    "delivery authorization outcome ended unexpectedly: {error}"
                ))),
            };
            for hook in &observe_hooks {
                hook.on_request_end(&ctx, &outcome).await;
            }
        });

        Ok(PreparedPipelineResponse { response, delivery })
    }

    /// Execute a streaming request: Stages 1–3 run eagerly (so pre-stream
    /// failures are real errors), then the canonical `StreamPart` stream flows
    /// through the StreamHook stage; Settlement runs once the stream terminates.
    pub async fn execute_stream(
        self: Arc<Self>,
        req: PipelineRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamPart>> + Send>>> {
        let prepared = self.execute_stream_prepared(req).await?;
        Ok(Box::pin(prepared.then(|item| async move {
            let PreparedStreamPart { part, delivery } = item?;
            if let Some(delivery) = delivery {
                delivery.deliver().await?;
            }
            Ok(part)
        })))
    }

    pub(crate) async fn execute_stream_prepared(
        self: Arc<Self>,
        req: PipelineRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<PreparedStreamPart>> + Send>>> {
        let mut ctx = PipelineContext::new(req);
        self.observe_start(&ctx).await;

        if let Err(e) = self.run_pre_request(&mut ctx).await {
            log_request_resolve_failed(&ctx, &e);
            self.observe_end(&ctx, RequestOutcome::Failed(e.clone()))
                .await;
            return Err(e);
        }
        self.observe_after(Phase::PreRequest, &ctx).await;

        let chain = match self.resolve_route(&mut ctx).await {
            Ok(chain) => chain,
            Err(e) => {
                log_request_resolve_failed(&ctx, &e);
                self.run_settlement(&mut ctx, false, Some(e.clone())).await;
                self.observe_after(Phase::Settlement, &ctx).await;
                self.observe_end(&ctx, RequestOutcome::Failed(e.clone()))
                    .await;
                return Err(e);
            }
        };
        self.observe_after(Phase::Route, &ctx).await;
        log_request_received(&ctx, chain.first(), true);
        let latest_attempt: SharedStreamAttempt = Arc::new(std::sync::Mutex::new(None));

        // Route Stage 3 through the server-side tool loop when configured: the
        // merged stream becomes the "upstream" the settlement guard drains, so
        // settlement is unchanged. Otherwise the pipeline stays single-shot.
        let upstream_result = match &self.server_tool_loop {
            Some(server_loop) => {
                let tool_ctx = ToolContext::from_pipeline(&ctx);
                let upstream_impl: Arc<dyn UpstreamStream> = Arc::new(PipelineStreamUpstream {
                    pipeline: self.clone(),
                    chain: chain.clone(),
                    context: ctx.fork_for_prompt(ctx.prompt().clone()),
                    latest_attempt: latest_attempt.clone(),
                });
                match server_loop
                    .clone()
                    .run_stream(ctx.prompt(), &tool_ctx, upstream_impl)
                    .await
                {
                    Ok(stream) => load_stream_attempt(&latest_attempt)
                        .map(|attempt| StreamingExecution {
                            stream,
                            target: attempt.target,
                            provider_started_at: attempt.provider_started_at,
                        })
                        .ok_or_else(|| {
                            BitrouterError::internal(
                                "server-tool stream opened without a successful upstream attempt",
                            )
                        }),
                    Err(error) => Err(error),
                }
            }
            None => match self.execute_stream_with_fallback(&chain, &ctx).await {
                Ok(execution) => {
                    store_stream_attempt(
                        &latest_attempt,
                        StreamAttempt {
                            target: execution.target.clone(),
                            provider_started_at: execution.provider_started_at,
                        },
                    );
                    Ok(execution)
                }
                Err(error) => Err(error),
            },
        };
        let upstream = match upstream_result {
            Ok(upstream) => upstream,
            Err(error) => {
                self.run_settlement(&mut ctx, false, Some(error.clone()))
                    .await;
                self.observe_after(Phase::Settlement, &ctx).await;
                self.observe_end(&ctx, RequestOutcome::Failed(error.clone()))
                    .await;
                return Err(error);
            }
        };
        // A placeholder execution result so Settlement has provider/model ids;
        // usage is folded in from the StreamContext at stream end.
        ctx.set_stream_provider_started_at(upstream.provider_started_at);
        ctx.execution_result = Some(ExecutionResult {
            provider_id: upstream.target.provider_name.clone(),
            model_id: upstream.target.service_id.clone(),
            account_label: upstream.target.account_label.clone(),
            result: crate::language_model::types::GenerateResult {
                content: Vec::new(),
                usage: None,
                finish_reason: None,
                response_id: None,
                stop_details: None,
                provider_metadata: Default::default(),
            },
            request_duration_ms: 0,
            upstream_duration_ms: None,
            server_tool_calls: Vec::new(),
        });
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
            latest_attempt,
            started_required_finalizers: 0,
            state: Some(StreamDeliveryState::Pending(Box::new(
                PendingStreamDelivery {
                    processor: Some(processor),
                    ctx: Some(ctx),
                },
            ))),
        };

        Ok(Box::pin(self.drive_stream(upstream.stream, guard)))
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
    ) -> impl Stream<Item = Result<PreparedStreamPart>> + Send {
        async_stream::stream! {
            'pump: loop {
                match upstream.next().await {
                    Some(Ok(part)) => {
                        let processed = guard.processor().process_part(part).await;
                        match processed {
                            Ok(parts) => {
                                for p in parts {
                                    let delivery = if p.is_terminal() {
                                        match guard.finalize(StreamOutcome::Completed).await {
                                            Ok(delivery) => delivery,
                                            Err(error) => {
                                                yield Err(error);
                                                break 'pump;
                                            }
                                        }
                                    } else {
                                        None
                                    };
                                    let terminal = p.is_terminal();
                                    yield Ok(PreparedStreamPart { part: p, delivery });
                                    if terminal {
                                        break 'pump;
                                    }
                                }
                            }
                            Err(abort_err) => {
                                let _ = guard
                                    .finalize(StreamOutcome::Aborted(abort_err.clone()))
                                    .await;
                                yield Err(abort_err);
                                break 'pump;
                            }
                        }
                    }
                    Some(Err(e)) => {
                        let _ = guard
                            .finalize(StreamOutcome::UpstreamError(e.clone()))
                            .await;
                        yield Err(e);
                        break 'pump;
                    }
                    None => {
                        if let Err(error) = guard.finalize(StreamOutcome::Completed).await {
                            yield Err(error);
                        }
                        break 'pump;
                    }
                }
            }
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

    async fn prepare_required_finalizers(
        &self,
        ctx: &PipelineContext,
        streamed: bool,
    ) -> Result<RequiredFinalizationGuard> {
        let finalization = ctx.required_finalization_context(streamed);
        let mut guard = RequiredFinalizationGuard::new(
            self.required_finalizers.clone(),
            finalization.clone(),
            self.pending_settlements.clone(),
        );
        if !finalization.successful_terminal {
            guard.disarm();
            return Ok(guard);
        }
        guard.started = self
            .prepare_required_finalizer_tracked(&finalization)
            .await?;
        Ok(guard)
    }

    async fn prepare_required_finalizer_tracked(
        &self,
        finalization: &RequiredFinalizationContext,
    ) -> Result<usize> {
        let Some(finalizer) = self.required_finalizers.first().cloned() else {
            return Ok(0);
        };
        let task_finalization = finalization.clone();
        let (prepared_tx, prepared_rx) = tokio::sync::oneshot::channel();
        self.spawn_stream_finalization(async move {
            match finalizer.finalize(&task_finalization).await {
                Ok(()) => {
                    if prepared_tx.send(Ok(())).is_err()
                        && let Err(error) = finalizer.rollback(&task_finalization).await
                    {
                        tracing::error!(error = %error, "RequiredFinalizer rollback failed");
                    }
                }
                Err(error) => {
                    if let Err(rollback_error) = finalizer.rollback(&task_finalization).await {
                        tracing::error!(error = %rollback_error, "RequiredFinalizer rollback failed");
                    }
                    let _ = prepared_tx.send(Err(error));
                }
            }
        });
        prepared_rx.await.map_err(|error| {
            BitrouterError::internal(format!(
                "required finalizer preparation ended unexpectedly: {error}"
            ))
        })??;
        Ok(1)
    }

    async fn resolve_route(&self, ctx: &mut PipelineContext) -> Result<Vec<RoutingTarget>> {
        // Stage 0 resolves `@preset` / `:variant` exactly once. App-owned model
        // selectors then choose an effective model without losing the preset's
        // prompt defaults or routing preferences.
        let resolution = self.routing_table.resolve_model(ctx.model()).await?;
        ctx.apply_preset_overrides(&resolution.overrides);
        ctx.set_model(resolution.clean_model);
        if let Some(policy) = resolution.policy.as_deref() {
            for selector in &self.model_selectors {
                selector
                    .select_variant(policy, resolution.variant.as_deref(), ctx)
                    .await?;
            }
        }

        // Restrict the chain to providers that advertise every capability this
        // request actually uses (e.g. structured outputs). Empty for plain
        // requests, so those route unchanged.
        let mut prefs = resolution.prefs;
        prefs.require_capabilities = ctx.prompt().required_capabilities();
        // Carry the inbound protocol so the table can prefer a native,
        // same-protocol upstream for each chosen target.
        prefs.inbound_protocol = ctx.inbound_protocol();
        let mut chain = self
            .routing_table
            .route_resolved(ctx.model(), &prefs, ctx.caller())
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
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> Result<ExecutionResult> {
        let mut errors = Vec::new();
        for (attempt_index, target) in chain.iter().enumerate() {
            self.wait_before_fallback(attempt_index).await;
            self.observe_hop_start(ctx, target).await;
            let outcome = self.executor.execute(target, prompt, ctx).await;
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
                    ctx.set_successful_target(target.clone());
                    return Ok(result);
                }
                Err(e) => match self.classify_failure(ctx, &e, target).await {
                    FallbackDecision::TryNext => {
                        tracing::warn!(
                            provider = %target.provider_name,
                            model = %target.service_id,
                            error = %e,
                            "upstream route candidate failed"
                        );
                        errors.push(e);
                        continue;
                    }
                    FallbackDecision::Fail(e) => return Err(e),
                },
            }
        }
        Err(aggregate_fallback_errors(errors))
    }

    async fn execute_stream_with_fallback(
        &self,
        chain: &[RoutingTarget],
        ctx: &PipelineContext,
    ) -> Result<StreamingExecution> {
        let mut errors = Vec::new();
        for (attempt_index, target) in chain.iter().enumerate() {
            self.wait_before_fallback(attempt_index).await;
            let provider_started_at = Instant::now();
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
                Ok(stream) => {
                    ctx.set_successful_target(target.clone());
                    let stream = Box::pin(ObservedUpstreamStream {
                        inner: stream,
                        hooks: self.observe_hooks.clone(),
                        request_id: ctx.request_id().to_string(),
                        target: target.clone(),
                        provider_started_at,
                        terminal: false,
                    });
                    return Ok(StreamingExecution {
                        stream,
                        target: target.clone(),
                        provider_started_at,
                    });
                }
                Err(e) => match self.classify_failure(ctx, &e, target).await {
                    FallbackDecision::TryNext => {
                        tracing::warn!(
                            provider = %target.provider_name,
                            model = %target.service_id,
                            error = %e,
                            "upstream streaming route candidate failed"
                        );
                        errors.push(e);
                        continue;
                    }
                    FallbackDecision::Fail(e) => return Err(e),
                },
            }
        }
        Err(aggregate_fallback_errors(errors))
    }

    async fn wait_before_fallback(&self, attempt_index: usize) {
        if attempt_index == 0 || self.fallback_backoff.is_empty() {
            return;
        }
        let schedule_index = (attempt_index - 1).min(self.fallback_backoff.len() - 1);
        let delay = self.fallback_backoff[schedule_index];
        if delay.is_zero() {
            return;
        }
        tracing::debug!(
            attempt = attempt_index + 1,
            delay_ms = delay.as_millis(),
            "waiting before retryable fallback candidate"
        );
        tokio::time::sleep(delay).await;
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
        ctx.finalize_request_duration();
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

    async fn observe_start(&self, ctx: &PipelineContext) {
        for hook in &self.observe_hooks {
            let fut = std::panic::AssertUnwindSafe(hook.on_request_start(ctx));
            if fut.catch_unwind().await.is_err() {
                tracing::warn!("ObserveHook::on_request_start panicked; swallowed");
            }
        }
    }

    async fn observe_after(&self, phase: Phase, ctx: &PipelineContext) {
        for hook in &self.observe_hooks {
            let fut = std::panic::AssertUnwindSafe(hook.after_phase(phase, ctx));
            if fut.catch_unwind().await.is_err() {
                tracing::warn!(?phase, "ObserveHook::after_phase panicked; swallowed");
            }
        }
    }

    async fn observe_hop_start(&self, ctx: &PipelineContext, target: &RoutingTarget) {
        ctx.set_last_attempted_target(target.clone());
        observe_hop_start_with(&self.observe_hooks, ctx, target).await;
    }

    async fn observe_hop_end(
        &self,
        ctx: &PipelineContext,
        target: &RoutingTarget,
        outcome: HopOutcome<'_>,
    ) {
        observe_hop_end_with(&self.observe_hooks, ctx, target, outcome).await;
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

async fn observe_hop_start_with(
    hooks: &[Arc<dyn ObserveHook>],
    ctx: &PipelineContext,
    target: &RoutingTarget,
) {
    for hook in hooks {
        let fut = std::panic::AssertUnwindSafe(hook.on_hop_start(ctx, target));
        if fut.catch_unwind().await.is_err() {
            tracing::warn!("ObserveHook::on_hop_start panicked; swallowed");
        }
    }
}

async fn observe_hop_end_with(
    hooks: &[Arc<dyn ObserveHook>],
    ctx: &PipelineContext,
    target: &RoutingTarget,
    outcome: HopOutcome<'_>,
) {
    for hook in hooks {
        let fut = std::panic::AssertUnwindSafe(hook.on_hop_end(ctx, target, outcome));
        if fut.catch_unwind().await.is_err() {
            tracing::warn!("ObserveHook::on_hop_end panicked; swallowed");
        }
    }
}

/// Produce a deterministic terminal error from a fully exhausted fallback
/// chain. The result describes the route set rather than whichever provider
/// happened to sort last.
fn aggregate_fallback_errors(errors: Vec<BitrouterError>) -> BitrouterError {
    if errors.is_empty() {
        return BitrouterError::NotFound("empty routing chain".to_string());
    }

    if errors.iter().all(|error| {
        matches!(
            error,
            BitrouterError::UpstreamRateLimited { .. }
                | BitrouterError::Upstream { status: 429, .. }
        )
    }) {
        let retry_after = errors
            .iter()
            .filter_map(|error| match error {
                BitrouterError::UpstreamRateLimited { retry_after, .. } => *retry_after,
                _ => None,
            })
            .min();
        let detail = errors
            .iter()
            .find_map(BitrouterError::upstream_detail)
            .map(str::to_owned);
        return BitrouterError::UpstreamRateLimited {
            retry_after,
            detail,
        };
    }

    if errors
        .iter()
        .all(|error| matches!(error, BitrouterError::UpstreamTimeout))
    {
        return BitrouterError::UpstreamTimeout;
    }

    if errors
        .iter()
        .all(|error| matches!(error, BitrouterError::UpstreamPaymentRequired { .. }))
    {
        let detail = errors
            .iter()
            .find_map(BitrouterError::upstream_detail)
            .map(str::to_owned);
        return BitrouterError::UpstreamPaymentRequired { detail };
    }

    if errors.iter().all(|error| {
        matches!(
            error,
            BitrouterError::UpstreamBadRequest { .. }
                | BitrouterError::UpstreamPolicyViolation { .. }
        )
    }) && let Some(error) = errors.last()
    {
        return error.clone();
    }
    if errors
        .iter()
        .all(|error| matches!(error, BitrouterError::UpstreamInvalidResponse { .. }))
        && let Some(error) = errors.first()
    {
        return error.clone();
    }

    let is_availability_failure = |error: &BitrouterError| {
        matches!(
            error,
            BitrouterError::UpstreamRateLimited { .. }
                | BitrouterError::UpstreamTimeout
                | BitrouterError::UpstreamUnavailable
                | BitrouterError::Upstream { status: 408, .. }
                | BitrouterError::Upstream {
                    status: 500..=599,
                    ..
                }
        )
    };
    if errors.iter().all(is_availability_failure) {
        return BitrouterError::UpstreamUnavailable;
    }

    let is_provider_failure = |error: &BitrouterError| {
        matches!(
            error,
            BitrouterError::UpstreamPaymentRequired { .. }
                | BitrouterError::UpstreamRateLimited { .. }
                | BitrouterError::Upstream { .. }
                | BitrouterError::UpstreamAuth { .. }
                | BitrouterError::UpstreamTimeout
                | BitrouterError::UpstreamUnavailable
        )
    };
    if errors.iter().all(is_provider_failure) {
        return BitrouterError::UpstreamUnavailable;
    }

    errors
        .into_iter()
        .last()
        .unwrap_or_else(|| BitrouterError::NotFound("empty routing chain".to_string()))
}

#[cfg(test)]
mod fallback_error_tests {
    use super::aggregate_fallback_errors;
    use crate::error::BitrouterError;

    #[test]
    fn mixed_provider_failures_aggregate_to_unavailable() {
        let error = aggregate_fallback_errors(vec![
            BitrouterError::UpstreamPaymentRequired {
                detail: Some("insufficient balance".to_owned()),
            },
            BitrouterError::UpstreamRateLimited {
                retry_after: Some(30),
                detail: Some("quota exceeded".to_owned()),
            },
            BitrouterError::Upstream {
                status: 503,
                message: "service unavailable".to_owned(),
            },
        ]);

        assert!(matches!(error, BitrouterError::UpstreamUnavailable));
    }

    #[test]
    fn homogeneous_rate_limits_keep_retry_and_diagnostic() {
        let error = aggregate_fallback_errors(vec![
            BitrouterError::UpstreamRateLimited {
                retry_after: Some(60),
                detail: Some("requests exhausted".to_owned()),
            },
            BitrouterError::UpstreamRateLimited {
                retry_after: Some(15),
                detail: None,
            },
        ]);

        assert!(matches!(
            error,
            BitrouterError::UpstreamRateLimited {
                retry_after: Some(15),
                detail: Some(ref detail),
            } if detail == "requests exhausted"
        ));
    }

    #[test]
    fn homogeneous_payment_failures_keep_diagnostic() {
        let error = aggregate_fallback_errors(vec![
            BitrouterError::UpstreamPaymentRequired {
                detail: Some("insufficient credit".to_owned()),
            },
            BitrouterError::UpstreamPaymentRequired { detail: None },
        ]);

        assert!(matches!(
            error,
            BitrouterError::UpstreamPaymentRequired { detail: Some(ref detail) }
                if detail == "insufficient credit"
        ));
    }
}

/// Owns provisional non-stream success state until the response is ready to be
/// returned. Dropping `Pipeline::execute` at any await after a finalizer starts
/// schedules reverse-order rollback on the same shutdown-tracked task set used
/// by streaming settlement.
struct RequiredFinalizationGuard {
    finalizers: Vec<Arc<dyn RequiredFinalizer>>,
    finalization: RequiredFinalizationContext,
    pending_settlements: Arc<std::sync::Mutex<tokio::task::JoinSet<()>>>,
    started: usize,
    armed: bool,
}

impl RequiredFinalizationGuard {
    fn new(
        finalizers: Vec<Arc<dyn RequiredFinalizer>>,
        finalization: RequiredFinalizationContext,
        pending_settlements: Arc<std::sync::Mutex<tokio::task::JoinSet<()>>>,
    ) -> Self {
        Self {
            finalizers,
            finalization,
            pending_settlements,
            started: 0,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
        self.started = 0;
    }

    fn begin_delivery(
        &mut self,
    ) -> (
        DeliveryPermit,
        tokio::sync::oneshot::Receiver<DeliveryAuthorizationOutcome>,
    ) {
        let (permit, delivery) = delivery_handshake();
        let (outcome_tx, outcome_rx) = tokio::sync::oneshot::channel();
        let finalizers = self.finalizers.clone();
        let finalization = self.finalization.clone();
        let started = self.started;
        self.disarm();
        let authorization = async move {
            let authorization_result = if started == 0 {
                delivery.wait_for_delivery().await
            } else {
                finalizers[0].commit(&finalization, &delivery).await
            };
            let delivered = match authorization_result {
                Ok(delivered) => delivered,
                Err(error) => {
                    delivery.reject(error.clone());
                    for finalizer in finalizers.iter().take(started).rev() {
                        if let Err(rollback_error) = finalizer.rollback(&finalization).await {
                            tracing::error!(error = %rollback_error, "RequiredFinalizer rollback failed");
                        }
                    }
                    let _ = outcome_tx.send(DeliveryAuthorizationOutcome::Failed(error));
                    return;
                }
            };
            if !delivered {
                for finalizer in finalizers.iter().take(started).rev() {
                    if let Err(error) = finalizer.rollback(&finalization).await {
                        tracing::error!(error = %error, "RequiredFinalizer rollback failed");
                    }
                }
            }
            let _ = outcome_tx.send(if delivered {
                DeliveryAuthorizationOutcome::Delivered
            } else {
                DeliveryAuthorizationOutcome::Disconnected
            });
        }
        .instrument(tracing::Span::current());
        match self.pending_settlements.lock() {
            Ok(mut set) => {
                while set.try_join_next().is_some() {}
                set.spawn(authorization);
            }
            Err(poisoned) => {
                let mut set = poisoned.into_inner();
                while set.try_join_next().is_some() {}
                set.spawn(authorization);
            }
        }
        (permit, outcome_rx)
    }
}

impl Drop for RequiredFinalizationGuard {
    fn drop(&mut self) {
        if !self.armed || self.started == 0 {
            return;
        }
        let finalizers = self.finalizers.clone();
        let finalization = self.finalization.clone();
        let started = self.started;
        self.disarm();
        let rollback = async move {
            for finalizer in finalizers.iter().take(started).rev() {
                if let Err(error) = finalizer.rollback(&finalization).await {
                    tracing::error!(error = %error, "RequiredFinalizer rollback failed");
                }
            }
        }
        .instrument(tracing::Span::current());
        match self.pending_settlements.lock() {
            Ok(mut set) => {
                while set.try_join_next().is_some() {}
                set.spawn(rollback);
            }
            Err(poisoned) => {
                let mut set = poisoned.into_inner();
                while set.try_join_next().is_some() {}
                set.spawn(rollback);
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
    latest_attempt: SharedStreamAttempt,
    started_required_finalizers: usize,
    /// Owned until the terminal is ready to be returned. Cancellation at any
    /// await leaves this state available to `Drop`, which reclassifies the
    /// request as disconnected and never runs required finalizers.
    state: Option<StreamDeliveryState>,
}

struct PendingStreamDelivery {
    processor: Option<StreamProcessor>,
    ctx: Option<PipelineContext>,
}

enum StreamDeliveryState {
    Pending(Box<PendingStreamDelivery>),
    Preparing {
        disconnected: Arc<AtomicBool>,
        prepared: Arc<std::sync::Mutex<Option<PipelineContext>>>,
    },
    Ready(Box<PipelineContext>),
}

fn take_prepared_context(
    slot: &Arc<std::sync::Mutex<Option<PipelineContext>>>,
) -> Option<PipelineContext> {
    match slot.lock() {
        Ok(mut ctx) => ctx.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    }
}

impl StreamSettlementGuard {
    /// Mutable access to the in-flight processor (the stream driver feeds parts
    /// through it). Panics only if called after finalisation, which the driver
    /// never does.
    fn processor(&mut self) -> &mut StreamProcessor {
        match self
            .state
            .as_mut()
            .expect("stream guard used after finalisation")
        {
            StreamDeliveryState::Pending(state) => state
                .processor
                .as_mut()
                .expect("stream processor already absorbed"),
            StreamDeliveryState::Preparing { .. } | StreamDeliveryState::Ready(_) => {
                panic!("stream processor already preparing or absorbed")
            }
        }
    }

    async fn finish_processor(&mut self, outcome: StreamOutcome) -> Result<()> {
        let Some(StreamDeliveryState::Pending(mut pending)) = self.state.take() else {
            return Ok(());
        };
        let mut processor = pending
            .processor
            .take()
            .expect("stream processor already absorbed");
        let mut ctx = pending.ctx.take().expect("stream context already taken");
        let disconnected = Arc::new(AtomicBool::new(false));
        let task_disconnected = disconnected.clone();
        let prepared = Arc::new(std::sync::Mutex::new(None));
        let task_prepared = prepared.clone();
        let pipeline = self.pipeline.clone();
        let latest_attempt = self.latest_attempt.clone();
        let task_outcome = outcome.clone();
        let (prepared_tx, prepared_rx) = tokio::sync::oneshot::channel();
        self.pipeline.spawn_stream_finalization(async move {
            // Stream hooks and usage finalization run in one tracked task. If
            // the body poll is cancelled inside a hook await, this task keeps
            // ownership and completes each hook exactly once; it never runs a
            // required finalizer.
            processor.finish(task_outcome.clone()).await;
            sync_execution_target(&mut ctx, &latest_attempt);
            ctx.absorb_stream(processor.into_context());
            ctx.finalize_stream_upstream_duration();

            if task_disconnected.load(Ordering::Acquire)
                && matches!(task_outcome, StreamOutcome::Completed)
            {
                settle_prepared_stream(pipeline, ctx, None, RequestOutcome::ClientDisconnected)
                    .await;
                return;
            }
            match task_prepared.lock() {
                Ok(mut slot) => *slot = Some(ctx),
                Err(poisoned) => *poisoned.into_inner() = Some(ctx),
            }
            if task_disconnected.load(Ordering::Acquire)
                && matches!(task_outcome, StreamOutcome::Completed)
            {
                if let Some(ctx) = take_prepared_context(&task_prepared) {
                    settle_prepared_stream(pipeline, ctx, None, RequestOutcome::ClientDisconnected)
                        .await;
                }
                return;
            }
            if prepared_tx.send(()).is_err() {
                let Some(ctx) = take_prepared_context(&task_prepared) else {
                    return;
                };
                let (error, request_outcome) = if matches!(task_outcome, StreamOutcome::Completed) {
                    (None, RequestOutcome::ClientDisconnected)
                } else {
                    stream_terminal_metadata(&task_outcome)
                };
                settle_prepared_stream(pipeline, ctx, error, request_outcome).await;
            }
        });
        self.state = Some(StreamDeliveryState::Preparing {
            disconnected,
            prepared: prepared.clone(),
        });
        prepared_rx.await.map_err(|error| {
            BitrouterError::internal(format!(
                "stream terminal preparation ended unexpectedly: {error}"
            ))
        })?;
        let ctx = take_prepared_context(&prepared).ok_or_else(|| {
            BitrouterError::internal("stream terminal preparation returned no context")
        })?;
        self.state = Some(StreamDeliveryState::Ready(Box::new(ctx)));
        Ok(())
    }

    async fn run_required_finalizers(&mut self) -> Result<()> {
        let Some(StreamDeliveryState::Ready(ctx)) = self.state.as_ref() else {
            return Ok(());
        };
        let finalization = ctx.required_finalization_context(true);
        if !finalization.successful_terminal {
            return Ok(());
        }
        self.started_required_finalizers = self
            .pipeline
            .prepare_required_finalizer_tracked(&finalization)
            .await?;
        Ok(())
    }

    /// Finalise a provider terminal inline only through the required-finalizer
    /// boundary. Settlement is then enqueued and the terminal is returned
    /// synchronously by the same `poll_next` call.
    async fn finalize(&mut self, outcome: StreamOutcome) -> Result<Option<DeliveryPermit>> {
        if self.state.is_none() {
            return Ok(None);
        }
        self.finish_processor(outcome.clone()).await?;

        let (settlement_error, request_outcome, delivery_error) = {
            let Some(StreamDeliveryState::Ready(ctx)) = self.state.as_ref() else {
                return Ok(None);
            };
            let (error, outcome) = stream_terminal_metadata(&outcome);
            if matches!(outcome, RequestOutcome::Completed) && !ctx.stream_terminal_succeeded() {
                let error = BitrouterError::UpstreamInvalidResponse {
                    message: "stream ended with a non-success provider terminal".to_string(),
                };
                let missing_terminal = ctx
                    .required_finalization_context(true)
                    .finish_reason
                    .is_none();
                (
                    Some(error.clone()),
                    RequestOutcome::Failed(error.clone()),
                    missing_terminal.then_some(error),
                )
            } else {
                (error, outcome, None)
            }
        };

        if matches!(outcome, StreamOutcome::Completed)
            && settlement_error.is_none()
            && let Err(error) = self.run_required_finalizers().await
        {
            let Some(StreamDeliveryState::Ready(ctx)) = self.state.take() else {
                return Err(error);
            };
            let settlement_error = error.clone();
            let pipeline = self.pipeline.clone();
            let started = self.started_required_finalizers;
            self.started_required_finalizers = 0;
            self.pipeline.spawn_stream_finalization(async move {
                rollback_required_finalizers(&pipeline, &ctx, true, started).await;
                settle_prepared_stream(
                    pipeline.clone(),
                    *ctx,
                    Some(settlement_error.clone()),
                    RequestOutcome::Failed(settlement_error),
                )
                .await;
            });
            return Err(error);
        }

        let Some(StreamDeliveryState::Ready(ctx)) = self.state.take() else {
            return Ok(None);
        };
        if settlement_error.is_some() {
            let pipeline = self.pipeline.clone();
            self.pipeline.spawn_stream_finalization(async move {
                settle_prepared_stream(pipeline, *ctx, settlement_error, request_outcome).await;
            });
            if let Some(error) = delivery_error {
                return Err(error);
            }
            return Ok(None);
        }
        let pipeline = self.pipeline.clone();
        let started = self.started_required_finalizers;
        self.started_required_finalizers = 0;
        let finalization = ctx.required_finalization_context(true);
        let (permit, delivery) = delivery_handshake();
        self.pipeline.spawn_stream_finalization(async move {
            let authorization_result = if started == 0 {
                delivery.wait_for_delivery().await
            } else {
                pipeline.required_finalizers[0]
                    .commit(&finalization, &delivery)
                    .await
            };
            let delivered = match authorization_result {
                Ok(delivered) => delivered,
                Err(error) => {
                    delivery.reject(error.clone());
                    rollback_required_finalizers(&pipeline, &ctx, true, started).await;
                    settle_prepared_stream(
                        pipeline.clone(),
                        *ctx,
                        Some(error.clone()),
                        RequestOutcome::Failed(error),
                    )
                    .await;
                    return;
                }
            };
            if delivered {
                settle_prepared_stream(pipeline, *ctx, settlement_error, request_outcome).await;
            } else {
                rollback_required_finalizers(&pipeline, &ctx, true, started).await;
                settle_prepared_stream(pipeline, *ctx, None, RequestOutcome::ClientDisconnected)
                    .await;
            }
        });

        // NO AWAIT BELOW THIS POINT. The caller yields the buffered terminal
        // immediately after this Ready return, in this same `poll_next` call.
        // This transition is the delivery authorization boundary: before it,
        // Drop owns PendingDelivery and may reclassify to ClientDisconnected;
        // after it, required finalization has committed and Poll::Ready carries
        // the terminal to the consumer without another cancellation point.
        Ok(Some(permit))
    }
}

fn stream_terminal_metadata(outcome: &StreamOutcome) -> (Option<BitrouterError>, RequestOutcome) {
    match outcome {
        StreamOutcome::Completed => (None, RequestOutcome::Completed),
        StreamOutcome::ClientDisconnected => (None, RequestOutcome::ClientDisconnected),
        StreamOutcome::Aborted(err) | StreamOutcome::UpstreamError(err) => {
            (Some(err.clone()), RequestOutcome::Failed(err.clone()))
        }
    }
}

async fn rollback_required_finalizers(
    pipeline: &Pipeline,
    ctx: &PipelineContext,
    streamed: bool,
    started: usize,
) {
    if started == 0 {
        return;
    }
    let finalization = ctx.required_finalization_context(streamed);
    for finalizer in pipeline.required_finalizers.iter().take(started).rev() {
        if let Err(error) = finalizer.rollback(&finalization).await {
            tracing::error!(error = %error, "RequiredFinalizer rollback failed");
        }
    }
}

async fn settle_prepared_stream(
    pipeline: Arc<Pipeline>,
    mut ctx: PipelineContext,
    settlement_error: Option<BitrouterError>,
    request_outcome: RequestOutcome,
) {
    pipeline
        .run_settlement(&mut ctx, true, settlement_error)
        .await;
    pipeline.observe_after(Phase::Settlement, &ctx).await;
    pipeline.observe_end(&ctx, request_outcome).await;
}

async fn finalize_disconnected_stream(
    pipeline: Arc<Pipeline>,
    latest_attempt: SharedStreamAttempt,
    mut state: PendingStreamDelivery,
) {
    if let Some(processor) = state.processor.as_mut() {
        processor.finish(StreamOutcome::ClientDisconnected).await;
    }
    let Some(mut ctx) = state.ctx.take() else {
        return;
    };
    if let Some(processor) = state.processor.take() {
        sync_execution_target(&mut ctx, &latest_attempt);
        ctx.absorb_stream(processor.into_context());
        ctx.finalize_stream_upstream_duration();
    }
    settle_prepared_stream(pipeline, ctx, None, RequestOutcome::ClientDisconnected).await;
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
        match self.state.take() {
            Some(StreamDeliveryState::Pending(state)) => {
                let pipeline = self.pipeline.clone();
                let latest_attempt = self.latest_attempt.clone();
                self.pipeline.spawn_stream_finalization(async move {
                    finalize_disconnected_stream(pipeline, latest_attempt, *state).await;
                });
            }
            Some(StreamDeliveryState::Preparing {
                disconnected,
                prepared,
            }) => {
                // The tracked terminal-hook task retains processor/context
                // ownership and will settle as ClientDisconnected. It cannot
                // invoke required finalizers.
                disconnected.store(true, Ordering::Release);
                if let Some(ctx) = take_prepared_context(&prepared) {
                    let pipeline = self.pipeline.clone();
                    self.pipeline.spawn_stream_finalization(async move {
                        settle_prepared_stream(
                            pipeline,
                            ctx,
                            None,
                            RequestOutcome::ClientDisconnected,
                        )
                        .await;
                    });
                }
            }
            Some(StreamDeliveryState::Ready(ctx)) => {
                let pipeline = self.pipeline.clone();
                let started = self.started_required_finalizers;
                self.started_required_finalizers = 0;
                self.pipeline.spawn_stream_finalization(async move {
                    rollback_required_finalizers(&pipeline, &ctx, true, started).await;
                    settle_prepared_stream(
                        pipeline.clone(),
                        *ctx,
                        None,
                        RequestOutcome::ClientDisconnected,
                    )
                    .await;
                });
            }
            None => {}
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
            request_duration_ms = settle.request_duration_ms,
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
            request_duration_ms = settle.request_duration_ms,
            error = %err,
            "request finished"
        ),
    }
}

#[cfg(test)]
mod stream_outcome_tests {
    use super::{RequestOutcome, StreamOutcome, delivery_handshake, stream_terminal_metadata};
    use crate::error::BitrouterError;

    #[test]
    fn upstream_error_maps_to_failed() {
        // The fix for the streaming-error blind spot: a mid-stream upstream error
        // must reach `on_request_end` as `Failed`, not a silent `Completed`.
        let outcome = StreamOutcome::UpstreamError(BitrouterError::internal("mid-stream boom"));
        let (error, request_outcome) = stream_terminal_metadata(&outcome);
        assert!(error.is_some());
        assert!(matches!(request_outcome, RequestOutcome::Failed(_)));
    }

    #[test]
    fn abort_maps_to_failed_settlement() {
        let outcome = StreamOutcome::Aborted(BitrouterError::bad_request("blocked"));
        let (error, request_outcome) = stream_terminal_metadata(&outcome);
        assert!(error.is_some());
        assert!(matches!(request_outcome, RequestOutcome::Failed(_)));
    }

    #[test]
    fn completion_maps_to_completed_without_error() {
        let (error, request_outcome) = stream_terminal_metadata(&StreamOutcome::Completed);
        assert!(error.is_none());
        assert!(matches!(request_outcome, RequestOutcome::Completed));
    }

    #[test]
    fn disconnect_maps_to_disconnect() {
        let (error, request_outcome) = stream_terminal_metadata(&StreamOutcome::ClientDisconnected);
        assert!(error.is_none());
        assert!(matches!(
            request_outcome,
            RequestOutcome::ClientDisconnected
        ));
    }

    #[tokio::test]
    async fn explicit_wire_failure_is_not_classified_as_disconnect() {
        let (permit, delivery) = delivery_handshake();
        let authorization = tokio::spawn(async move { delivery.wait_for_delivery().await });
        permit
            .fail(BitrouterError::internal("terminal encoder failed"))
            .await
            .expect("failure acknowledgement reaches authorization worker");
        let outcome = authorization.await.expect("authorization worker joins");
        assert!(matches!(
            outcome,
            Err(BitrouterError::Internal(message)) if message == "terminal encoder failed"
        ));
    }
}
