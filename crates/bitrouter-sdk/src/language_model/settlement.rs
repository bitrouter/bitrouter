//! Settlement stage for the `language_model` protocol — the always-run
//! [`SettlementRecorder`] list.
//!
//! The SDK is opinionated only about *pipeline data correctness*: a recorder
//! receives the final token / latency / model / error data the pipeline
//! observed, and may emit events forward for later stages / observe hooks.
//! What a recorder does with that data (metering, charging, signed receipts,
//! blockchain anchoring, …) is deployment-specific and lives outside the SDK.

use async_trait::async_trait;
use std::sync::{Arc, Mutex};

use crate::caller::CallerContext;
use crate::error::BitrouterError;
use crate::error::Result;
use crate::event::{EventBus, PipelineEvent};
use crate::language_model::auth::ContinuationAuthority;
use crate::language_model::protocol::responses::CausalPrefixCommitment;
use crate::language_model::timing::FirstTokenKind;
use crate::language_model::types::{
    ApiProtocol, FinishReason, ReasoningEffort, RoutingTarget, UsageOrigin,
};

/// Success-only lifecycle data supplied to required finalizers before a
/// response is allowed to advertise successful completion.
#[derive(Clone)]
pub struct RequiredFinalizationContext {
    /// Stable gateway request and public Responses continuation identity.
    pub request_id: String,
    /// Process-unique request attempt used to own provisional finalizer state.
    pub delivery_attempt_id: u64,
    /// Authenticated caller whose ownership scopes any durable result.
    pub caller: CallerContext,
    /// Exact final target that served the response.
    pub target: Option<RoutingTarget>,
    /// Public logical model selector resolved for the successful request.
    pub effective_model: String,
    /// Canonical effort that produced the continuation, when present.
    pub effective_effort: Option<ReasoningEffort>,
    /// Fixed-size rolling commitment to the delivered causal message prefix.
    pub causal_prefix_commitment: Option<CausalPrefixCommitment>,
    /// Inbound client protocol.
    pub inbound_protocol: Option<ApiProtocol>,
    /// Final provider response id, when the serving protocol supplied one.
    pub response_id: Option<String>,
    /// Canonical terminal reason.
    pub finish_reason: Option<FinishReason>,
    /// Whether this was the streaming pipeline.
    pub streamed: bool,
    /// True only when the pipeline observed a clean successful terminal.
    pub successful_terminal: bool,
    /// True only when a streamed native Responses provider supplied the clean
    /// terminal completion. Router-synthetic finishes never set this proof.
    pub native_response_completed: bool,
    /// Redaction-safe stable authority returned with the exact authenticated
    /// transport request that produced this response.
    pub credential_authority: Option<ContinuationAuthority>,
}

/// Opaque identity for one required-finalizer invocation.
///
/// The pipeline creates a fresh receipt for every call and carries that same
/// identity through prepare, commit, and every rollback path. Finalizers may
/// retain a clone to prove that later compensation belongs to the invocation
/// that actually acquired provisional ownership.
#[derive(Clone)]
pub struct RequiredFinalizationReceipt(Arc<()>);

impl RequiredFinalizationReceipt {
    pub(crate) fn new() -> Self {
        Self(Arc::new(()))
    }

