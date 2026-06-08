//! Settlement stage for the `language_model` protocol — the always-run
//! [`SettlementRecorder`] list.
//!
//! The SDK is opinionated only about *pipeline data correctness*: a recorder
//! receives the final token / latency / model / error data the pipeline
//! observed, and may emit events forward for later stages / observe hooks.
//! What a recorder does with that data (metering, charging, signed receipts,
//! blockchain anchoring, …) is deployment-specific and lives outside the SDK.

use async_trait::async_trait;

use crate::caller::CallerContext;
use crate::error::BitrouterError;
use crate::error::Result;
use crate::event::{EventBus, PipelineEvent};
use crate::language_model::types::RoutingTarget;

/// The Settlement-stage view, borrowed from `PipelineContext`. Carries
/// pipeline-observed data only — no charging / funding fields. Deployments
/// that need those compute them inside their own [`SettlementRecorder`]
/// impls.
pub struct SettlementContext {
    /// The request id.
    pub request_id: String,
    /// The caller.
    pub caller: CallerContext,
    /// The target that actually served the request.
    pub target: Option<RoutingTarget>,
    /// Resolved model id.
    pub model_id: String,
    /// Resolved provider id.
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
    /// Whether the request was streamed.
    pub streamed: bool,
    /// End-to-end latency in milliseconds.
    pub latency_ms: u64,
    /// Upstream generation time in milliseconds.
    pub generation_time_ms: u64,
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
