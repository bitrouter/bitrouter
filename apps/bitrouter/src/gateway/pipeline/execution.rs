//! The `language_model` flight pipeline plus its interleaved StreamHook stage.

use crate::gateway::model::conversion::ReportAdmission;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[cfg(feature = "server-tools")]
use async_trait::async_trait;
use futures::{FutureExt, StreamExt, future::BoxFuture};
use futures_core::Stream;
use tracing::Instrument;

use crate::gateway::error::{BitrouterError, Result};
use crate::gateway::extensions::request_check::{Decision, Input};
use crate::gateway::pipeline::context::PipelineContext;
use crate::gateway::pipeline::executor::{Executor, StreamPartStream};
#[cfg(test)]
use crate::gateway::pipeline::hooks::StreamHook;
use crate::gateway::pipeline::hooks::{
    ExecutionHook, FallbackDecision, HookDecision, HopOutcome, ObserveHook, Phase, PreRequestHook,
    RequestOutcome, RouteHook, StreamHopOutcome,
};
use crate::gateway::pipeline::operations::{HookRegistration, applicable};
use crate::gateway::pipeline::request_checks::{
    CheckerFailure, CheckerFailureKind, CheckerResult, MAX_REQUEST_CHECKS_PER_ROUTER,
    RequestCheckBinding, RequestCheckerRunner, content_fragments, decision_content_fragments,
};
use crate::gateway::pipeline::routing::{FallbackPolicy, ModelResolution, RoutingTable};
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::loop_controller::{ServerToolLoop, UpstreamTurn};
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::stream::UpstreamStream;
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::toolset::ToolContext;
use crate::gateway::pipeline::settlement::{
    DeliveryAcknowledgement, RequiredDeliveryHandshake, RequiredFinalizationContext,
    RequiredFinalizationReceipt, RequiredFinalizer, SettlementContext, SettlementRecorder,
};
use bitrouter_ai::types::OperationScope;

use crate::gateway::pipeline::stream::{StreamOutcome, StreamProcessor};

use crate::gateway::pipeline::types::{
    ExecutionResult, PipelineInput, PipelineRequest, PipelineResponse, RoutingTarget,
};
use bitrouter_ai::types::{ModelOperation, Prompt, StreamPart};

/// The default SSE keepalive interval.
pub const DEFAULT_KEEPALIVE: Duration = Duration::from_secs(30);