    /// Whether both receipts name the same finalizer invocation.
    pub fn same_invocation(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// The synchronous delivery rendezvous handed to a required finalizer after
/// its provisional work has completed. A finalizer with post-acknowledgement
/// durable work may use [`Self::wait_for_delivery_acknowledgement`], announce
/// its result with [`Self::complete_activation`], and retain rollback ownership
/// until [`Self::wait_for_terminal_commit`] confirms the returning poll.
#[derive(Clone)]
pub struct RequiredDeliveryHandshake {
    inner: Arc<RequiredDeliveryHandshakeInner>,
}

struct RequiredDeliveryHandshakeInner {
    ready: Mutex<Option<tokio::sync::oneshot::Sender<Result<()>>>>,
    acknowledged:
        tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<DeliveryAcknowledgement>>>,
    activation_completed: Mutex<Option<tokio::sync::oneshot::Sender<Result<()>>>>,
    terminal_committed: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

/// Downstream's proof that an activated payload was delivered or replaced by
/// a concrete server-side failure. Channel closure alone means disconnect.
#[derive(Debug)]
pub enum DeliveryAcknowledgement {
    /// Downstream is ready to return the successful payload once the
    /// finalizer's post-acknowledgement activation completes.
    Delivered,
    /// Rendering or wire encoding failed before the successful payload could
    /// be returned.
    Failed(BitrouterError),
}

impl RequiredDeliveryHandshake {
    /// Construct the two ends of a required-delivery rendezvous.
    pub fn new(
        ready: tokio::sync::oneshot::Sender<Result<()>>,
        acknowledged: tokio::sync::oneshot::Receiver<DeliveryAcknowledgement>,
    ) -> Self {
        Self::with_completion(ready, acknowledged, None, None)
    }

    /// Construct a drop-aware rendezvous whose delivery side waits for durable
    /// activation and commits to return the terminal in the same final poll.
    pub fn new_with_completion(
        ready: tokio::sync::oneshot::Sender<Result<()>>,
        acknowledged: tokio::sync::oneshot::Receiver<DeliveryAcknowledgement>,
        activation_completed: tokio::sync::oneshot::Sender<Result<()>>,
        terminal_committed: tokio::sync::oneshot::Receiver<()>,
    ) -> Self {
        Self::with_completion(
            ready,
            acknowledged,
            Some(activation_completed),
            Some(terminal_committed),
        )
    }

    fn with_completion(
        ready: tokio::sync::oneshot::Sender<Result<()>>,
        acknowledged: tokio::sync::oneshot::Receiver<DeliveryAcknowledgement>,
        activation_completed: Option<tokio::sync::oneshot::Sender<Result<()>>>,
        terminal_committed: Option<tokio::sync::oneshot::Receiver<()>>,
    ) -> Self {
        Self {
            inner: Arc::new(RequiredDeliveryHandshakeInner {
                ready: Mutex::new(Some(ready)),
                acknowledged: tokio::sync::Mutex::new(Some(acknowledged)),
                activation_completed: Mutex::new(activation_completed),
                terminal_committed: tokio::sync::Mutex::new(terminal_committed),
            }),
        }
    }

    /// Advertise readiness, wait for downstream's synchronous acknowledgment,
    /// and release a delivery permit that has no post-acknowledgement work.
    pub async fn wait_for_delivery(&self) -> Result<bool> {
        let result = self.wait_for_delivery_acknowledgement().await;
        if !self.complete_activation(Ok(())) {
            return match result {
                Ok(_) => Ok(false),
                Err(error) => Err(error),
            };
        }
        if !self.wait_for_terminal_commit().await {
            return match result {
                Ok(_) => Ok(false),
                Err(error) => Err(error),
            };
        }
        result
    }

    /// Advertise pre-delivery readiness and wait for downstream's synchronous
    /// acknowledgment without yet releasing the successful terminal.
    pub async fn wait_for_delivery_acknowledgement(&self) -> Result<bool> {
        let ready = match self.inner.ready.lock() {
            Ok(mut ready) => ready.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if ready.is_none_or(|ready| ready.send(Ok(())).is_err()) {
            return Ok(false);
        }
        let acknowledged = self.inner.acknowledged.lock().await.take();
        match acknowledged {
            Some(acknowledged) => match acknowledged.await {
                Ok(DeliveryAcknowledgement::Delivered) => Ok(true),
                Ok(DeliveryAcknowledgement::Failed(error)) => Err(error),
                Err(_) => Ok(false),
            },
            None => Ok(false),
        }
    }

    /// Announce that post-acknowledgement activation or compensation finished.
    /// `false` means the delivery future was dropped before observing it.
    pub fn complete_activation(&self, result: Result<()>) -> bool {
        let activation_completed = match self.inner.activation_completed.lock() {
            Ok(mut completed) => completed.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        activation_completed.is_none_or(|completed| completed.send(result).is_ok())
    }

    /// Wait until the delivery future has observed completed activation and,
    /// in the same poll with no intervening await, committed to return the
    /// terminal. Channel closure means activation must be compensated.
    pub async fn wait_for_terminal_commit(&self) -> bool {
        match self.inner.terminal_committed.lock().await.take() {
            Some(committed) => committed.await.is_ok(),
            None => true,
        }
    }

    /// Reject activation before a payload can be acknowledged.
    pub fn reject(&self, error: BitrouterError) {
        let ready = match self.inner.ready.lock() {
            Ok(mut ready) => ready.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(ready) = ready {
            let _ = ready.send(Err(error.clone()));
        }
        let _ = self.complete_activation(Err(error));
    }
}

/// A success-critical finalizer. Unlike ordinary settlement recorders, an
/// error is propagated before the caller can receive a successful terminal.
#[async_trait]
pub trait RequiredFinalizer: Send + Sync {
    /// Finalize durable success state.
    async fn finalize(&self, ctx: &RequiredFinalizationContext) -> Result<()>;

    /// Receipt-aware preparation used by the production pipeline.
    ///
    /// The default preserves source and behavior compatibility for finalizers
    /// that do not own provisional state per invocation.
    async fn finalize_with_receipt(
        &self,
        ctx: &RequiredFinalizationContext,
        _receipt: &RequiredFinalizationReceipt,
    ) -> Result<()> {
        self.finalize(ctx).await
    }

    /// Compensate a finalizer that started before downstream delivery was
    /// cancelled. Default is a no-op for finalizers with no provisional state.
    async fn rollback(&self, _ctx: &RequiredFinalizationContext) -> Result<()> {
        Ok(())
    }

    /// Receipt-aware compensation used by every production rollback path.
    async fn rollback_with_receipt(
        &self,
        ctx: &RequiredFinalizationContext,
        _receipt: &RequiredFinalizationReceipt,
    ) -> Result<()> {
        self.rollback(ctx).await
    }

    /// Activate provisional state and wait for the actual downstream delivery
    /// acknowledgment. Returning `false` means the receiver disappeared and
    /// the caller will invoke [`Self::rollback`].
    async fn commit(
        &self,
        _ctx: &RequiredFinalizationContext,
        delivery: &RequiredDeliveryHandshake,
    ) -> Result<bool> {
        delivery.wait_for_delivery().await
    }

    /// Receipt-aware activation used by the production pipeline.
    async fn commit_with_receipt(
        &self,
        ctx: &RequiredFinalizationContext,
        receipt: &RequiredFinalizationReceipt,
        delivery: &RequiredDeliveryHandshake,
    ) -> Result<bool> {
        let _ = receipt;
        self.commit(ctx, delivery).await
    }

    /// Drain and stop any bounded background work owned by this finalizer.
    /// The default is a no-op for finalizers without background lifecycle.
    async fn drain_pending_work(&self) -> Result<()> {
        Ok(())
    }
}

/// The Settlement-stage view, borrowed from `PipelineContext`. Carries
/// pipeline-observed data only — no charging / funding fields. Deployments
/// that need those compute them inside their own [`SettlementRecorder`]
/// impls.
pub struct SettlementContext {
    /// The request id.
    pub request_id: String,
    /// The caller.
    pub caller: CallerContext,
    /// The target that served the request, or the latest attempted target when
    /// execution failed before producing an [`ExecutionResult`](crate::language_model::ExecutionResult).
    pub target: Option<RoutingTarget>,
    /// Resolved model id, including the attempted model on execution failure.
    pub model_id: String,
    /// Canonical effort applied to the settled request, when present.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Resolved provider id, including the attempted provider on execution
    /// failure.
    pub provider_id: String,
    /// Which account of a multi-account provider served the request —
    /// `None` for a single-credential provider. Reflects any failover
    /// hop.
    pub account_label: Option<String>,
    /// Prompt tokens consumed.
    pub prompt_tokens: u64,
    /// Completion tokens consumed.
    pub completion_tokens: u64,
    /// Reasoning tokens consumed.
    pub reasoning_tokens: u64,
    /// Cache-read prompt tokens — already-cached content served from cache.
    /// Subset of `prompt_tokens`. Lets a recorder apply discounted pricing
    /// (e.g. Anthropic cache-read at 0.1× the prompt rate).
    pub cache_read_tokens: u64,
    /// Cache-write prompt tokens — content written to the cache this turn.
    /// Subset of `prompt_tokens`. Lets a recorder apply premium pricing
    /// (e.g. Anthropic cache-write at 1.25× the prompt rate).
    pub cache_write_tokens: u64,
    /// Whether usage was reported by the provider, estimated, or unavailable.
    pub usage_origin: UsageOrigin,
    /// Original provider usage object, when the provider reported one.
    pub raw_usage: Option<serde_json::Value>,
    /// Provider-executed web searches (from `Usage::web_search_count`).
    pub web_search_count: u64,
    /// Media content blocks in the request prompt.
    pub media_input_count: u64,
    /// Media content blocks in the response.
    pub media_output_count: u64,
    /// Server-tool calls observed (router + provider). Observability only.
    pub server_tool_calls: Vec<crate::language_model::types::ServerToolCall>,
    /// Whether the request was streamed.
    pub streamed: bool,
    /// End-to-end request duration in milliseconds.
    pub request_duration_ms: u64,
    /// Time spent in the final provider-facing operation.
    pub upstream_duration_ms: Option<u64>,
    /// Time from the successful provider attempt start to the first semantic
    /// stream delta.
    pub ttft_ms: Option<u64>,
    /// Time from the first to the last semantic stream delta.
    pub generation_duration_ms: Option<u64>,
    /// Kind of the first semantic stream delta.
    pub first_token_kind: Option<FirstTokenKind>,
    /// Canonical reason a successful generation ended.
    pub finish_reason: Option<FinishReason>,
    /// The error, if the request failed (Settlement still runs).
    pub error: Option<BitrouterError>,
    /// Events carried over from the request lifecycle (so recorders can
    /// inspect events emitted by earlier stages).
    ///
    /// `pub` so external test code can construct a context directly when
    /// exercising a recorder in isolation; production recorders should read
    /// through [`Self::has_event`] / [`Self::get_event`] /
    /// [`Self::get_events`] rather than poking the bus directly.
    pub events: EventBus,
}

impl SettlementContext {
    /// Emit a typed event from within the Settlement stage.
    pub fn emit<E: PipelineEvent>(&mut self, event: E) {
        self.events.emit(event);
    }

    /// Whether an event of type `E` was emitted anywhere in this request.
    pub fn has_event<E: PipelineEvent>(&self) -> bool {
        self.events.has::<E>()
    }

    /// The first emitted event of type `E`.
    pub fn get_event<E: PipelineEvent>(&self) -> Option<&E> {
        self.events.get::<E>()
    }

    /// All emitted events of type `E`.
    pub fn get_events<E: PipelineEvent>(&self) -> Vec<&E> {
        self.events.get_all::<E>()
    }
}

/// A bookkeeping recorder. Registered into an **always-run** list: every
/// recorder runs for every request (success or failure). Deployments use
/// recorders to write metering events, charge ledgers, sign receipts, etc.
#[async_trait]
pub trait SettlementRecorder: Send + Sync {
    /// Record the request outcome.
    ///
    /// `ctx` is `&mut` so a recorder may also [`SettlementContext::emit`]
    /// events forward (e.g. cloud-computed span attributes) for later stages
    /// and observe hooks: [`PipelineContext::absorb_settlement`] folds the
    /// settlement bus back into the request bus before `on_request_end`.
    ///
    /// [`PipelineContext::absorb_settlement`]: crate::language_model::PipelineContext::absorb_settlement
    async fn record(&self, ctx: &mut SettlementContext) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caller::CallerContext;
    use crate::event::EventBus;
    use crate::language_model::types::{ServerToolCall, ServerToolKind, ServerToolStatus};

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn required_finalization_receipt_is_cloneable_send_sync_invocation_identity() {
        assert_send_sync::<RequiredFinalizationReceipt>();
        let first = RequiredFinalizationReceipt::new();
        let first_clone = first.clone();
        let second = RequiredFinalizationReceipt::new();
        assert!(first.same_invocation(&first_clone));
        assert!(!first.same_invocation(&second));
    }

    fn make_settlement_context() -> SettlementContext {
        SettlementContext {
            request_id: "test-req".into(),
            caller: CallerContext::local(),
            target: None,
            model_id: "test-model".into(),
            reasoning_effort: None,
            provider_id: "test-provider".into(),
            account_label: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            reasoning_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            usage_origin: UsageOrigin::Unknown,
            raw_usage: None,
            web_search_count: 0,
            media_input_count: 0,
            media_output_count: 0,
            server_tool_calls: Vec::new(),
            streamed: false,
            request_duration_ms: 0,
            upstream_duration_ms: None,
            ttft_ms: None,
            generation_duration_ms: None,
            first_token_kind: None,
            finish_reason: None,
            error: None,
            events: EventBus::default(),
        }
    }

    #[test]
    fn settlement_context_carries_server_tool_signals() {
        let mut c = make_settlement_context();
        c.web_search_count = 3;
        c.media_output_count = 1;
        c.server_tool_calls = vec![ServerToolCall {
            name: "web_search".into(),
            kind: ServerToolKind::Provider,
            call_id: None,
            status: ServerToolStatus::Ok,
            result_count: 3,
        }];
        assert_eq!(c.web_search_count, 3);
        assert_eq!(c.media_output_count, 1);
        assert_eq!(c.server_tool_calls.len(), 1);
    }
}
