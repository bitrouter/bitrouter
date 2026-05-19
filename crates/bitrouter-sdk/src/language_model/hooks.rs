//! The `language_model` hook traits.
//!
//! these are **not** shared across protocols — `mcp` /
//! `acp` define their own, independently. The full set here:
//! `PreRequestHook` / `RouteHook` / `ExecutionHook` / `StreamHook` /
//! `SettlementRecorder` / `ObserveHook`. `SettlementRecorder` lives in
//! [`crate::language_model::settlement`]; this module defines the rest.

use async_trait::async_trait;

use crate::error::{BitrouterError, Result};
use crate::language_model::context::{PipelineContext, StreamContext};
use crate::language_model::stream::{StreamAction, StreamInterest, StreamOutcome};
use crate::language_model::types::{ExecutionResult, RoutingTarget, StreamPart};

/// A PreRequest hook's verdict.
#[derive(Debug)]
pub enum HookDecision {
    /// Continue to the next hook / stage.
    Allow,
    /// Reject the request now; the pipeline stops.
    Deny(DenyReason),
}

/// Why a PreRequest hook denied a request. Maps to an HTTP status.
#[derive(Debug)]
pub enum DenyReason {
    /// 401.
    Unauthorized(String),
    /// 403.
    Forbidden(String),
    /// 402 — carries a human-readable challenge description.
    PaymentRequired(String),
    /// 429.
    RateLimited {
        /// Seconds to wait before retry.
        retry_after: Option<u64>,
    },
    /// 400 — content blocked by a guardrail.
    GuardrailViolation(String),
    /// 400 — generic bad request.
    BadRequest(String),
    /// Any other status + message.
    Custom(u16, String),
}

impl From<DenyReason> for BitrouterError {
    fn from(reason: DenyReason) -> Self {
        match reason {
            DenyReason::Unauthorized(m) => BitrouterError::Unauthorized(m),
            DenyReason::Forbidden(m) => BitrouterError::Forbidden(m),
            DenyReason::PaymentRequired(m) => BitrouterError::PaymentRequired(m),
            DenyReason::RateLimited { retry_after } => BitrouterError::RateLimited { retry_after },
            DenyReason::GuardrailViolation(m) => BitrouterError::BadRequest { message: m },
            DenyReason::BadRequest(m) => BitrouterError::BadRequest { message: m },
            DenyReason::Custom(status, m) => match status {
                401 => BitrouterError::Unauthorized(m),
                402 => BitrouterError::PaymentRequired(m),
                403 => BitrouterError::Forbidden(m),
                404 => BitrouterError::NotFound(m),
                429 => BitrouterError::RateLimited { retry_after: None },
                _ => BitrouterError::BadRequest { message: m },
            },
        }
    }
}

/// Stage 1 — pre-request checks (auth, policy, rate limit, balance, guardrails).
///
/// Hooks run in registration order; the first `Deny` stops the pipeline.
#[async_trait]
pub trait PreRequestHook: Send + Sync {
    /// Inspect the request and either allow it or deny it.
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision>;
}

/// Stage 2 — route resolution. Each hook may rewrite the whole fallback chain
/// and emit typed events / write metadata for downstream stages.
#[async_trait]
pub trait RouteHook: Send + Sync {
    /// Resolve / mutate the routing chain.
    async fn resolve(
        &self,
        chain: &mut Vec<RoutingTarget>,
        ctx: &mut PipelineContext,
    ) -> Result<()>;
}

/// Stage 3 — execution observation + fallback control.
#[async_trait]
pub trait ExecutionHook: Send + Sync {
    /// Called when an upstream attempt succeeds.
    async fn on_success(&self, ctx: &PipelineContext, result: &ExecutionResult) -> Result<()>;

    /// Called when an upstream attempt fails; decides whether to fall back.
    async fn on_failure(&self, ctx: &PipelineContext, error: &BitrouterError) -> FallbackDecision;
}

/// What to do after an upstream attempt fails.
#[derive(Debug)]
pub enum FallbackDecision {
    /// Try the next target in the chain.
    TryNext,
    /// Stop and fail with this error.
    Fail(BitrouterError),
}

/// The StreamHook stage — inline-awaited interception of the canonical
/// `StreamPart` stream, before outbound protocol conversion. Carries Guardrails
/// downstream rewriting and MPP per-checkpoint settlement.
#[async_trait]
pub trait StreamHook: Send + Sync {
    /// Which part kinds this hook wants. The pipeline only invokes the hook on
    /// matching parts, so per-delta cost scales with declared interest.
    fn interest(&self) -> StreamInterest;

    /// Called for each (interesting) stream part. May rewrite, drop or abort.
    async fn on_part(&self, ctx: &mut StreamContext, part: StreamPart) -> Result<StreamAction>;

    /// Called exactly once when the stream terminates — for any outcome.
    /// This is v0's `finally` block made explicit: MPP finalises its last
    /// checkpoint here, accumulated usage lands in the context for Settlement.
    async fn on_stream_end(&self, ctx: &mut StreamContext, outcome: &StreamOutcome) -> Result<()>;
}

/// Which pipeline stage just completed (passed to `ObserveHook::after_phase`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Stage 1.
    PreRequest,
    /// Stage 2.
    Route,
    /// Stage 3.
    Execution,
    /// Stage 4.
    Settlement,
}

/// How a request ultimately ended (passed to `ObserveHook::on_request_end`).
#[derive(Debug)]
pub enum RequestOutcome {
    /// Completed successfully.
    Completed,
    /// Failed with an error.
    Failed(BitrouterError),
    /// The client disconnected before completion.
    ClientDisconnected,
}

/// A cross-cutting, read-only observation hook. Invoked at every stage boundary
/// (including the StreamHook stage). It returns no decision, cannot mutate data,
/// and **errors / panics inside it never affect the request** — the pipeline
/// swallows them.
#[async_trait]
pub trait ObserveHook: Send + Sync {
    /// Called after each non-streaming stage completes.
    async fn after_phase(&self, phase: Phase, ctx: &PipelineContext);

    /// Which stream part kinds this hook wants observed. Defaults to none.
    fn stream_interest(&self) -> StreamInterest {
        StreamInterest::none()
    }

    /// Called for each (interesting) stream part — read-only.
    async fn on_stream_part(&self, ctx: &StreamContext, part: &StreamPart);

    /// Called exactly once after the request fully ends (success / failure /
    /// disconnect). Settlement data is final by this point.
    async fn on_request_end(&self, ctx: &PipelineContext, outcome: &RequestOutcome);
}