enum ExecutionInput<'a> {
    GenerateContent(&'a super::types::generate_content::Input),
    #[cfg(feature = "server-tools")]
    Generation(&'a Prompt),
    Decisions(&'a bitrouter_ai::decisions::DecisionRequest),
}

struct StreamingExecution {
    stream: StreamPartStream,
    target: RoutingTarget,
    provider_started_at: Instant,
}

struct ResolvedRequestBinding {
    resolution: ModelResolution,
    request_checks: Vec<RequestCheckBinding>,
    resolved_selector: String,
}

struct PreparedEntry {
    ctx: PipelineContext,
    chain: Vec<RoutingTarget>,
}

struct EntryPreparationFailure {
    error: BitrouterError,
    requires_settlement: bool,
}

impl EntryPreparationFailure {
    fn pre_request(error: BitrouterError) -> Self {
        Self {
            error,
            requires_settlement: false,
        }
    }

    fn request_check(error: BitrouterError) -> Self {
        Self {
            error,
            requires_settlement: false,
        }
    }

    fn route(error: BitrouterError) -> Self {
        Self {
            error,
            requires_settlement: true,
        }
    }
}

/// Private ownership follows detached preparation and compensation without
/// adding service implementation details to RequiredFinalizationContext.
#[derive(Clone, Default)]
struct ExecutionCustody {
    #[cfg(feature = "multi-instance")]
    admission: Option<Arc<crate::gateway::shared::admitted::SharedAdmission>>,
    // These retain capacity and the external producer through delivery and
    // compensation. Receipt outcomes remain the responsibility of finalizers.
    _tenant: Option<Arc<crate::tenancy::execution::TenantExecution>>,
    _registration: Option<Arc<crate::tenancy::execution::lifecycle::Registration>>,
}

impl ExecutionCustody {
    fn from_context(ctx: &PipelineContext) -> Self {
        Self {
            #[cfg(feature = "multi-instance")]
            admission: ctx.extension(),
            _tenant: ctx.extension(),
            _registration: ctx.extension(),
        }
    }

    fn record_pipeline_outcome(&self, successful_terminal: bool) {
        #[cfg(feature = "multi-instance")]
        if let Some(admission) = &self.admission {
            admission.record_pipeline_outcome(successful_terminal);
        }
        let _ = successful_terminal;
    }

    async fn finish_delivery(&self, successful_terminal: bool) -> Result<()> {
        #[cfg(feature = "multi-instance")]
        if let Some(admission) = &self.admission {
            admission.finish_delivery(successful_terminal).await?;
        }
        if let Some(execution) = &self._tenant {
            execution
                .terminal_receipt(if successful_terminal {
                    bitrouter_ai::tenancy::execution::Outcome::Completed
                } else {
                    bitrouter_ai::tenancy::execution::Outcome::Failed
                })
                .await?;
        }
        Ok(())
    }
}

pub(crate) enum DeliveryPermit {
    Local(DeliveryState),
    Tenant(Arc<std::sync::Mutex<Option<DeliveryState>>>),
}

pub(crate) struct DeliveryState {
    ready: tokio::sync::oneshot::Receiver<Result<()>>,
    acknowledged: Option<tokio::sync::oneshot::Sender<DeliveryAcknowledgement>>,
    activation_completed: tokio::sync::oneshot::Receiver<Result<()>>,
    terminal_committed: Option<tokio::sync::oneshot::Sender<()>>,
    custody: ExecutionCustody,
    successful_terminal: bool,
    #[cfg(feature = "multi-instance")]
    terminal_observation: super::settlement::terminal::Writer,
}

impl DeliveryState {
    async fn deliver(mut self) -> Result<DeliveryReady> {
        match self.ready.await {
            Ok(Ok(())) => {
                // Activation and authoritative accounting precede delivery.
                // The terminal-commit signal and Ready return still happen in
                // one poll, so cancellation cannot expose provisional success.
                let acknowledged = self
                    .acknowledged
                    .take()
                    .is_some_and(|ack| ack.send(DeliveryAcknowledgement::Delivered).is_ok());
                if acknowledged {
                    self.activation_completed
                        .await
                        .map_err(|error| {
                            BitrouterError::internal(format!(
                                "delivery activation ended unexpectedly: {error}"
                            ))
                        })?
                        .inspect_err(|_| {
                            self.custody.record_pipeline_outcome(false);
                        })?;
                    self.custody
                        .finish_delivery(self.successful_terminal)
                        .await?;
                    Ok(DeliveryReady {
                        committed: self.terminal_committed.take().ok_or_else(|| {
                            BitrouterError::internal("delivery terminal commit was already used")
                        })?,
                        #[cfg(feature = "multi-instance")]
                        observation: self.terminal_observation,
                        #[cfg(feature = "multi-instance")]
                        successful: self.successful_terminal,
                    })
                } else {
                    Err(BitrouterError::internal(
                        "delivery authorization closed before acknowledgment",
                    ))
                }
            }
            Ok(Err(error)) => {
                self.custody.record_pipeline_outcome(false);
                Err(error)
            }
            Err(error) => Err(BitrouterError::internal(format!(
                "delivery authorization ended unexpectedly: {error}"
            ))),
        }
    }

    async fn fail_recorded(mut self, failure: BitrouterError) -> Result<DeliveryReady> {
        match self.ready.await {
            Ok(Ok(())) => {
                let acknowledged = self
                    .acknowledged
                    .take()
                    .is_some_and(|ack| ack.send(DeliveryAcknowledgement::Failed(failure)).is_ok());
                if acknowledged {
                    self.activation_completed.await.map_err(|error| {
                        BitrouterError::internal(format!(
                            "delivery compensation ended unexpectedly: {error}"
                        ))
                    })??;
                    self.custody.finish_delivery(false).await?;
                    Ok(DeliveryReady {
                        committed: self.terminal_committed.take().ok_or_else(|| {
                            BitrouterError::internal("delivery terminal commit was already used")
                        })?,
                        #[cfg(feature = "multi-instance")]
                        observation: self.terminal_observation,
                        #[cfg(feature = "multi-instance")]
                        successful: false,
                    })
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

/// Contains no request, pipeline, receipt owner or body buffers. Its final signal
/// and caller Ready return remain in one poll, including after an owned await.
struct DeliveryReady {
    committed: tokio::sync::oneshot::Sender<()>,
    #[cfg(feature = "multi-instance")]
    observation: super::settlement::terminal::Writer,
    #[cfg(feature = "multi-instance")]
    successful: bool,
}
impl DeliveryReady {
    fn commit(self) -> Result<()> {
        self.committed
            .send(())
            .map_err(|_| BitrouterError::internal("delivery terminal commit receiver closed"))?;
        #[cfg(feature = "multi-instance")]
        self.observation.complete(self.successful);
        Ok(())
    }
}

impl DeliveryPermit {
    fn new(state: DeliveryState) -> Self {
        let execution = state.custody._tenant.clone();
        match execution {
            None => Self::Local(state),
            Some(execution) => {
                let slot = Arc::new(std::sync::Mutex::new(Some(state)));
                let weak = Arc::downgrade(&slot);
                let registered = execution.receipt_owner().and_then(|owner| {
                    owner.on_cancel(Box::new(move || {
                        if let Some(slot) = weak.upgrade() {
                            let discarded = slot
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .take();
                            drop(discarded);
                        }
                    }))
                });
                if registered.is_err() {
                    let discarded = slot
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .take();
                    drop(discarded);
                    execution
                        .reject(bitrouter_ai::tenancy::operation::ErrorCode::StorageUnavailable);
                }
                Self::Tenant(slot)
            }
        }
    }
    fn take(self) -> Result<(DeliveryState, bool)> {
        match self {
            Self::Local(state) => Ok((state, false)),
            Self::Tenant(slot) => {
                let state = slot
                    .lock()
                    .map_err(|_| BitrouterError::UpstreamUnavailable)?
                    .take()
                    .ok_or(BitrouterError::UpstreamUnavailable)?;
                Ok((state, true))
            }
        }
    }
    /// Ownership moves synchronously, before the returned wait can be dropped.
    /// Tenant delivery/compensation uses the existing tracked settlement set.
    pub(crate) fn deliver(self) -> BoxFuture<'static, Result<()>> {
        self.start(None)
    }
    pub(crate) fn fail(self, error: BitrouterError) -> BoxFuture<'static, Result<()>> {
        self.start(Some(error))
    }
    fn start(self, error: Option<BitrouterError>) -> BoxFuture<'static, Result<()>> {
        let (state, owned) = match self.take() {
            Ok(state) => state,
            Err(error) => return futures::future::ready(Err(error)).boxed(),
        };
        if error.is_some() {
            // Freeze known rendering failure before returning a future that
            // can be dropped unpolled, including on the ordinary local path.
            state.custody.record_pipeline_outcome(false);
            #[cfg(feature = "multi-instance")]
            state.terminal_observation.complete(false);
        }
        let execution = state.custody._tenant.clone();
        let registration = state.custody._registration.clone();
        let completion = execution.clone();
        let work = async move {
            let result = match error {
                Some(error) => state.fail_recorded(error).await,
                None => state.deliver().await,
            };
            if result.is_err()
                && let Some(execution) = completion
            {
                execution
                    .receipt_owner()?
                    .finish_or_join(bitrouter_ai::tenancy::execution::Outcome::Failed)
                    .await
                    .map_err(|code| {
                        execution.reject(code);
                        BitrouterError::UpstreamUnavailable
                    })?;
            }
            result
        };
        if !owned {
            return async move { work.await?.commit() }.boxed();
        }
        let Some(registration) = registration else {
            return futures::future::ready(Err(BitrouterError::UpstreamUnavailable)).boxed();
        };
        let channel = execution
            .as_ref()
            .ok_or(BitrouterError::UpstreamUnavailable)
            .and_then(|execution| execution.receipt_owner())
            .and_then(crate::tenancy::receipts::output::channel);
        let (reply, mut wait) = match channel {
            Ok(channel) => channel,
            Err(error) => return futures::future::ready(Err(error)).boxed(),
        };
        let delivery_execution = execution.as_ref().map(Arc::downgrade);
        registration.spawn_delivery(async move {
            let _ = reply.send(work.await).await;
        });
        async move {
            let ready = wait
                .recv()
                .await
                .ok_or(BitrouterError::UpstreamUnavailable)??;
            // The handoff can wait after receipt commit. Recheck lifetime and
            // resource revocation, without demanding a fresh caller grant for
            // already admitted work, in this final delivery poll.
            if let Some(execution) = delivery_execution {
                let execution = execution
                    .upgrade()
                    .ok_or(BitrouterError::UpstreamUnavailable)?;
                execution
                    .check_delivery()
                    .map_err(|code| execution.reject(code))?;
            }
            ready.commit()
        }
        .boxed()
    }
}

pub(crate) struct PreparedStreamPart {
    pub(crate) part: StreamPart,
    pub(crate) delivery: Option<DeliveryPermit>,
}

pub(crate) struct PreparedPipelineResponse {
    pub(crate) response: PipelineResponse,
    pub(crate) delivery: DeliveryPermit,

    pub(crate) model_id: String,
}

struct ControlledExecution {
    result: ExecutionResult,
    output_rejection: Option<BitrouterError>,
}

pub(crate) struct PreparedPipelineStream {
    pub(crate) parts: Pin<Box<dyn Stream<Item = Result<PreparedStreamPart>> + Send>>,

    pub(crate) model_id: String,
}

enum DeliveryAuthorizationOutcome {
    Delivered,
    Disconnected,
    Failed(BitrouterError),
}

fn delivery_handshake(
    custody: ExecutionCustody,
    successful_terminal: bool,
) -> (DeliveryPermit, RequiredDeliveryHandshake) {
    custody.record_pipeline_outcome(successful_terminal);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (acknowledged_tx, acknowledged_rx) = tokio::sync::oneshot::channel();
    let (activation_completed_tx, activation_completed_rx) = tokio::sync::oneshot::channel();
    let (terminal_committed_tx, terminal_committed_rx) = tokio::sync::oneshot::channel();
    #[cfg(feature = "multi-instance")]
    let (terminal_witness, terminal_observation) = super::settlement::terminal::pair();
    #[cfg(feature = "multi-instance")]
    let delivery = RequiredDeliveryHandshake::new_with_terminal_witness(
        ready_tx,
        acknowledged_rx,
        activation_completed_tx,
        terminal_committed_rx,
        terminal_witness,
    );
    #[cfg(not(feature = "multi-instance"))]
    let delivery = RequiredDeliveryHandshake::new_with_completion(
        ready_tx,
        acknowledged_rx,
        activation_completed_tx,
        terminal_committed_rx,
    );
    (
        DeliveryPermit::new(DeliveryState {
            ready: ready_rx,
            acknowledged: Some(acknowledged_tx),
            activation_completed: activation_completed_rx,
            terminal_committed: Some(terminal_committed_tx),
            custody,
            successful_terminal,
            #[cfg(feature = "multi-instance")]
            terminal_observation,
        }),
        delivery,
    )
}

struct ObservedUpstreamStream {
    execution: Option<std::sync::Weak<crate::tenancy::execution::TenantExecution>>,
    operational: Option<Arc<crate::tenancy::operational::Hop>>,
    inner: StreamPartStream,
    hooks: Vec<Arc<dyn ObserveHook>>,
    request_id: String,
    target: RoutingTarget,
    provider_started_at: Instant,
    terminal: bool,
    technical_hop: Option<Arc<crate::tenancy::receipts::provider_technical::scope::Hop>>,
    technical: Option<Arc<crate::tenancy::receipts::provider_technical::Operation>>,
}

impl ObservedUpstreamStream {
    fn notify(&mut self, outcome: StreamHopOutcome<'_>, complete: bool) {
        if self.terminal {
            return;
        }
        self.terminal = true;
        if !matches!(outcome, StreamHopOutcome::Completed)
            && let Some(operation) = &self.technical
        {
            operation.source_end(false);
        }
        if let Some(hop) = &self.operational {
            match outcome {
                StreamHopOutcome::Failed(error) => {
                    if let Some(execution) =
                        self.execution.as_ref().and_then(std::sync::Weak::upgrade)
                    {
                        execution.operational_failed(error);
                    } else {
                        hop.dropped();
                    }
                }
                StreamHopOutcome::Dropped => hop.dropped(),
                // Only the protocol decoder, after terminal/EOF validation,
                // can mark success; wrapper EOF alone proves no such fact.
                StreamHopOutcome::Completed => {}
            }
        }
        let duration_ms =
            crate::gateway::pipeline::timing::elapsed_millis(self.provider_started_at);
        if let Some(hop) = &self.technical_hop {
            use bitrouter_ai::tenancy::provider_technical_facts::{HopEnd, HopProfile};
            let end = match outcome {
                StreamHopOutcome::Completed => HopEnd::Completed,
                StreamHopOutcome::Failed(_) => HopEnd::Failed,
                StreamHopOutcome::Dropped => HopEnd::Dropped,
            };
            hop.finish(
                HopProfile::CoreStreamHopNotifyV1,
                end,
                duration_ms,
                !complete,
            );
        }
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
            std::task::Poll::Ready(None) => {
                if !self.terminal
                    && let Some(operation) = &self.technical
                {
                    operation.source_end(false);
                }
                self.notify(StreamHopOutcome::Completed, false);
            }
            std::task::Poll::Ready(Some(Err(error))) => {
                self.notify(StreamHopOutcome::Failed(error), false);
            }
            std::task::Poll::Ready(Some(Ok(part))) => {
                // Accepted provider IR before server-tool transforms or hooks.
                // Bind its semantic clock to this exact original stream/arm set.
                if let Some(operation) = &self.technical
                    && let Some(kind) = super::timing::FirstTokenKind::from_part(part)
                {
                    operation.semantic(Instant::now(), kind);
                }
                if part.is_terminal() {
                    if let Some(operation) = &self.technical {
                        operation.source_end(super::executor::receipt::successful(part));
                    }
                    self.notify(
                        StreamHopOutcome::Completed,
                        super::executor::receipt::successful(part),
                    );
                }
            }
            std::task::Poll::Pending => {}
        }
        item
    }
}

impl Drop for ObservedUpstreamStream {
    fn drop(&mut self) {
        self.notify(StreamHopOutcome::Dropped, false);
    }
}

#[derive(Clone)]
struct StreamAttempt {
    target: RoutingTarget,
    #[cfg(feature = "server-tools")]
    provider_started_at: Instant,
}

/// The `language_model` flight pipeline. Holds the registered hooks for every
/// stage plus the routing table, fallback policy and executor. Built via
/// [`crate::gateway::pipeline::builder::PipelineBuilder`].
pub struct Pipeline {
    pub(crate) pre_resolution_hooks: Vec<HookRegistration<dyn PreRequestHook>>,
    pub(crate) router_preparation_hooks: Vec<HookRegistration<dyn PreRequestHook>>,
    pub(crate) pre_request_hooks: Vec<HookRegistration<dyn PreRequestHook>>,
    pub(crate) route_hooks: Vec<HookRegistration<dyn RouteHook>>,
    pub(crate) model_selectors:
        Vec<HookRegistration<dyn crate::gateway::pipeline::routing::ModelSelector>>,
    pub(crate) execution_hooks: Vec<HookRegistration<dyn ExecutionHook>>,
    pub(crate) decision_model: Option<crate::gateway::model::decision::DecisionRuntime>,
    #[cfg(test)]
    pub(crate) stream_hooks: Vec<Arc<dyn StreamHook>>,
    pub(crate) settlement_recorders: Vec<HookRegistration<dyn SettlementRecorder>>,
    pub(crate) required_finalizers: Vec<HookRegistration<dyn RequiredFinalizer>>,
    pub(crate) observe_hooks: Vec<HookRegistration<dyn ObserveHook>>,
    pub(crate) routing_table: Arc<dyn RoutingTable>,
    pub(crate) fallback_policy: Arc<dyn FallbackPolicy>,
    pub(crate) executor: Arc<dyn Executor>,
    /// When set, non-streaming execution runs through the server-side tool loop
    /// (`server_tools`): router tools are injected, the model's calls to them
    /// are executed by BitRouter, and the upstream is re-called until the model
    /// stops calling them — all behind one caller response. `None` keeps the
    /// pipeline strictly single-shot.
    #[cfg(feature = "server-tools")]
    pub(crate) server_tool_loop: Option<Arc<ServerToolLoop>>,
    pub(crate) keepalive_interval: Duration,
    /// Opt-in bounded delay schedule between retryable fallback candidates.
    /// The final entry repeats for longer chains.
    pub(crate) fallback_backoff: Vec<Duration>,
    /// Detached stream-finalization tasks. Every terminal stream moves its
    /// settlement here before awaiting it, so a client disconnect cannot cancel
    /// recorders after the terminal SSE frame has already been delivered.
    /// Local shutdown awaits them through
    /// [`Pipeline::drain_required_pending_settlements`]; shared generations
    /// retain them in their owned lifecycle.
    pub(super) pending_settlements: super::tasks::Settlements,
    /// Owned non-streaming execution and pre-header streaming preparation.
    /// A `TaskTracker` (not the `JoinSet` above) because *every* non-streaming
    /// request runs here, so completed tasks must be reaped automatically
    /// rather than retained until shutdown. The graceful drain methods close
    /// and await it so a SIGTERM can't cut a request that the upstream is still
    /// billing us for.
    pub(crate) detached_executions: tokio_util::task::TaskTracker,
    pub(crate) request_checker_runner: Option<Arc<dyn RequestCheckerRunner>>,
    pub(crate) served_operations: OperationScope,
}

/// Adapts the pipeline's fallback execution into an [`UpstreamTurn`] so the
/// server-side tool loop can re-call the upstream for each iteration.
#[cfg(feature = "server-tools")]
struct PipelineUpstream<'a> {
    pipeline: &'a Pipeline,
    chain: &'a [RoutingTarget],
    ctx: &'a PipelineContext,
}

#[cfg(feature = "server-tools")]
#[async_trait]
impl UpstreamTurn for PipelineUpstream<'_> {
    async fn run(&self, prompt: &Prompt) -> Result<ExecutionResult> {
        self.pipeline
            .execute_with_fallback(self.chain, ExecutionInput::Generation(prompt), self.ctx)
            .await
    }
}

/// Drives one upstream **streaming** turn for the server-side tool loop. Each
/// iteration uses the pipeline's standard fallback policy over the routing
/// `chain` until one stream starts. A fresh throwaway [`PipelineContext`] keeps
/// per-turn fallback/observation state separate while the real request context
/// stays owned by the settlement guard.
#[cfg(feature = "server-tools")]
struct PipelineStreamUpstream {
    pipeline: Arc<Pipeline>,
    chain: Vec<RoutingTarget>,
    context: PipelineContext,
    latest_attempt: SharedStreamAttempt,
}

#[cfg(feature = "server-tools")]
#[async_trait]
impl UpstreamStream for PipelineStreamUpstream {
    async fn run(&self, prompt: &Prompt) -> Result<StreamPartStream> {
        let mut ctx = self.context.fork_for_prompt(prompt.clone());
        ctx.insert_extension(Arc::new(super::server_tools::stream::ProviderToolTurn));
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
    pub(crate) fn owned_tasks(&self) -> Option<&Arc<super::owned_tasks::OwnedTasks>> {
        self.pending_settlements.owned.as_ref()
    }

    /// The routing table backing this pipeline.
    pub fn routing_table(&self) -> &Arc<dyn RoutingTable> {
        &self.routing_table
    }

    /// The SSE keepalive interval — used by the server's streaming path to wrap
    /// the outbound frame stream in [`crate::gateway::pipeline::stream::SseKeepaliveStream`].
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
    ///
    /// Shared generations require the owned lifecycle instead; this legacy
    /// best-effort method logs misuse and returns zero without touching work.
    #[cfg(test)]
    pub async fn drain_pending_settlements(&self) -> usize {
        if self.owned_tasks().is_some() {
            tracing::error!("Pipeline requires its owned lifecycle drain");
            return 0;
        }
        let drained = self.drain_detached_work().await;
        if self.drain_required_finalizers().await.is_err() {
            tracing::error!(
                reason = "required_finalizer_drain_failed",
                "RequiredFinalizer background drain failed"
            );
        }
        drained
    }

    /// Wait for detached pipeline work and require every success-critical
    /// finalizer to drain successfully.
    ///
    /// Production servers must use this error-returning shutdown contract so
    /// unresolved durable ownership cannot be mistaken for a clean exit. It is
    /// safe to retry after an error: detached work remains drained while each
    /// finalizer re-enters its retained reconciliation evidence.
    ///
    /// Shared generations reject this local-only contract: their lifecycle
    /// must retain task handles and apply one deadline to all resource owners.
    pub async fn drain_required_pending_settlements(&self) -> Result<usize> {
        if self.owned_tasks().is_some() {
            return Err(BitrouterError::internal(
                "pipeline requires its owned lifecycle drain",
            ));
        }
        let drained = self.drain_detached_work().await;
        self.drain_required_finalizers().await?;
        Ok(drained)
    }

    async fn drain_detached_work(&self) -> usize {
        // Detached non-stream execution may itself enqueue finalizer rollback
        // and delivery-observation tasks. Drain it first so those tasks are in
        // the JoinSet before we take that set below.
        let drained = self.detached_executions.len();
        self.detached_executions.close();
        self.detached_executions.wait().await;

        drained + self.pending_settlements.drain_local().await
    }

    async fn drain_required_finalizers(&self) -> Result<()> {
        for finalizer in &self.required_finalizers {
            finalizer.drain_pending_work().await?;
        }
        Ok(())
    }

    /// Drain a closed, explicitly owned pipeline without losing handles if the
    /// waiter is cancelled. Unclean joins retain their failure classification.
    pub(crate) async fn drain_owned_execution(&self) -> Result<()> {
        let tasks = self.owned_tasks().ok_or_else(|| {
            BitrouterError::internal("execution pipeline has no owned task lifecycle")
        })?;
        let summary = tasks.drain().await?;
        self.drain_required_finalizers().await?;
        // A finalizer drain may have registered compensation. Join it too and
        // preserve any late-registration failure instead of trusting a stale
        // summary collected before the finalizer was awaited.
        let final_summary = tasks.drain().await?;
        if !summary.is_clean() || !final_summary.is_clean() {
            return Err(BitrouterError::internal(
                "execution task drain was not clean",
            ));
        }
        Ok(())
    }

    fn spawn_stream_finalization<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.pending_settlements
            .spawn(future.instrument(tracing::Span::current()));
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
    /// returned future does not cancel it. The connected caller still awaits
    /// the task's `JoinHandle` and gets the real response. Streaming requests
    /// keep their own cancel-on-disconnect path ([`Pipeline::execute_stream_prepared`]).
    #[cfg(any(test, feature = "server-tools"))]
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
        self.execute_detached_prepared_with_mode(
            req,
            #[cfg(feature = "server-tools")]
            true,
        )
        .await
    }

    async fn execute_detached_prepared_with_mode(
        self: Arc<Self>,
        req: PipelineRequest,
        #[cfg(feature = "server-tools")] run_server_tools: bool,
    ) -> Result<PreparedPipelineResponse> {
        if let Some(execution) = req
            .extensions
            .get::<Arc<crate::tenancy::execution::TenantExecution>>()
            .cloned()
        {
            let registration = req
                .extensions
                .get::<Arc<crate::tenancy::execution::lifecycle::Registration>>()
                .ok_or(BitrouterError::UpstreamUnavailable)?;
            registration.validate(self.owned_tasks())?;
            registration.validate_execution(execution.id())?;
            execution.claim_preparation()?;
            let tasks = self
                .owned_tasks()
                .cloned()
                .ok_or(BitrouterError::UpstreamUnavailable)?;
            let receipt = execution.receipt_owner()?.clone();
            let (sender, mut receiver) = crate::tenancy::receipts::output::channel(&receipt)?;
            tasks.spawn_execution(async move {
                let result={let preparing=self.execute_prepared(req,#[cfg(feature="server-tools")]run_server_tools);
                    tokio::pin!(preparing);
                    tokio::select!{biased;()=receipt.cancelled.cancelled()=>{
                        #[cfg(test)]
                        eprintln!("headroom execution boundary: stage=PreparedExecution, reason=ReceiptCancelled, original_deadline_elapsed={}", execution.remaining().is_some_and(|remaining| remaining.is_zero()));
                        preparing.await
                    },()=sender.closed()=>{receipt.cancel();preparing.await},result=&mut preparing=>result}};
                let result=match result {Ok(prepared)=>Ok(prepared),Err(error)=>{
                    match execution.terminal_receipt(bitrouter_ai::tenancy::execution::Outcome::Failed).await {Ok(())=>Err(error),Err(error)=>Err(error)}
                }};
                let _=sender.send(result).await;
            }.instrument(tracing::Span::current()));
            return receiver
                .recv()
                .await
                .ok_or(BitrouterError::UpstreamUnavailable)?;
        }
        let pipeline = Arc::clone(&self);
        // `tokio::spawn` does not propagate the current tracing span, so attach
        // it explicitly — otherwise the whole request's logs would detach from
        // the handler's request span / trace context.
        let span = tracing::Span::current();
        let (prepared_tx, prepared_rx) = tokio::sync::oneshot::channel();
        let work = async move {
            let result = pipeline
                .execute_prepared(
                    req,
                    #[cfg(feature = "server-tools")]
                    run_server_tools,
                )
                .await;
            // A dropped handler drops the prepared delivery permit here;
            // upstream execution and settlement have nevertheless run on
            // this shutdown-tracked task.
            let _ = prepared_tx.send(result);
        }
        .instrument(span);
        if let Some(owner) = self.owned_tasks() {
            owner.spawn_execution(work);
        } else {
            self.detached_executions
                .spawn(super::tasks::with_telemetry_context(work));
        }
        prepared_rx.await.map_err(|error| {
            BitrouterError::internal(format!(
                "non-streaming execution task failed to complete: {error}"
            ))
        })?
    }

    /// Execute a non-streaming request: the four stages, in order.
    #[cfg(any(test, feature = "server-tools"))]
    pub async fn execute(&self, req: PipelineRequest) -> Result<PipelineResponse> {
        if req
            .extensions
            .get::<Arc<crate::tenancy::execution::TenantExecution>>()
            .is_some()
        {
            // Validation does not own a preparation or a failure proposal.
            // The detached entry claims custody only after exact registration
            // checks; the registered lifecycle owns abandoned-input cleanup.
            let pipeline = req
                .extensions
                .get::<Arc<crate::tenancy::execution::lifecycle::Registration>>()
                .ok_or(BitrouterError::UpstreamUnavailable)?
                .pipeline(self)?;
            return pipeline.execute_detached(req).await;
        }
        let prepared = self
            .execute_prepared(
                req,
                #[cfg(feature = "server-tools")]
                true,
            )
            .await?;
        prepared.delivery.deliver().await?;
        Ok(prepared.response)
    }

    async fn execute_prepared(
        &self,
        req: PipelineRequest,
        #[cfg(feature = "server-tools")] run_server_tools: bool,
    ) -> Result<PreparedPipelineResponse> {
        let PreparedEntry { mut ctx, chain } = self.prepare_entry(req, false).await?;

        // ---- Stage 3: execution (with the server-side tool loop when configured) ----
        let exec_outcome = match (ctx.input(), self) {
            #[cfg(feature = "server-tools")]
            (
                PipelineInput::Generation(prompt),
                Self {
                    server_tool_loop: Some(server_loop),
                    ..
                },
            ) if run_server_tools => {
                let tool_ctx = ToolContext::from_pipeline(&ctx);
                let upstream = PipelineUpstream {
                    pipeline: self,
                    chain: &chain,
                    ctx: &ctx,
                };
                server_loop
                    .run_with_provenance(prompt, &tool_ctx, &upstream)
                    .await
                    .map(|outcome| (outcome.result, outcome.provider_terminal_exposed, None))
            }
            (PipelineInput::Generation(prompt), _) => self
                .execute_generation_with_fallback(&chain, prompt, &ctx)
                .await
                .map(|outcome| (outcome.result, true, outcome.output_rejection)),
            (PipelineInput::GenerateContent(input), _) => self
                .execute_with_fallback(&chain, ExecutionInput::GenerateContent(input), &ctx)
                .await
                .map(|result| (result, true, None)),
            (PipelineInput::Decisions(request), _) => self
                .execute_with_fallback(&chain, ExecutionInput::Decisions(request), &ctx)
                .await
                .map(|result| ControlledExecution {
                    result,
                    output_rejection: None,
                })
                .map(|outcome| (outcome.result, true, outcome.output_rejection)),
        };
        match exec_outcome {
            Ok((result, provider_terminal_exposed, output_rejection)) => {
                if let Some(error) = output_rejection {
                    // The provider completed and may bill the entire result.
                    // Preserve its original usage for settlement, but never
                    // deliver rejected content or start a fallback attempt.
                    ctx.execution_result = Some(result);
                    self.run_settlement(&mut ctx, false, Some(error.clone()))
                        .await;
                    self.observe_after(Phase::Settlement, &ctx).await;
                    self.observe_end(&ctx, RequestOutcome::Failed(error.clone()))
                        .await;
                    return Err(error);
                }
                let response_id_valid = result
                    .result
                    .generation()
                    .and_then(|result| result.response_id.as_deref())
                    .is_some_and(|id| !id.is_empty());
                let native_responses_terminal_invalid = provider_terminal_exposed
                    && ctx.successful_target().is_some_and(|target| {
                        target.api_protocol == bitrouter_ai::types::ApiProtocol::Responses
                    })
                    && (!response_id_valid
                        || !matches!(
                            result
                                .result
                                .generation()
                                .and_then(|result| result.finish_reason.as_ref()),
                            Some(
                                bitrouter_ai::types::FinishReason::Stop
                                    | bitrouter_ai::types::FinishReason::Length
                            )
                        ));
                if native_responses_terminal_invalid {
                    let error = BitrouterError::UpstreamInvalidResponse {
                        message: "native Responses result has no valid successful terminal"
                            .to_string(),
                        usage: None,
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
                        target.api_protocol == bitrouter_ai::types::ApiProtocol::Responses
                    })
                    && response_id_valid
                    && matches!(
                        result
                            .result
                            .generation()
                            .and_then(|result| result.finish_reason.as_ref()),
                        Some(
                            bitrouter_ai::types::FinishReason::Stop
                                | bitrouter_ai::types::FinishReason::Length
                        )
                    );
                ctx.set_nonstream_native_response_completed(native_response_completed);
                ctx.execution_result = Some(result);
                if matches!(
                    ctx.execution_result
                        .as_ref()
                        .and_then(|result| result.result.generation())
                        .and_then(|result| result.finish_reason.as_ref()),
                    Some(bitrouter_ai::types::FinishReason::Error(_))
                ) {
                    ExecutionCustody::from_context(&ctx).record_pipeline_outcome(false);
                }
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
        let response = ctx.response()?;

        let model_id = ctx
            .successful_target()
            .and_then(|target| self.routing_table.canonical_model_id(ctx.model(), &target))
            .unwrap_or_else(|| ctx.model().to_owned());
        let (delivery, authorization) = finalization.begin_delivery();
        let observe_hooks = applicable(&self.observe_hooks, ctx.operation());
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

        Ok(PreparedPipelineResponse {
            response,
            delivery,

            model_id,
        })
    }

    /// Execute a streaming request: Stages 1–3 run eagerly (so pre-stream
    /// failures are real errors), then the canonical `StreamPart` stream flows
    /// through the StreamHook stage; Settlement runs once the stream terminates.
    #[cfg(test)]
    pub async fn execute_stream(
        self: Arc<Self>,
        req: PipelineRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamPart>> + Send>>> {
        let prepared = self.execute_stream_prepared(req).await?;
        Ok(Self::deliver_stream(prepared))
    }

    #[cfg(test)]
    fn deliver_stream(
        prepared: PreparedPipelineStream,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamPart>> + Send>> {
        Box::pin(prepared.parts.then(|item| async move {
            let PreparedStreamPart { part, delivery } = item?;
            if let Some(delivery) = delivery {
                delivery.deliver().await?;
            }
            Ok(part)
        }))
    }

    pub(crate) async fn execute_stream_prepared(
        self: Arc<Self>,
        req: PipelineRequest,
    ) -> Result<PreparedPipelineStream> {
        self.execute_stream_prepared_with_mode(
            req,
            #[cfg(feature = "server-tools")]
            true,
        )
        .await
    }

    async fn execute_stream_prepared_with_mode(
        self: Arc<Self>,
        req: PipelineRequest,
        #[cfg(feature = "server-tools")] run_server_tools: bool,
    ) -> Result<PreparedPipelineStream> {
        if let Some(execution) = req
            .extensions
            .get::<Arc<crate::tenancy::execution::TenantExecution>>()
            .cloned()
        {
            let registration = req
                .extensions
                .get::<Arc<crate::tenancy::execution::lifecycle::Registration>>()
                .ok_or(BitrouterError::UpstreamUnavailable)?;
            registration.validate(self.owned_tasks())?;
            registration.validate_execution(execution.id())?;
            execution.claim_preparation()?;
            let tasks = self
                .owned_tasks()
                .cloned()
                .ok_or(BitrouterError::UpstreamUnavailable)?;
            let receipt = execution.receipt_owner()?.clone();
            let (sender, mut receiver) = crate::tenancy::receipts::output::channel(&receipt)?;
            let driver_tasks = tasks.clone();
            tasks.spawn_execution(async move {
                // Cancel transport I/O, not the owning preparation future. Its
                // error path still owes settlement and external callback ACKs.
                let result={let preparing=self.prepare_stream(req,None,#[cfg(feature="server-tools")]run_server_tools);
                    tokio::pin!(preparing);
                    tokio::select!{biased;()=receipt.cancelled.cancelled()=>preparing.await,()=sender.closed()=>{receipt.cancel();preparing.await},result=&mut preparing=>result}};
                let result=match result {
                    Ok(prepared)=>Self::own_tenant_stream(prepared,execution.clone(),driver_tasks),
                    Err(error)=>match execution.terminal_receipt(bitrouter_ai::tenancy::execution::Outcome::Failed).await {Ok(())=>Err(error),Err(error)=>Err(error)},
                };
                let _=sender.send(result).await;
            }.instrument(tracing::Span::current()));
            return receiver
                .recv()
                .await
                .ok_or(BitrouterError::UpstreamUnavailable)?;
        }
        if let Some(registration) = req
            .extensions
            .get::<Arc<crate::tenancy::execution::lifecycle::Registration>>()
        {
            registration.validate(self.owned_tasks())?;
            let owner = self.owned_tasks().cloned().ok_or_else(|| {
                BitrouterError::internal("tenant stream requires owned task tracking")
            })?;
            let (prepared_tx, prepared_rx) = tokio::sync::oneshot::channel();
            let work = async move {
                let result = self
                    .prepare_stream(
                        req,
                        None,
                        #[cfg(feature = "server-tools")]
                        run_server_tools,
                    )
                    .await;
                // The request retains its registration while provider headers
                // are pending. If the handler has disappeared, destroy the
                // prepared stream here: its guard registers settlement before
                // this owned preparation task can finish. A result queued in
                // the channel retains the same custody until received/dropped.
                if let Err(undelivered) = prepared_tx.send(result) {
                    drop(undelivered);
                }
            }
            .instrument(tracing::Span::current());
            owner.spawn_execution(work);
            return prepared_rx.await.map_err(|error| {
                BitrouterError::internal(format!(
                    "tenant stream preparation task failed to complete: {error}"
                ))
            })?;
        }

        // Keep ownership from authenticated preparation through upstream headers.
        // Disconnect cancels provider work, but cannot cancel durable capture or
        // the settlement that closes it. Shutdown drains this preparation task
        // before draining any stream guard it produces.
        let pipeline = self.clone();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let (mut prepared_tx, prepared_rx) = tokio::sync::oneshot::channel();
        let work = async move {
            let preparing = pipeline.prepare_stream(
                req,
                Some(cancellation.clone()),
                #[cfg(feature = "server-tools")]
                run_server_tools,
            );
            tokio::pin!(preparing);
            let result = tokio::select! {
                result = &mut preparing => result,
                () = prepared_tx.closed() => {
                    cancellation.cancel();
                    preparing.await
                }
            };
            // An undeliverable stream drops its guard before this task exits.
            let _ = prepared_tx.send(result);
        }
        .instrument(tracing::Span::current());
        if let Some(owner) = self.owned_tasks() {
            owner.spawn_execution(work);
        } else {
            self.detached_executions
                .spawn(super::tasks::with_telemetry_context(work));
        }
        prepared_rx.await.map_err(|error| {
            BitrouterError::internal(format!(
                "stream preparation task failed to complete: {error}"
            ))
        })?
    }

    fn own_tenant_stream(
        prepared: PreparedPipelineStream,
        execution: Arc<crate::tenancy::execution::TenantExecution>,
        tasks: Arc<super::owned_tasks::OwnedTasks>,
    ) -> Result<PreparedPipelineStream> {
        let receipt = execution.receipt_owner()?.clone();
        let (sender, mut receiver) = crate::tenancy::receipts::output::channel(&receipt)?;
        let PreparedPipelineStream {
            mut parts,
            model_id,
        } = prepared;
        tasks.spawn_execution(async move {
            loop {
                let item=tokio::select!{biased;()=receipt.cancelled.cancelled()=>break,()=sender.closed()=>break,item=parts.next()=>item};
                let Some(mut item)=item else{break};
                if let Ok(part)=&item && let Err(error)=crate::tenancy::receipts::output::bounded_part(&part.part) {item=Err(error);}
                if item.is_err() {
                    // Destroy upstream custody before waiting for its journal.
                    drop(parts);
                    if let Err(error)=execution.terminal_receipt(bitrouter_ai::tenancy::execution::Outcome::Failed).await {item=Err(error);}
                    let _=sender.send(item).await;
                    return;
                }
                if sender.send(item).await.is_err(){break;}
            }
            // Unpolled/disconnected receivers never retain this stream/guard.
            drop(parts);
        }.instrument(tracing::Span::current()));
        let parts = Box::pin(
            async_stream::stream! {while let Some(item)=receiver.recv().await {yield item;}},
        );
        Ok(PreparedPipelineStream { parts, model_id })
    }

    async fn prepare_stream(
        self: Arc<Self>,
        req: PipelineRequest,
        cancellation: Option<tokio_util::sync::CancellationToken>,
        #[cfg(feature = "server-tools")] run_server_tools: bool,
    ) -> Result<PreparedPipelineStream> {
        let PreparedEntry { mut ctx, chain } = self.prepare_entry(req, true).await?;
        let latest_attempt: SharedStreamAttempt = Arc::new(std::sync::Mutex::new(None));

        // Route Stage 3 through the server-side tool loop when configured: the
        // merged stream becomes the "upstream" the settlement guard drains, so
        // settlement is unchanged. Otherwise the pipeline stays single-shot.
        let upstream = async {
            match self.as_ref() {
                #[cfg(feature = "server-tools")]
                Self {
                    server_tool_loop: Some(server_loop),
                    ..
                } if run_server_tools => {
                    let tool_ctx = ToolContext::from_pipeline(&ctx);
                    let upstream_impl: Arc<dyn UpstreamStream> = Arc::new(PipelineStreamUpstream {
                        pipeline: self.clone(),
                        chain: chain.clone(),
                        context: ctx.fork_for_prompt(ctx.require_generation_prompt()?.clone()),
                        latest_attempt: latest_attempt.clone(),
                    });
                    match server_loop
                    .clone()
                    .run_stream(ctx.require_generation_prompt()?, &tool_ctx, upstream_impl)
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
                _ => match self.execute_stream_with_fallback(&chain, &ctx).await {
                    Ok(execution) => {
                        store_stream_attempt(
                            &latest_attempt,
                            StreamAttempt {
                                target: execution.target.clone(),
                                #[cfg(feature = "server-tools")]
                                provider_started_at: execution.provider_started_at,
                            },
                        );
                        Ok(execution)
                    }
                    Err(error) => Err(error),
                },
            }
        };
        let upstream_result = if let Some(cancellation) = cancellation {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => Err(BitrouterError::Cancelled),
                result = upstream => result,
            }
        } else {
            upstream.await
        };
        let upstream = match upstream_result {
            Ok(upstream) => upstream,
            Err(error) => {
                self.run_settlement(&mut ctx, false, Some(error.clone()))
                    .await;
                self.observe_after(Phase::Settlement, &ctx).await;
                let outcome = if matches!(error, BitrouterError::Cancelled) {
                    RequestOutcome::ClientDisconnected
                } else {
                    RequestOutcome::Failed(error.clone())
                };
                self.observe_end(&ctx, outcome).await;
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
            result: bitrouter_ai::types::GenerateResult {
                content: Vec::new(),
                usage: None,
                finish_reason: None,
                response_id: None,
                stop_details: None,
                provider_metadata: Default::default(),
            }
            .into(),
            request_duration_ms: 0,
            upstream_duration_ms: None,
            #[cfg(feature = "server-tools")]
            server_tool_calls: Vec::new(),
        });
        self.observe_after(Phase::Execution, &ctx).await;

        let mut stream_context = ctx.stream_context();
        let pricing = self.stream_pricing(&ctx, &upstream.target);
        stream_context.accumulated_usage.set_pricing(pricing);
        let processor = StreamProcessor::new(
            #[cfg(test)]
            self.stream_hooks.clone(),
            applicable(&self.observe_hooks, ModelOperation::Generation),
            stream_context,
        );

        // The guard owns the processor + context. Whatever happens to the
        // returned stream — drained to completion, errored, or **dropped early
        // by the client** — `on_stream_end` and Settlement run exactly once
        // so streaming settlement is never lost.

        let model_id = self
            .routing_table
            .canonical_model_id(ctx.model(), &upstream.target)
            .unwrap_or_else(|| ctx.model().to_owned());
        let guard = StreamSettlementGuard {
            pipeline: self.clone(),
            latest_attempt,
            required_finalizer_receipt: None,
            state: Some(StreamDeliveryState::Pending(Box::new(
                PendingStreamDelivery {
                    processor: Some(processor),
                    ctx: Some(ctx),
                },
            ))),
        };

        Ok(PreparedPipelineStream {
            parts: Box::pin(self.drive_stream(upstream.stream, guard)),

            model_id,
        })
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
                        let processed = match guard.processor() {
                            Ok(processor) => processor.process_part(part).await,
                            Err(error) => Err(error),
                        };
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

    async fn prepare_entry(&self, req: PipelineRequest, streamed: bool) -> Result<PreparedEntry> {
        if !self.served_operations.contains(req.input.operation()) {
            return Err(BitrouterError::bad_request(
                "pipeline does not serve this model operation",
            ));
        }
        if streamed && req.input.operation() != ModelOperation::Generation {
            return Err(BitrouterError::bad_request(
                "Decisions does not support streaming",
            ));
        }
        if req
            .inbound_protocol
            .as_ref()
            .is_some_and(|protocol| protocol.operation() != req.input.operation())
        {
            return Err(BitrouterError::bad_request(
                "endpoint protocol does not match model operation",
            ));
        }
        let mut ctx = PipelineContext::new(req);
        if let Some(registration) =
            ctx.extension::<crate::tenancy::execution::lifecycle::Registration>()
        {
            registration.validate(self.owned_tasks())?;
        } else if ctx
            .extension::<crate::tenancy::execution::TenantExecution>()
            .is_some()
        {
            return Err(BitrouterError::internal(
                "tenant execution has no lifecycle custody",
            ));
        }
        self.observe_start(&ctx).await;

        let prepared = self.prepare_entry_stages(&mut ctx).await;
        match prepared {
            Ok(chain) => {
                self.observe_after(Phase::Route, &ctx).await;
                log_request_received(&ctx, chain.first(), streamed);
                Ok(PreparedEntry { ctx, chain })
            }
            Err(failure) => {
                log_request_resolve_failed(&ctx, &failure.error);
                if failure.requires_settlement
                    || ctx
                        .extension::<crate::trajectory::ingress::CapturedRequest>()
                        .is_some()
                {
                    self.run_settlement(&mut ctx, false, Some(failure.error.clone()))
                        .await;
                    self.observe_after(Phase::Settlement, &ctx).await;
                }
                self.observe_end(&ctx, RequestOutcome::Failed(failure.error.clone()))
                    .await;
                Err(failure.error)
            }
        }
    }

    async fn prepare_entry_stages(
        &self,
        ctx: &mut PipelineContext,
    ) -> std::result::Result<Vec<RoutingTarget>, EntryPreparationFailure> {
        // Local auth/session/continuation normalization must finish before any
        // configured checker can cause external egress.
        self.run_pre_resolution(ctx)
            .await
            .map_err(EntryPreparationFailure::pre_request)?;

        // Freeze the ingress router identity and checker bindings before an
        // app hook can choose a different effective route.
        let mut binding = self
            .resolve_binding(ctx)
            .await
            .map_err(EntryPreparationFailure::route)?;
        self.validate_binding_operation(ctx, &binding)
            .map_err(EntryPreparationFailure::pre_request)?;
        self.run_router_preparation(ctx)
            .await
            .map_err(EntryPreparationFailure::pre_request)?;

        if binding.request_checks.is_empty() {
            // Compatibility path: before named-router request checks existed,
            // ordinary pre-request hooks could rewrite a bare/legacy selector
            // before its defaults were resolved. Preserve that behavior for
            // unguarded requests so only the final selector contributes
            // defaults, preferences, and policy.
            self.run_pre_request(ctx, None)
                .await
                .map_err(EntryPreparationFailure::pre_request)?;
            binding = self
                .resolve_effective_binding(ctx, binding)
                .await
                .map_err(EntryPreparationFailure::route)?;
        } else {
            // Checked routers resolve preparation rewrites and defaults before
            // local policy/guardrail hooks. The effective selector is then
            // immutable: changing it here would either mix two sets of
            // defaults or check content for a route that will not be served.
            binding = self
                .resolve_effective_binding(ctx, binding)
                .await
                .map_err(EntryPreparationFailure::route)?;
            let checked_selector = binding.resolved_selector.clone();
            self.run_pre_request(ctx, Some(&checked_selector))
                .await
                .map_err(EntryPreparationFailure::pre_request)?;
        }

        self.run_request_checks(ctx, &binding.request_checks)
            .await
            .map_err(EntryPreparationFailure::request_check)?;
        self.observe_after(Phase::PreRequest, ctx).await;

        self.resolve_route(ctx, binding)
            .await
            .map_err(EntryPreparationFailure::route)
    }

    async fn run_pre_resolution(&self, ctx: &mut PipelineContext) -> Result<()> {
        self.run_preparation_hooks(ctx, &self.pre_resolution_hooks, None)
            .await
    }

    async fn run_pre_request(
        &self,
        ctx: &mut PipelineContext,
        checked_selector: Option<&str>,
    ) -> Result<()> {
        self.run_preparation_hooks(ctx, &self.pre_request_hooks, checked_selector)
            .await
    }

    async fn run_router_preparation(&self, ctx: &mut PipelineContext) -> Result<()> {
        self.run_preparation_hooks(ctx, &self.router_preparation_hooks, None)
            .await
    }

    async fn run_preparation_hooks(
        &self,
        ctx: &mut PipelineContext,
        hooks: &[HookRegistration<dyn PreRequestHook>],
        checked_selector: Option<&str>,
    ) -> Result<()> {
        let operation = ctx.operation();
        for hook in hooks.iter().filter(|hook| hook.supports(operation)) {
            match hook.check(ctx).await? {
                HookDecision::Allow => {
                    if checked_selector.is_some_and(|selector| ctx.model() != selector) {
                        return Err(BitrouterError::internal(
                            "a pre-request hook cannot change a checked router selector; register selector rewrites as router preparation hooks",
                        ));
                    }
                }
                HookDecision::Deny(reason) => return Err(reason.into()),
            }
        }
        Ok(())
    }

    async fn resolve_binding(&self, ctx: &mut PipelineContext) -> Result<ResolvedRequestBinding> {
        let resolved_selector = ctx.model().to_owned();
        let mut resolution = self.routing_table.resolve_model(ctx.model()).await?;
        if resolution.request_checks.len() > MAX_REQUEST_CHECKS_PER_ROUTER {
            return Err(BitrouterError::internal(
                "named router exceeds the maximum of 16 request checks",
            ));
        }
        if let Some(mut identity) = resolution.router.take() {
            // Preserve the documented exact ingress selector in public router
            // identity. `resolved_selector` separately retains the
            // post-session value used for this resolution.
            identity.original_selector = ctx.original_model().to_owned();
            ctx.set_router_identity(identity.clone());
            ctx.emit(identity);
        } else if !resolution.request_checks.is_empty() {
            return Err(BitrouterError::internal(
                "request checks require a named-router binding",
            ));
        }
        let request_checks = resolution.request_checks.clone();
        Ok(ResolvedRequestBinding {
            resolution,
            request_checks,
            resolved_selector,
        })
    }

    async fn resolve_effective_binding(
        &self,
        ctx: &mut PipelineContext,
        mut binding: ResolvedRequestBinding,
    ) -> Result<ResolvedRequestBinding> {
        if ctx.model() != binding.resolved_selector {
            let effective = self.routing_table.resolve_model(ctx.model()).await?;
            if binding.request_checks.is_empty() && !effective.request_checks.is_empty() {
                return Err(BitrouterError::internal(
                    "a selector rewrite cannot introduce request checks after the ingress binding is frozen",
                ));
            }
            binding.resolution = effective;
            binding.resolved_selector = ctx.model().to_owned();
        }
        self.validate_binding_operation(ctx, &binding)?;
        ctx.apply_preset_overrides(&binding.resolution.overrides)?;
        Ok(binding)
    }

    fn validate_binding_operation(
        &self,
        ctx: &PipelineContext,
        binding: &ResolvedRequestBinding,
    ) -> Result<()> {
        let operation = ctx.operation();
        if operation == ModelOperation::Decisions && !binding.resolution.overrides.is_empty() {
            return Err(BitrouterError::bad_request(
                "configured generation defaults do not support Decisions",
            ));
        }
        if operation == ModelOperation::Decisions
            && binding.resolution.policy.is_some()
            && !self
                .model_selectors
                .iter()
                .any(|hook| hook.supports(operation))
        {
            return Err(BitrouterError::bad_request(
                "configured model policy does not support Decisions",
            ));
        }
        for check in &binding.request_checks {
            if operation == ModelOperation::Decisions
                && !self
                    .request_checker_runner
                    .as_ref()
                    .is_some_and(|runner| runner.supports_operation(check, operation))
            {
                return Err(BitrouterError::bad_request(
                    "configured request checker does not support this model operation",
                ));
            }
        }
        Ok(())
    }

    async fn run_request_checks(
        &self,
        ctx: &mut PipelineContext,
        bindings: &[RequestCheckBinding],
    ) -> Result<()> {
        if bindings.is_empty() {
            return Ok(());
        }
        for content in ctx
            .generation_prompt()
            .into_iter()
            .flat_map(|prompt| &prompt.messages)
            .flat_map(|message| &message.content)
        {
            crate::gateway::model::protocol::responses::validate_reasoning_projection(content)
                .map_err(BitrouterError::bad_request)?;
        }
        let runner = self.request_checker_runner.as_ref();
        ctx.router_identity().ok_or_else(|| {
            BitrouterError::internal("request checks lost their named-router binding")
        })?;
        let Some(runner) = runner else {
            return Err(BitrouterError::internal(
                "configured request checker is unavailable",
            ));
        };
        for binding in bindings {
            let projection = match ctx.input() {
                PipelineInput::Generation(prompt) => {
                    content_fragments(prompt, binding.max_input_bytes)
                }
                PipelineInput::GenerateContent(input) => {
                    let prompt = input.projection().ok_or_else(|| {
                        super::types::generate_content::Input::unavailable(
                            &bitrouter_ai::types::ApiProtocol::Custom(
                                bitrouter_ai::tenancy::generate_content::PROTOCOL.into(),
                            ),
                        )
                    })?;
                    content_fragments(prompt, binding.max_input_bytes)
                }
                PipelineInput::Decisions(request) => {
                    decision_content_fragments(request, binding.max_input_bytes)
                }
            };
            let (content, coverage) = projection.map_err(|_| BitrouterError::BadRequest {
                message: "request exceeds the configured checker input limit".to_string(),
            })?;
            async {
                let result = runner
                    .check(
                        binding.clone(),
                        Input {
                            operation: ctx.operation(),
                            content,
                            coverage,
                        },
                    )
                    .await
                    .and_then(|result| {
                        result.decision.validate().map_err(|_| CheckerFailure {
                            kind: CheckerFailureKind::InvalidResponse,
                            detail: Some("invalid_decision".to_owned()),
                        })?;
                        Ok(result)
                    });
                match result {
                    Ok(CheckerResult {
                        decision: Decision::Allow,
                        revision: _,
                    }) => {}
                    Ok(CheckerResult {
                        decision: Decision::Deny { reason_code: _ },
                        revision: _,
                    }) => {
                        return Err(BitrouterError::Forbidden(
                            "request denied by configured checker".to_string(),
                        ));
                    }
                    Err(_) => {
                        return Err(BitrouterError::internal(
                            "configured request checker failed closed",
                        ));
                    }
                }
                Ok(())
            }
            .await?;
        }
        Ok(())
    }

    fn http_finalizer(
        &self,
        ctx: &PipelineContext,
    ) -> Option<Arc<crate::continuation::tenant::runtime::Runtime>> {
        // A generic/composed finalizer cannot inherit this private obligation's
        // proof. Only the actual installed tenant HTTP runtime may use it.
        self.required_finalizers
            .iter()
            .find(|hook| hook.supports(ctx.operation()))
            .filter(|hook| {
                hook.type_id == std::any::TypeId::of::<crate::continuation::ContinuationRuntime>()
            })?;
        ctx.extension::<crate::continuation::tenant::runtime::Runtime>()
    }

    async fn prepare_required_finalizers(
        &self,
        ctx: &PipelineContext,
        streamed: bool,
    ) -> Result<RequiredFinalizationGuard> {
        let finalization = ctx.required_finalization_context(streamed);
        let mut guard = RequiredFinalizationGuard::new(
            applicable(&self.required_finalizers, ctx.operation()),
            finalization.clone(),
            self.pending_settlements.clone(),
            ExecutionCustody::from_context(ctx),
            self.http_finalizer(ctx),
        );
        if !finalization.successful_terminal {
            guard.disarm();
            return Ok(guard);
        }
        guard.receipt = self
            .prepare_required_finalizer_tracked(
                &finalization,
                ExecutionCustody::from_context(ctx),
                guard.http.clone(),
            )
            .await?;
        Ok(guard)
    }

    async fn prepare_required_finalizer_tracked(
        &self,
        finalization: &RequiredFinalizationContext,
        custody: ExecutionCustody,
        http: Option<Arc<crate::continuation::tenant::runtime::Runtime>>,
    ) -> Result<Option<RequiredFinalizationReceipt>> {
        let Some(finalizer) = self
            .required_finalizers
            .iter()
            .find(|hook| hook.supports(finalization.operation))
            .map(|hook| hook.hook.clone())
        else {
            return Ok(None);
        };
        let task_finalization = finalization.clone();
        let receipt = RequiredFinalizationReceipt::new();
        let task_receipt = receipt.clone();
        let tasks = self.pending_settlements.clone();
        let (prepared_tx, prepared_rx) = tokio::sync::oneshot::channel();
        let (consumed_tx, consumed_rx) = tokio::sync::oneshot::channel();
        self.spawn_stream_finalization(async move {
            let preparation_custody = custody;
            match finalizer
                .finalize_with_receipt(&task_finalization, &task_receipt)
                .await
            {
                Ok(()) => {
                    // A successful send does not prove that the caller took
                    // rollback ownership. Keep this tracked owner until the
                    // receiver acknowledges consumption; dropping a queued
                    // payload must compensate just like a failed send.
                    // https://docs.rs/tokio/latest/tokio/sync/oneshot/struct.Sender.html#method.send
                    if (prepared_tx.send(Ok((receipt, consumed_tx))).is_err()
                        || consumed_rx.await.is_err())
                        && let Err(error) = finalizer
                            .rollback_with_receipt(&task_finalization, &task_receipt)
                            .await
                    {
                        tasks.finalizer_failed(&error);
                    }
                }
                Err(error) => {
                    preparation_custody.record_pipeline_outcome(false);
                    rollback_failed_finalizer(
                        finalizer.as_ref(),
                        &task_finalization,
                        &task_receipt,
                        &tasks,
                        http.as_deref(),
                        &error,
                    )
                    .await;
                    let _ = prepared_tx.send(Err(error));
                }
            }
        });
        let (receipt, consumed) = prepared_rx.await.map_err(|error| {
            BitrouterError::internal(format!(
                "required finalizer preparation ended unexpectedly: {error}"
            ))
        })??;
        // No await may separate this acknowledgement, the Ready return, and
        // installation in the nonstream/stream guard by our callers. If the
        // tracked producer was aborted, the caller still owns this receipt.
        if consumed.send(()).is_err() {
            tracing::warn!("required finalizer producer ended during receipt handoff");
        }
        Ok(Some(receipt))
    }

    async fn resolve_route(
        &self,
        ctx: &mut PipelineContext,
        binding: ResolvedRequestBinding,
    ) -> Result<Vec<RoutingTarget>> {
        // Binding/default resolution already ran before local and external
        // request checks. Effective model selection remains here so existing
        // pre-request ACLs continue to see the normalized named selector.
        if ctx.model() != binding.resolved_selector {
            return Err(BitrouterError::internal(
                "the effective selector changed after entry preparation",
            ));
        }
        let operation = ctx.operation();
        let resolution = binding.resolution;
        #[cfg(feature = "multi-instance")]
        if let Some(admission) =
            ctx.extension::<crate::gateway::shared::admitted::SharedAdmission>()
        {
            use crate::gateway::shared::capture::Bypass;
            if resolution.policy.is_none() {
                admission.retain_bypass(if operation == ModelOperation::Decisions {
                    Bypass::Decisions
                } else {
                    Bypass::UnboundRoute
                })?;
            }
        }
        if let Some(policy) = resolution.policy.as_deref() {
            for selector in self
                .model_selectors
                .iter()
                .filter(|hook| hook.supports(operation))
            {
                selector.prepare_selection(policy, ctx).await?;
            }
        }
        if operation == ModelOperation::Generation
            && !matches!(ctx.input(), PipelineInput::GenerateContent(_))
        {
            let prepared = ctx
                .extension::<crate::gateway::routing::preparation::Prepared>()
                .map(|prepared| prepared.as_ref().clone());
            let prepared = match prepared {
                Some(prepared) => prepared,
                None => {
                    crate::gateway::routing::preparation::Prepared::from_prompt(
                        ctx.require_generation_prompt()?,
                        resolution.policy.as_ref().and(self.decision_model.as_ref()),
                        ctx.request_id(),
                    )
                    .await
                }
            };
            prepared.validate_for(ctx.require_generation_prompt()?)?;
            if let Some(receipt) = &prepared.receipt {
                ctx.emit(receipt.clone());
                #[cfg(feature = "multi-instance")]
                if let Some(admission) =
                    ctx.extension::<crate::gateway::shared::admitted::SharedAdmission>()
                {
                    admission.retain_routing(ctx)?;
                }
            }
            ctx.insert_extension(Arc::new(prepared));
        }
        ctx.set_model(resolution.clean_model);
        if let Some(policy) = resolution.policy.as_deref() {
            let operation = ctx.operation();
            for selector in self
                .model_selectors
                .iter()
                .filter(|hook| hook.supports(operation))
            {
                selector.select_policy(policy, ctx).await?;
            }
        }

        // Supply actual prompt requirements to the shared routing table. The
        // configured catalog carries positive observations; omitted declarations
        // alone do not establish incompatibility or remove a provider.
        let mut prefs = resolution.prefs;
        prefs.require_capabilities = ctx
            .generation_prompt()
            .map(crate::gateway::model::capabilities::required_capabilities)
            .unwrap_or_default();
        prefs.operation = ctx.operation();
        prefs.registry_requirements = ctx
            .generation_prompt()
            .map(crate::gateway::catalog::execution::RequestRequirements::generation)
            .or_else(|| {
                ctx.decision_request()
                    .map(crate::gateway::catalog::execution::RequestRequirements::decisions)
            });
        // Carry the inbound protocol so the table can prefer a native,
        // same-protocol upstream for each chosen target.
        prefs.inbound_protocol = ctx.inbound_protocol();
        let mut chain = self
            .routing_table
            .route_resolved(ctx.model(), &prefs, ctx.caller())
            .await?;
        let mut routing_prompt = None;
        if operation == ModelOperation::Generation
            && !matches!(ctx.input(), PipelineInput::GenerateContent(_))
        {
            filter_reasoning_effort_targets(&mut chain, ctx.require_generation_prompt()?)?;
            let prepared = ctx
                .extension::<crate::gateway::routing::preparation::Prepared>()
                .ok_or_else(|| BitrouterError::internal("routing preparation vanished"))?;
            let mut prompt = ctx.require_generation_prompt()?.clone();
            // Freeze the effective route, retaining the ingress selector in
            // original_model rather than restoring it from the input prompt.
            prompt.model = ctx.model().to_owned();
            prepared.validate_for(&prompt)?;
            ctx.apply_routing_prompt(prompt.clone())?;
            routing_prompt = Some(prompt);
        }
        let operation = ctx.operation();
        for hook in self
            .route_hooks
            .iter()
            .filter(|hook| hook.supports(operation))
        {
            hook.resolve(&mut chain, ctx).await?;
        }
        chain.retain(|target| target.api_protocol.operation() == ctx.operation());
        if let Some(prompt) = ctx.generation_prompt() {
            filter_reasoning_effort_targets(&mut chain, prompt)?;
        }
        if chain.is_empty() {
            if ctx.operation() == ModelOperation::Decisions {
                return Err(BitrouterError::bad_request(
                    "no compatible Decisions target for the selected model",
                ));
            }
            return Err(BitrouterError::NotFound(format!(
                "no route for model '{}'",
                ctx.model()
            )));
        }
        for hook in self
            .route_hooks
            .iter()
            .filter(|hook| hook.supports(operation))
        {
            hook.after_resolve(&chain, ctx).await?;
        }
        ctx.route_chain = Some(chain.clone());
        if let Some(prompt) = routing_prompt.as_ref()
            && (ctx.model() != prompt.model || ctx.generation_prompt() != Some(prompt))
        {
            return Err(BitrouterError::bad_request(
                "route hook changed the frozen routing prompt",
            ));
        }
        #[cfg(feature = "multi-instance")]
        if let Some(admission) =
            ctx.extension::<crate::gateway::shared::admitted::SharedAdmission>()
        {
            admission.retain_routing(ctx)?;
            admission.route_ready()?;
        }
        Ok(chain)
    }

    fn stream_pricing(
        &self,
        ctx: &PipelineContext,
        target: &RoutingTarget,
    ) -> Option<super::stream::UsagePricing> {
        let target_key = crate::gateway::pipeline::stream::PricingTargetKey::from_target(target);
        let frozen = ctx
            .get_events::<crate::gateway::pipeline::stream::UsagePricingSnapshot>()
            .into_iter()
            .rev()
            .find(|snapshot| snapshot.target == target_key);
        match frozen {
            Some(snapshot) => snapshot.pricing.clone(),
            None => self.routing_table.usage_pricing(ctx.model(), target),
        }
    }

    /// Keep native streaming state on the heap so ordinary batch callers do
    /// not inherit its async frame through nested model/judge futures.
    async fn execute_with_fallback(
        &self,
        chain: &[RoutingTarget],
        input: ExecutionInput<'_>,
        ctx: &PipelineContext,
    ) -> Result<ExecutionResult> {
        let mut errors = Vec::new();
        let mut excluded = crate::gateway::conversion_report::ConversionReport::default();
        for (attempt_index, target) in chain.iter().enumerate() {
            if target.api_protocol.operation() != ctx.operation() {
                continue;
            }
            match input {
                ExecutionInput::GenerateContent(input) => {
                    if !self
                        .admit_generate_content_candidate(target, input, false, ctx, &mut excluded)
                        .await?
                    {
                        continue;
                    }
                }
                #[cfg(feature = "server-tools")]
                ExecutionInput::Generation(prompt) => {
                    if !self
                        .admit_candidate(target, prompt, false, ctx, &mut excluded)
                        .await?
                    {
                        continue;
                    }
                }
                ExecutionInput::Decisions(request) => {
                    self.executor.preflight_decisions(target, request)?;
                }
            }
            self.wait_before_fallback(errors.len()).await;
            let reached = match ctx.extension::<crate::tenancy::execution::TenantExecution>() {
                Some(execution) => execution.reached_hop(attempt_index, target),
                None => Ok(()),
            };
            let mut technical_hop = super::provider_technical::begin_hop(ctx, false);
            self.observe_hop_start(ctx, target).await;
            let outcome = match reached {
                Err(error) => Err(error),
                Ok(()) => match input {
                    ExecutionInput::GenerateContent(input) => {
                        self.executor
                            .execute_generate_content(target, input, ctx)
                            .await
                    }
                    #[cfg(feature = "server-tools")]
                    ExecutionInput::Generation(prompt) => {
                        self.executor.execute(target, prompt, ctx).await
                    }
                    ExecutionInput::Decisions(request) => {
                        self.executor.execute_decisions(target, request, ctx).await
                    }
                },
            }
            .and_then(|result| {
                let matches = match input {
                    ExecutionInput::GenerateContent(_) => {
                        result.result.generation().is_some()
                            || result.result.native_generate_content().is_some()
                    }
                    #[cfg(feature = "server-tools")]
                    ExecutionInput::Generation(_) => result.result.generation().is_some(),
                    ExecutionInput::Decisions(_) => result.result.decisions().is_some(),
                };
                if !matches {
                    if ctx.operation() == ModelOperation::Decisions {
                        // Usage from a generation result is not native wire
                        // evidence and cannot be charged as Decisions usage.
                        ctx.record_decision_failure_usage(None);
                    }
                    return Err(BitrouterError::UpstreamInvalidResponse {
                        message: "executor result operation mismatch".into(),
                        usage: None,
                    });
                }
                if let (ExecutionInput::Decisions(request), Some(native)) =
                    (&input, result.result.decisions())
                {
                    // Custom executors must satisfy the same native contract as
                    // HTTP executors before any success hook or settlement runs.
                    crate::gateway::model::protocol::decisions::DecisionsCodec::render_response(
                        native, request,
                    )
                    .map_err(|error| {
                        ctx.record_decision_failure_usage(error.decision_usage().cloned());
                        BitrouterError::from(error)
                    })?;
                }
                Ok(result)
            });
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
            technical_hop.finish(outcome.is_ok());
            match outcome {
                Ok(result) => {
                    for hook in self
                        .execution_hooks
                        .iter()
                        .filter(|hook| hook.supports(ctx.operation()))
                    {
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
        if errors.is_empty() && !excluded.issues.is_empty() {
            return Err(BitrouterError::Incompatible { report: excluded });
        }
        Err(aggregate_fallback_errors(errors))
    }

    async fn execute_generation_with_fallback(
        &self,
        chain: &[RoutingTarget],
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> Result<ControlledExecution> {
        let mut errors = Vec::new();
        let mut excluded = crate::gateway::conversion_report::ConversionReport::default();
        let mut dispatched = 0;
        for (attempt_index, target) in chain.iter().enumerate() {
            if !self
                .admit_candidate(target, prompt, false, ctx, &mut excluded)
                .await?
            {
                continue;
            }
            self.wait_before_fallback(dispatched).await;
            dispatched += 1;
            let reached = match ctx.extension::<crate::tenancy::execution::TenantExecution>() {
                Some(execution) => execution.reached_hop(attempt_index, target),
                None => Ok(()),
            };
            let mut technical_hop = super::provider_technical::begin_hop(ctx, false);
            self.observe_hop_start(ctx, target).await;
            let mut outcome = match reached {
                Err(error) => Err(error),
                Ok(()) => self.executor.execute(target, prompt, ctx).await,
            };
            outcome = outcome.and_then(|result| {
                if result.result.generation().is_none() {
                    return Err(BitrouterError::UpstreamInvalidResponse {
                        message: "executor result operation mismatch".into(),
                        usage: None,
                    });
                }
                Ok(result)
            });
            // An Ok generation result is selected terminal work, even when
            // its finish reason is an error. Preserve that decision before
            // report/observer awaits; ordinary Err attempts may still recover
            // through fallback and must not poison a later successful result.
            if matches!(
                outcome
                    .as_ref()
                    .ok()
                    .and_then(|result| result.result.generation())
                    .and_then(|result| result.finish_reason.as_ref()),
                Some(bitrouter_ai::types::FinishReason::Error(_))
            ) {
                ExecutionCustody::from_context(ctx).record_pipeline_outcome(false);
            }

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
            technical_hop.finish(outcome.is_ok());
            if let Some(execution) = ctx.extension::<crate::tenancy::execution::TenantExecution>()
                && let Some(failure) = execution.failure()
            {
                // A tenant outcome callback may expire after the provider
                // completed. Preserve billed output for error settlement while
                // preventing successful delivery or another fallback attempt.
                let error = execution.reject(failure.0.code);
                return match outcome {
                    Ok(result) => {
                        ctx.set_successful_target(target.clone());
                        Ok(ControlledExecution {
                            result,
                            output_rejection: Some(error),
                        })
                    }
                    // An executor error may carry provider-reported usage.
                    // The ingress maps the remembered tenancy failure after
                    // settlement; keep this evidence on the terminal path.
                    Err(error) => Err(error),
                };
            }
            match outcome {
                Ok(result) => {
                    for hook in self
                        .execution_hooks
                        .iter()
                        .filter(|hook| hook.supports(ctx.operation()))
                    {
                        hook.on_success(ctx, &result).await?;
                    }
                    ctx.set_successful_target(target.clone());
                    return Ok(ControlledExecution {
                        result,
                        output_rejection: None,
                    });
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
        if errors.is_empty() && !excluded.issues.is_empty() {
            return Err(BitrouterError::Incompatible { report: excluded });
        }
        Err(aggregate_fallback_errors(errors))
    }

    async fn execute_stream_with_fallback(
        &self,
        chain: &[RoutingTarget],
        ctx: &PipelineContext,
    ) -> Result<StreamingExecution> {
        let mut errors = Vec::new();
        let mut excluded = crate::gateway::conversion_report::ConversionReport::default();
        for (attempt_index, target) in chain.iter().enumerate() {
            let admitted = match ctx.input() {
                PipelineInput::GenerateContent(input) => {
                    self.admit_generate_content_candidate(target, input, true, ctx, &mut excluded)
                        .await?
                }
                _ => {
                    self.admit_candidate(
                        target,
                        ctx.require_generation_prompt()?,
                        true,
                        ctx,
                        &mut excluded,
                    )
                    .await?
                }
            };
            if !admitted {
                continue;
            }
            self.wait_before_fallback(errors.len()).await;
            let provider_started_at = Instant::now();
            let mut technical_hop = super::provider_technical::begin_hop(ctx, true);
            let technical =
                super::provider_technical::begin(ctx, target, provider_started_at, true);
            let reached = match ctx.extension::<crate::tenancy::execution::TenantExecution>() {
                Some(execution) => execution.reached_hop(attempt_index, target),
                None => Ok(()),
            };
            self.observe_hop_start(ctx, target).await;
            let outcome = match reached {
                Err(error) => Err(error),
                Ok(()) => match ctx.input() {
                    PipelineInput::GenerateContent(input) => {
                        self.executor
                            .execute_generate_content_stream(target, input, ctx)
                            .await
                    }
                    _ => {
                        self.executor
                            .execute_stream(target, ctx.require_generation_prompt()?, ctx)
                            .await
                    }
                },
            };
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
                    technical.detach();
                    let stream = Box::pin(ObservedUpstreamStream {
                        execution: ctx
                            .extension::<crate::tenancy::execution::TenantExecution>()
                            .map(|execution| Arc::downgrade(&execution)),
                        operational: ctx
                            .extension::<crate::tenancy::execution::TenantExecution>()
                            .and_then(|execution| execution.current_hop()),
                        inner: stream,
                        hooks: applicable(&self.observe_hooks, ModelOperation::Generation),
                        request_id: ctx.request_id().to_string(),
                        target: target.clone(),
                        provider_started_at,
                        terminal: false,
                        technical_hop: technical_hop.opened(),
                        technical: super::provider_technical::current(ctx),
                    });
                    return Ok(StreamingExecution {
                        stream,
                        target: target.clone(),
                        provider_started_at,
                    });
                }
                Err(e) => {
                    technical_hop.finish(false);
                    match self.classify_failure(ctx, &e, target).await {
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
                    }
                }
            }
        }
        if errors.is_empty() && !excluded.issues.is_empty() {
            return Err(BitrouterError::Incompatible { report: excluded });
        }
        Err(aggregate_fallback_errors(errors))
    }

    /// Share the executor's conversion contract without invoking failure policy
    /// or hop observers for a candidate that cannot represent this source.
    async fn admit_candidate(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        stream: bool,
        ctx: &PipelineContext,
        excluded: &mut crate::gateway::conversion_report::ConversionReport,
    ) -> Result<bool> {
        let controls = ctx
            .extension::<crate::tenancy::execution::TenantExecution>()
            .map_or(Ok(()), |execution| {
                execution.admit_native_controls(prompt, &target.api_protocol)
            });
        let assessment = controls
            .and_then(|()| self.executor.preflight(target, prompt, stream))
            .and_then(|report| {
                report.require_admitted()?;
                Ok(report)
            });
        self.admit_assessment(assessment, target, ctx, excluded)
            .await
    }

    async fn admit_generate_content_candidate(
        &self,
        target: &RoutingTarget,
        input: &super::types::generate_content::Input,
        stream: bool,
        ctx: &PipelineContext,
        excluded: &mut crate::gateway::conversion_report::ConversionReport,
    ) -> Result<bool> {
        let assessment = self
            .executor
            .preflight_generate_content(target, input, stream, ctx)
            .and_then(|report| {
                report.require_admitted()?;
                Ok(report)
            });
        self.admit_assessment(assessment, target, ctx, excluded)
            .await
    }

    async fn admit_assessment(
        &self,
        assessment: Result<crate::gateway::conversion_report::ConversionReport>,
        target: &RoutingTarget,
        ctx: &PipelineContext,
        excluded: &mut crate::gateway::conversion_report::ConversionReport,
    ) -> Result<bool> {
        match assessment {
            Ok(report) => {
                if !report.admitted.is_empty() {
                    for hook in self
                        .observe_hooks
                        .iter()
                        .filter(|hook| hook.supports(ctx.operation()))
                    {
                        let fut = std::panic::AssertUnwindSafe(
                            hook.on_conversion_admitted(ctx, target, &report),
                        );
                        if fut.catch_unwind().await.is_err() {
                            tracing::warn!(
                                "ObserveHook::on_conversion_admitted panicked; swallowed"
                            );
                        }
                    }
                }
                Ok(true)
            }
            Err(BitrouterError::Incompatible { report }) if !report.is_output_failure() => {
                for hook in self
                    .observe_hooks
                    .iter()
                    .filter(|hook| hook.supports(ctx.operation()))
                {
                    let fut = std::panic::AssertUnwindSafe(
                        hook.on_conversion_excluded(ctx, target, &report),
                    );
                    if fut.catch_unwind().await.is_err() {
                        tracing::warn!("ObserveHook::on_conversion_excluded panicked; swallowed");
                    }
                }
                excluded.issues.extend(report.issues);
                excluded.admitted.extend(report.admitted);
                Ok(false)
            }
            Err(error) => Err(error),
        }
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
        if let Some(execution) = ctx.extension::<crate::tenancy::execution::TenantExecution>()
            && execution.receipt_blocks_fallback()
        {
            return FallbackDecision::Fail(err.clone());
        }
        if let Some(allowed) = ctx
            .extension::<crate::tenancy::execution::TenantExecution>()
            .and_then(|execution| execution.operational_fallback())
        {
            return if allowed {
                FallbackDecision::TryNext
            } else {
                FallbackDecision::Fail(err.clone())
            };
        }
        #[cfg(feature = "multi-instance")]
        if let Some(admission) =
            ctx.extension::<crate::gateway::shared::admitted::SharedAdmission>()
            && admission.blocks_fallback()
        {
            // This also persists observed terminal evidence on an interrupted
            // stream. A storage outage never permits another upstream send.
            let _ = admission.reconcile_dispatch().await;
            return FallbackDecision::Fail(err.clone());
        }
        if ctx.has_completed_decision_failure()
            || (ctx.operation() == ModelOperation::Decisions
                && matches!(err, BitrouterError::UpstreamInvalidResponse { .. }))
        {
            return FallbackDecision::Fail(err.clone());
        }
        for hook in self
            .execution_hooks
            .iter()
            .filter(|hook| hook.supports(ctx.operation()))
        {
            if let FallbackDecision::Fail(e) = hook.on_failure(ctx, err).await {
                return FallbackDecision::Fail(e);
            }
        }
        self.fallback_policy.classify(err, target)
    }

    /// Stage 4 — Settlement. Every `SettlementRecorder` runs in registration
    /// order against the immutable [`SettlementContext`]. Deployments that
    /// need exclusive-charging semantics implement them inside a single
    /// recorder. The application invokes all registered recorders.
    async fn run_settlement(
        &self,
        ctx: &mut PipelineContext,
        streamed: bool,
        error: Option<BitrouterError>,
    ) {
        if error.is_some() {
            ExecutionCustody::from_context(ctx).record_pipeline_outcome(false);
        }
        ctx.finalize_request_duration();
        ctx.finalize_technical_scope(streamed, error.is_some());
        let mut settle = ctx.settlement_context();
        settle.streamed = streamed;
        if let Some(BitrouterError::UpstreamInvalidResponse {
            usage: Some(usage), ..
        }) = &error
        {
            settle.prompt_tokens = usage.prompt_tokens;
            settle.completion_tokens = usage.completion_tokens;
            settle.reasoning_tokens = usage.reasoning_tokens;
            settle.cache_read_tokens = usage.cache_read_tokens;
            settle.cache_write_tokens = usage.cache_write_tokens;
            settle.web_search_count = usage.web_search_count;
            settle.usage_origin = usage.origin;
            settle.raw_usage = usage.raw.as_deref().cloned();
        }
        settle.error = error;

        // Emit the canonical "request finished" line before recorders
        // run. The two-line model (received + finished) is inherited from
        // v0's operator-facing log shape and kept for log-scraper
        // compatibility.
        log_request_finished(&settle);

        for recorder in self
            .settlement_recorders
            .iter()
            .filter(|hook| hook.supports(ctx.operation()))
        {
            if let Err(e) = recorder.record(&mut settle).await {
                tracing::error!(error = %e, "SettlementRecorder failed");
            }
        }

        ctx.absorb_settlement(settle);
    }

    // ===== observe helpers (read-only, swallow errors AND panics) =====

    async fn observe_start(&self, ctx: &PipelineContext) {
        for hook in self
            .observe_hooks
            .iter()
            .filter(|hook| hook.supports(ctx.operation()))
        {
            let fut = std::panic::AssertUnwindSafe(hook.on_request_start(ctx));
            if fut.catch_unwind().await.is_err() {
                tracing::warn!("ObserveHook::on_request_start panicked; swallowed");
            }
        }
    }

    async fn observe_after(&self, phase: Phase, ctx: &PipelineContext) {
        for hook in self
            .observe_hooks
            .iter()
            .filter(|hook| hook.supports(ctx.operation()))
        {
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
        if let HopOutcome::Failed(error) = outcome
            && let Some(execution) = ctx.extension::<crate::tenancy::execution::TenantExecution>()
        {
            execution.operational_failed(error);
        }
        observe_hop_end_with(&self.observe_hooks, ctx, target, outcome).await;
    }

    async fn observe_end(&self, ctx: &PipelineContext, outcome: RequestOutcome) {
        for hook in self
            .observe_hooks
            .iter()
            .filter(|hook| hook.supports(ctx.operation()))
        {
            let fut = std::panic::AssertUnwindSafe(hook.on_request_end(ctx, &outcome));
            if fut.catch_unwind().await.is_err() {
                tracing::warn!("ObserveHook::on_request_end panicked; swallowed");
            }
        }
    }
}

fn filter_reasoning_effort_targets(chain: &mut Vec<RoutingTarget>, prompt: &Prompt) -> Result<()> {
    let Some(effort) = prompt.params.reasoning_effort else {
        return Ok(());
    };
    let policy_owned =
        prompt.params.reasoning_effort_source == bitrouter_ai::types::ReasoningEffortSource::Policy;
    chain.retain(|target| match &target.reasoning_effort {
        Some(support) => support.levels.contains(&effort),
        None => !policy_owned,
    });
    if chain.is_empty() {
        return Err(BitrouterError::bad_request(format!(
            "reasoning effort '{effort}' has no supported provider/model route"
        )));
    }
    Ok(())
}

async fn observe_hop_start_with(
    hooks: &[HookRegistration<dyn ObserveHook>],
    ctx: &PipelineContext,
    target: &RoutingTarget,
) {
    for hook in hooks.iter().filter(|hook| hook.supports(ctx.operation())) {
        let fut = std::panic::AssertUnwindSafe(hook.on_hop_start(ctx, target));
        if fut.catch_unwind().await.is_err() {
            tracing::warn!("ObserveHook::on_hop_start panicked; swallowed");
        }
    }
}

async fn observe_hop_end_with(
    hooks: &[HookRegistration<dyn ObserveHook>],
    ctx: &PipelineContext,
    target: &RoutingTarget,
    outcome: HopOutcome<'_>,
) {
    for hook in hooks.iter().filter(|hook| hook.supports(ctx.operation())) {
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
mod policy_effort_target_tests {
    use super::filter_reasoning_effort_targets;
    use crate::config::model::ReasoningEffortConfig;
    use crate::gateway::pipeline::types::RoutingTarget;
    use bitrouter_ai::types::{
        ApiProtocol, GenerationParams, Prompt, ReasoningEffort, ReasoningEffortSource,
    };

    fn prompt() -> Prompt {
        Prompt {
            model: "model".to_string(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: Vec::new(),
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    fn target(name: &str, levels: Option<Vec<ReasoningEffort>>) -> RoutingTarget {
        RoutingTarget {
            registry: None,
            provider_name: name.to_string(),
            service_id: "model".to_string(),
            api_base: "https://example.test/v1".to_string(),
            api_key: "test".to_string(),
            api_protocol: ApiProtocol::Responses,
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
            chat_google_extensions: false,
            reasoning_effort: levels.map(|levels| ReasoningEffortConfig {
                levels,
                default: None,
            }),
            model_constraints: Default::default(),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
            headers: Vec::new(),
        }
    }

    #[test]
    fn policy_effort_keeps_only_exact_positive_support() -> crate::gateway::error::Result<()> {
        let mut prompt = prompt();
        prompt.params.reasoning_effort = Some(ReasoningEffort::High);
        prompt.params.reasoning_effort_source = ReasoningEffortSource::Policy;
        let mut chain = vec![
            target("supported", Some(vec![ReasoningEffort::High])),
            target("wrong-level", Some(vec![ReasoningEffort::Low])),
            target("unknown", None),
        ];

        filter_reasoning_effort_targets(&mut chain, &prompt)?;
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].provider_name, "supported");
        Ok(())
    }

    #[test]
    fn caller_effort_preserves_unknown_target_passthrough() -> crate::gateway::error::Result<()> {
        let mut prompt = prompt();
        prompt.params.reasoning_effort = Some(ReasoningEffort::High);
        let mut chain = vec![target("unknown", None)];

        filter_reasoning_effort_targets(&mut chain, &prompt)?;
        assert_eq!(chain.len(), 1);
        Ok(())
    }

    #[test]
    fn caller_effort_drops_routes_with_known_incompatible_ladders()
    -> crate::gateway::error::Result<()> {
        let mut prompt = prompt();
        prompt.params.reasoning_effort = Some(ReasoningEffort::Minimal);
        let mut chain = vec![
            target("anthropic", Some(vec![ReasoningEffort::Low])),
            target("gemini", Some(vec![ReasoningEffort::Minimal])),
            target("custom-unknown", None),
        ];

        filter_reasoning_effort_targets(&mut chain, &prompt)?;
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].provider_name, "gemini");
        assert_eq!(chain[1].provider_name, "custom-unknown");
        Ok(())
    }

    #[test]
    fn unsupported_policy_effort_fails_closed() -> crate::gateway::error::Result<()> {
        let mut prompt = prompt();
        prompt.params.reasoning_effort = Some(ReasoningEffort::Max);
        prompt.params.reasoning_effort_source = ReasoningEffortSource::Policy;
        let mut chain = vec![target("unsupported", Some(vec![ReasoningEffort::High]))];

        let error = filter_reasoning_effort_targets(&mut chain, &prompt)
            .err()
            .ok_or_else(|| {
                crate::gateway::error::BitrouterError::internal("unsupported effort was accepted")
            })?;
        assert!(error.to_string().contains("reasoning effort 'max'"));
        Ok(())
    }
}

#[cfg(test)]
mod fallback_error_tests {
    use super::aggregate_fallback_errors;
    use crate::gateway::error::BitrouterError;

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
    pending_settlements: super::tasks::Settlements,
    receipt: Option<RequiredFinalizationReceipt>,
    armed: bool,
    custody: ExecutionCustody,
    http: Option<Arc<crate::continuation::tenant::runtime::Runtime>>,
}

impl RequiredFinalizationGuard {
    fn new(
        finalizers: Vec<Arc<dyn RequiredFinalizer>>,
        finalization: RequiredFinalizationContext,
        pending_settlements: super::tasks::Settlements,
        custody: ExecutionCustody,
        http: Option<Arc<crate::continuation::tenant::runtime::Runtime>>,
    ) -> Self {
        custody.record_pipeline_outcome(finalization.successful_terminal);
        Self {
            finalizers,
            finalization,
            pending_settlements,
            receipt: None,
            armed: true,
            custody,
            http,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
        self.receipt = None;
    }

    fn begin_delivery(
        &mut self,
    ) -> (
        DeliveryPermit,
        tokio::sync::oneshot::Receiver<DeliveryAuthorizationOutcome>,
    ) {
        let (permit, delivery) =
            delivery_handshake(self.custody.clone(), self.finalization.successful_terminal);
        let (outcome_tx, outcome_rx) = tokio::sync::oneshot::channel();
        let finalizers = self.finalizers.clone();
        let finalization = self.finalization.clone();
        let receipt = self.receipt.clone();
        let custody = self.custody.clone();
        let tasks = self.pending_settlements.clone();
        let http = self.http.clone();
        self.disarm();
        let authorization = async move {
            let authorization_result = match receipt.as_ref() {
                Some(receipt) => {
                    finalizers[0]
                        .commit_with_receipt(&finalization, receipt, &delivery)
                        .await
                }
                None => delivery.wait_for_delivery().await,
            };
            let delivered = match authorization_result {
                Ok(delivered) => delivered,
                Err(error) => {
                    custody.record_pipeline_outcome(false);
                    delivery.reject(error.clone());
                    if let Some(receipt) = receipt.as_ref() {
                        rollback_failed_finalizer(
                            finalizers[0].as_ref(),
                            &finalization,
                            receipt,
                            &tasks,
                            http.as_deref(),
                            &error,
                        )
                        .await;
                    }
                    let _ = outcome_tx.send(DeliveryAuthorizationOutcome::Failed(error));
                    return;
                }
            };
            if !delivered
                && let Some(receipt) = receipt.as_ref()
                && let Err(error) = finalizers[0]
                    .rollback_with_receipt(&finalization, receipt)
                    .await
            {
                tasks.finalizer_failed(&error);
            }
            let _ = outcome_tx.send(if delivered {
                DeliveryAuthorizationOutcome::Delivered
            } else {
                DeliveryAuthorizationOutcome::Disconnected
            });
        }
        .instrument(tracing::Span::current());
        self.pending_settlements.spawn(authorization);
        (permit, outcome_rx)
    }
}

impl Drop for RequiredFinalizationGuard {
    fn drop(&mut self) {
        if !self.armed || self.receipt.is_none() {
            return;
        }
        let finalizers = self.finalizers.clone();
        let finalization = self.finalization.clone();
        let receipt = self.receipt.clone();
        let custody = self.custody.clone();
        let tasks = self.pending_settlements.clone();
        self.disarm();
        let rollback = async move {
            let _custody = custody;
            if let Some(receipt) = receipt.as_ref()
                && let Err(error) = finalizers[0]
                    .rollback_with_receipt(&finalization, receipt)
                    .await
            {
                tasks.finalizer_failed(&error);
            }
        }
        .instrument(tracing::Span::current());
        self.pending_settlements.spawn(rollback);
    }
}

/// Owns the streaming `StreamProcessor` + `PipelineContext` for the lifetime of
/// a streaming response, and guarantees the StreamHook stage's `on_stream_end`
/// plus Stage-4 Settlement run **exactly once** — whether the stream completes
/// normally or the client drops it early.
struct StreamSettlementGuard {
    pipeline: Arc<Pipeline>,
    latest_attempt: SharedStreamAttempt,
    required_finalizer_receipt: Option<RequiredFinalizationReceipt>,
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
    /// through it). Invalid or repeated state transitions fail closed without
    /// taking ownership away from `Drop`.
    fn processor(&mut self) -> Result<&mut StreamProcessor> {
        match self.state.as_mut() {
            Some(StreamDeliveryState::Pending(state)) => {
                state.processor.as_mut().ok_or_else(|| {
                    BitrouterError::internal("stream processor has already been absorbed")
                })
            }
            None => Err(BitrouterError::internal(
                "stream guard has already been finalized",
            )),
            Some(StreamDeliveryState::Preparing { .. } | StreamDeliveryState::Ready(_)) => Err(
                BitrouterError::internal("stream processor is already preparing or absorbed"),
            ),
        }
    }

    async fn finish_processor(&mut self, outcome: StreamOutcome) -> Result<()> {
        match self.state.as_ref() {
            Some(StreamDeliveryState::Pending(_)) => {}
            Some(StreamDeliveryState::Preparing { .. }) => {
                return Err(BitrouterError::internal(
                    "stream terminal preparation is already in progress",
                ));
            }
            Some(StreamDeliveryState::Ready(_)) => {
                return Err(BitrouterError::internal(
                    "stream processor has already finished",
                ));
            }
            None => {
                return Err(BitrouterError::internal(
                    "stream guard has already been finalized",
                ));
            }
        }
        let Some(StreamDeliveryState::Pending(mut pending)) = self.state.take() else {
            return Err(BitrouterError::internal(
                "stream guard state changed during terminal preparation",
            ));
        };
        let Some(mut processor) = pending.processor.take() else {
            self.state = Some(StreamDeliveryState::Pending(pending));
            return Err(BitrouterError::internal(
                "stream processor has already been absorbed",
            ));
        };
        let Some(mut ctx) = pending.ctx.take() else {
            pending.processor = Some(processor);
            self.state = Some(StreamDeliveryState::Pending(pending));
            return Err(BitrouterError::internal(
                "stream context has already been taken",
            ));
        };
        if matches!(
            outcome,
            StreamOutcome::Aborted(_) | StreamOutcome::UpstreamError(_)
        ) {
            ExecutionCustody::from_context(&ctx).record_pipeline_outcome(false);
        }
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
            if matches!(task_outcome, StreamOutcome::Completed) && !ctx.stream_terminal_succeeded()
            {
                ExecutionCustody::from_context(&ctx).record_pipeline_outcome(false);
            }

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
            return Err(BitrouterError::internal(
                "required stream finalizers need a prepared terminal",
            ));
        };
        let finalization = ctx.required_finalization_context(true);
        if !finalization.successful_terminal {
            return Ok(());
        }
        self.required_finalizer_receipt = self
            .pipeline
            .prepare_required_finalizer_tracked(
                &finalization,
                ExecutionCustody::from_context(ctx),
                self.pipeline.http_finalizer(ctx),
            )
            .await?;
        Ok(())
    }

    /// Finalise a provider terminal inline only through the required-finalizer
    /// boundary. Settlement is then enqueued and the terminal is returned
    /// synchronously by the same `poll_next` call.
    async fn finalize(&mut self, outcome: StreamOutcome) -> Result<Option<DeliveryPermit>> {
        if self.state.is_none() {
            return Err(BitrouterError::internal(
                "stream guard has already been finalized",
            ));
        }
        self.finish_processor(outcome.clone()).await?;

        let (settlement_error, request_outcome, delivery_error) = {
            let Some(StreamDeliveryState::Ready(ctx)) = self.state.as_ref() else {
                return Err(BitrouterError::internal(
                    "stream terminal preparation did not produce a ready context",
                ));
            };
            let (error, outcome) = stream_terminal_metadata(&outcome);
            if matches!(outcome, RequestOutcome::Completed) && !ctx.stream_terminal_succeeded() {
                let error = BitrouterError::UpstreamInvalidResponse {
                    message: "stream ended with a non-success provider terminal".to_string(),
                    usage: None,
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
            ExecutionCustody::from_context(&ctx).record_pipeline_outcome(false);
            let settlement_error = error.clone();
            let pipeline = self.pipeline.clone();
            let receipt = self.required_finalizer_receipt.take();
            self.pipeline.spawn_stream_finalization(async move {
                rollback_required_finalizer(&pipeline, &ctx, true, receipt).await;
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
            return Err(BitrouterError::internal(
                "stream terminal context was consumed before settlement",
            ));
        };
        if settlement_error.is_some() {
            ExecutionCustody::from_context(&ctx).record_pipeline_outcome(false);
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
        let receipt = self.required_finalizer_receipt.take();
        let finalization = ctx.required_finalization_context(true);
        let (permit, delivery) = delivery_handshake(
            ExecutionCustody::from_context(&ctx),
            finalization.successful_terminal,
        );
        self.pipeline.spawn_stream_finalization(async move {
            let authorization_result = match receipt.as_ref() {
                Some(receipt) => {
                    pipeline.required_finalizers[0]
                        .commit_with_receipt(&finalization, receipt, &delivery)
                        .await
                }
                None => delivery.wait_for_delivery().await,
            };
            let delivered = match authorization_result {
                Ok(delivered) => delivered,
                Err(error) => {
                    ExecutionCustody::from_context(&ctx).record_pipeline_outcome(false);
                    delivery.reject(error.clone());
                    if let Some(receipt) = receipt.as_ref() {
                        rollback_failed_finalizer(
                            pipeline.required_finalizers[0].hook.as_ref(),
                            &finalization,
                            receipt,
                            &pipeline.pending_settlements,
                            pipeline.http_finalizer(&ctx).as_deref(),
                            &error,
                        )
                        .await;
                    }
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
                rollback_required_finalizer(&pipeline, &ctx, true, receipt).await;
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

/// Request failure remains a failure. Only this private HTTP obligation's
/// actual actor acknowledgement can distinguish it from failed cleanup.
async fn rollback_failed_finalizer(
    finalizer: &dyn RequiredFinalizer,
    finalization: &RequiredFinalizationContext,
    receipt: &RequiredFinalizationReceipt,
    tasks: &super::tasks::Settlements,
    http: Option<&crate::continuation::tenant::runtime::Runtime>,
    error: &BitrouterError,
) {
    if http.is_none() {
        tasks.finalizer_failed(error);
    }
    let rollback = finalizer.rollback_with_receipt(finalization, receipt).await;
    if http.is_some()
        && (rollback.is_err()
            || !http.is_some_and(|runtime| runtime.confirmed_compensation(receipt)))
    {
        tasks.finalizer_failed(error);
    }
    if let Err(error) = rollback {
        tasks.finalizer_failed(&error);
    }
}

async fn rollback_required_finalizer(
    pipeline: &Pipeline,
    ctx: &PipelineContext,
    streamed: bool,
    receipt: Option<RequiredFinalizationReceipt>,
) {
    let Some(receipt) = receipt.as_ref() else {
        return;
    };
    let finalization = ctx.required_finalization_context(streamed);
    if let Err(error) = pipeline.required_finalizers[0]
        .rollback_with_receipt(&finalization, receipt)
        .await
    {
        pipeline.pending_settlements.finalizer_failed(&error);
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
                let receipt = self.required_finalizer_receipt.take();
                self.pipeline.spawn_stream_finalization(async move {
                    rollback_required_finalizer(&pipeline, &ctx, true, receipt).await;
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
    #[cfg(feature = "multi-instance")]
    use anyhow::Context as _;
    use std::sync::Arc;

    use super::{
        ExecutionCustody, RequestOutcome, StreamOutcome, StreamSettlementGuard, delivery_handshake,
        stream_terminal_metadata,
    };
    use crate::gateway::error::BitrouterError;

    use crate::gateway::pipeline::builder::PipelineBuilder;
    use crate::gateway::pipeline::executor::MockExecutor;
    use crate::gateway::pipeline::routing::StaticRoutingTable;

    fn finalized_guard() -> StreamSettlementGuard {
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(Arc::new(StaticRoutingTable::new()))
            .executor(Arc::new(MockExecutor::new(Vec::new())));
        StreamSettlementGuard {
            pipeline: Arc::new(builder.build().expect("test pipeline")),
            latest_attempt: Arc::new(std::sync::Mutex::new(None)),
            required_finalizer_receipt: None,
            state: None,
        }
    }

    #[tokio::test]
    async fn private_custody_retains_registration_after_context_is_destroyed() -> anyhow::Result<()>
    {
        let registry = crate::tenancy::execution::lifecycle::Registry::new();
        let mut builder = PipelineBuilder::new();
        builder
            .owned_drain()
            .routing_table(Arc::new(StaticRoutingTable::new()))
            .executor(Arc::new(MockExecutor::new(Vec::new())));
        let pipeline = Arc::new(builder.build()?);
        let weak = Arc::downgrade(&pipeline);
        let mut request = crate::gateway::pipeline::types::PipelineRequest::new(
            "route",
            crate::gateway::caller::CallerContext::new("credential", "principal"),
            bitrouter_ai::types::Prompt {
                model: "route".into(),
                system: None,
                system_provider_metadata: Default::default(),
                messages: vec![],
                tools: vec![],
                params: Default::default(),
                response_format: None,
                tool_choice: None,
                stream: false,
            },
        );
        registry
            .register(&"custody".try_into()?, pipeline)?
            .attach(&mut request)?;
        let ctx = crate::gateway::pipeline::context::PipelineContext::new(request);
        let custody = ExecutionCustody::from_context(&ctx);
        drop(ctx);
        {
            let mut drain = Box::pin(registry.close_and_drain());
            anyhow::ensure!(futures::poll!(&mut drain).is_pending());
        }
        anyhow::ensure!(weak.upgrade().is_some());
        drop(custody);
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            registry.close_and_drain(),
        )
        .await??;
        anyhow::ensure!(weak.upgrade().is_none());
        Ok(())
    }

    #[test]
    fn invalid_stream_guard_processor_access_returns_without_panicking() {
        let mut guard = finalized_guard();

        let access = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            guard.processor().map(|_| ())
        }));
        assert!(
            access.is_ok(),
            "invalid stream guard state panicked instead of returning a typed error"
        );
        let error = access
            .expect("processor access does not panic")
            .expect_err("finalized guard processor access must fail");
        assert!(matches!(error, BitrouterError::Internal(_)));
    }

    #[tokio::test]
    async fn repeated_stream_guard_finalization_returns_typed_error() {
        let error = match finalized_guard().finalize(StreamOutcome::Completed).await {
            Err(error) => error,
            Ok(_) => panic!("repeated finalization succeeded"),
        };
        assert!(matches!(error, BitrouterError::Internal(_)));
    }

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
        let (permit, delivery) = delivery_handshake(ExecutionCustody::default(), true);
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

    #[tokio::test]
    async fn delivered_permit_waits_for_finalizer_post_ack_commit() {
        let (permit, delivery) = delivery_handshake(ExecutionCustody::default(), true);
        let (ack_observed_tx, ack_observed_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let authorization = tokio::spawn(async move {
            let delivered = delivery.wait_for_delivery_acknowledgement().await?;
            let _ = ack_observed_tx.send(());
            let _ = finish_rx.await;
            assert!(delivery.complete_activation(Ok(())));
            assert!(delivery.wait_for_terminal_commit().await);
            Ok::<bool, BitrouterError>(delivered)
        });
        let mut delivery_result = tokio::spawn(async move { permit.deliver().await });

        ack_observed_rx
            .await
            .expect("finalizer observes downstream acknowledgement");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut delivery_result,)
                .await
                .is_err(),
            "successful terminal must wait for the finalizer's post-ack durable commit"
        );
        finish_tx.send(()).expect("release finalizer");
        assert!(authorization.await.expect("authorization joins").unwrap());
        delivery_result
            .await
            .expect("delivery task joins")
            .expect("delivery succeeds");
    }

    #[tokio::test]
    async fn dropping_permit_after_activation_signal_closes_terminal_commit() {
        let (permit, delivery) = delivery_handshake(ExecutionCustody::default(), true);
        let mut authorization = Box::pin(async move {
            assert!(delivery.wait_for_delivery_acknowledgement().await?);
            assert!(delivery.complete_activation(Ok(())));
            Ok::<bool, BitrouterError>(delivery.wait_for_terminal_commit().await)
        });
        assert!(futures::poll!(authorization.as_mut()).is_pending());

        let mut deliver = Box::pin(permit.deliver());
        assert!(futures::poll!(deliver.as_mut()).is_pending());
        assert!(futures::poll!(authorization.as_mut()).is_pending());
        drop(deliver);

        assert!(!authorization.await.expect("authorization remains valid"));
    }

    #[cfg(feature = "multi-instance")]
    #[tokio::test]
    async fn sender_witness_survives_terminal_send_before_receiver_resumes() -> anyhow::Result<()> {
        let (permit, delivery) = delivery_handshake(ExecutionCustody::default(), true);
        let witness = delivery
            .terminal_witness()
            .context("terminal witness missing")?;
        let mut authorization = Box::pin(async move {
            anyhow::ensure!(delivery.wait_for_delivery_acknowledgement().await?);
            anyhow::ensure!(delivery.complete_activation(Ok(())));
            Ok::<bool, anyhow::Error>(delivery.wait_for_terminal_commit().await)
        });
        anyhow::ensure!(futures::poll!(authorization.as_mut()).is_pending());
        let mut deliver = Box::pin(permit.deliver());
        anyhow::ensure!(futures::poll!(deliver.as_mut()).is_pending());
        anyhow::ensure!(futures::poll!(authorization.as_mut()).is_pending());
        anyhow::ensure!(witness.outcome().is_none());
        anyhow::ensure!(matches!(
            futures::poll!(deliver.as_mut()),
            std::task::Poll::Ready(Ok(()))
        ));
        // The terminal receiver has not resumed, but the sender's successful
        // accounting/send/Ready decision is already retained by the witness.
        drop(authorization);
        anyhow::ensure!(witness.outcome() == Some(true));
        anyhow::ensure!(witness.wait().await);
        Ok(())
    }

    #[cfg(feature = "multi-instance")]
    #[tokio::test]
    async fn failure_terminal_signal_does_not_become_a_success_witness() -> anyhow::Result<()> {
        let (permit, delivery) = delivery_handshake(ExecutionCustody::default(), true);
        let witness = delivery
            .terminal_witness()
            .context("terminal witness missing")?;
        let (authorization, delivery) = tokio::join!(
            delivery.wait_for_delivery(),
            permit.fail(BitrouterError::internal("fixture encoding failure")),
        );
        anyhow::ensure!(authorization.is_err());
        delivery?;
        anyhow::ensure!(witness.outcome() == Some(false));

        let (permit, delivery) = delivery_handshake(ExecutionCustody::default(), false);
        let witness = delivery
            .terminal_witness()
            .context("terminal witness missing")?;
        let (authorization, delivery) =
            tokio::join!(delivery.wait_for_delivery(), permit.deliver());
        anyhow::ensure!(authorization?);
        delivery?;
        anyhow::ensure!(!witness.wait().await);
        Ok(())
    }

    #[cfg(feature = "multi-instance")]
    #[tokio::test]
    async fn cancelled_and_unpolled_failed_permits_retain_negative_evidence() -> anyhow::Result<()>
    {
        let (permit, delivery) = delivery_handshake(ExecutionCustody::default(), true);
        let witness = delivery
            .terminal_witness()
            .context("terminal witness missing")?;
        let mut wait = Box::pin(witness.wait());
        anyhow::ensure!(futures::poll!(wait.as_mut()).is_pending());
        drop(wait);
        let failure = permit.fail(BitrouterError::internal("unpolled failure"));
        anyhow::ensure!(witness.outcome() == Some(false));
        drop(failure);
        anyhow::ensure!(!witness.wait().await);

        let (permit, delivery) = delivery_handshake(ExecutionCustody::default(), true);
        let witness = delivery
            .terminal_witness()
            .context("terminal witness missing")?;
        let mut deliver = Box::pin(permit.deliver());
        anyhow::ensure!(futures::poll!(deliver.as_mut()).is_pending());
        drop(deliver);
        anyhow::ensure!(!witness.wait().await);
        Ok(())
    }
}
