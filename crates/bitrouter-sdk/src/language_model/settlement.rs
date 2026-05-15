//! Settlement for the `language_model` protocol: the `ChargeStrategy`
//! responsibility chain (mutually exclusive, first-claim-wins) and the
//! always-run `SettlementRecorder`. See design doc 003 §4.5.

use async_trait::async_trait;

use crate::caller::{CallerContext, FundingSource};
use crate::error::BitrouterError;
use crate::error::Result;
use crate::event::{EventBus, PipelineEvent};
use crate::language_model::types::RoutingTarget;

/// The Settlement-stage view, borrowed from `PipelineContext`.
///
/// `final_charge_micro_usd` / `funding_source` are written by whichever
/// `ChargeStrategy` claims; if none claims they stay at their defaults.
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
    /// Prompt tokens consumed.
    pub prompt_tokens: u64,
    /// Completion tokens consumed.
    pub completion_tokens: u64,
    /// Reasoning tokens consumed.
    pub reasoning_tokens: u64,
    /// Cache-read prompt tokens — already-cached content served from cache.
    /// Subset of `prompt_tokens`. Lets a `ChargeStrategy` apply discounted
    /// pricing (e.g. Anthropic cache-read at 0.1× the prompt rate).
    pub cache_read_tokens: u64,
    /// Cache-write prompt tokens — content written to the cache this turn.
    /// Subset of `prompt_tokens`. Lets a `ChargeStrategy` apply premium
    /// pricing (e.g. Anthropic cache-write at 1.25× the prompt rate).
    pub cache_write_tokens: u64,
    /// Whether the request was streamed.
    pub streamed: bool,
    /// End-to-end latency in milliseconds.
    pub latency_ms: u64,
    /// Upstream generation time in milliseconds.
    pub generation_time_ms: u64,
    /// Final charge in micro-USD. Written by the claiming `ChargeStrategy`;
    /// stays `0` if the chain is exhausted without a claim.
    pub final_charge_micro_usd: i64,
    /// Which funding source settled the request.
    pub funding_source: FundingSource,
    /// BYOK marker. Set by `ByokCharge` when it sees the `ByokKeyApplied`
    /// event — **never** inferred from `target.api_key_override.is_some()`
    /// (cloud #235 lesson).
    pub byok_used: bool,
    /// The error, if the request failed (Settlement still runs).
    pub error: Option<BitrouterError>,
    /// Events carried over from the request lifecycle (so `ChargeStrategy`s can
    /// read `ByokKeyApplied`, `Authenticated`, etc.).
    pub(crate) events: EventBus,
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

/// The outcome of a `ChargeStrategy` attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum ChargeOutcome {
    /// This strategy handled charging (including "BYOK free, charge = 0"). The
    /// responsibility chain stops here — no later strategy is tried.
    Claimed,
    /// This strategy does not apply; try the next one.
    Pass,
}

/// A charging decision. Registered into a **mutually exclusive responsibility
/// chain**: the pipeline tries strategies in registration order and `break`s on
/// the first `Claimed`. "Charge at most once" is enforced by that `break`
/// structure, not by hook-side `is_settled()` etiquette.
#[async_trait]
pub trait ChargeStrategy: Send + Sync {
    /// Attempt to claim and apply charging for this request.
    async fn try_charge(&self, ctx: &mut SettlementContext) -> Result<ChargeOutcome>;
}

/// A bookkeeping recorder. Registered into an **always-run** list: every
/// recorder runs for every request (success or failure, regardless of which
/// strategy charged).
#[async_trait]
pub trait SettlementRecorder: Send + Sync {
    /// Record the (already-decided) settlement outcome.
    async fn record(&self, ctx: &SettlementContext) -> Result<()>;
}
