//! Settlement seam for the `acp` protocol — the always-run
//! [`AcpSettlementRecorder`] list, mirroring
//! [`language_model::settlement`](crate::language_model::settlement).
//!
//! **The SDK emits turn facts and nothing else.** An
//! [`AcpSettlementContext`] carries what the pipeline observed about one
//! completed turn: which agent ran it, how it stopped, how long it took, and
//! the cost-shaped signal ACP's stable surface exposes. It contains no notion
//! of a *good* turn — no score, no reward, no verdict. Deciding what a turn
//! was worth is deployment policy and lives in the consumer's recorder, the
//! same way `SettlementRecorder` keeps charging and metering out of the
//! `language_model` pipeline.
//!
//! Recorders run immediately after the [`ExecutionHook`](super::ExecutionHook)
//! stage, on success only — the ACP pipeline is pure routing and has no
//! failure settlement.

use async_trait::async_trait;

use crate::error::Result;

/// The cost-shaped facts ACP's **stable** surface exposes for a turn.
///
/// ACP reports context-window occupancy through `session/update UsageUpdate`,
/// not per-turn input/output token deltas (those sit behind the schema crate's
/// `unstable_end_turn_token_usage` feature, which this workspace does not
/// enable). So this is occupancy as of the upstream's latest report — a
/// consumer that needs money must price it itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcpCostFacts {
    /// Tokens in context after the turn.
    pub context_used: u64,
    /// Total context-window size in tokens.
    pub context_size: u64,
}

/// Facts observed for one completed ACP turn, handed to every registered
/// [`AcpSettlementRecorder`].
#[derive(Debug, Clone)]
pub struct AcpSettlementContext {
    /// The pipeline request id for this turn — the join key a consumer uses to
    /// correlate the turn with whatever else it recorded.
    pub turn_id: String,
    /// The agent that served the turn.
    pub agent: String,
    /// How the turn ended, as the ACP `StopReason` debug spelling (e.g.
    /// `"EndTurn"`, `"MaxTokens"`).
    pub stop_reason: String,
    /// Wall-clock latency for the turn in milliseconds (pipeline entry →
    /// post-execute).
    pub latency_ms: u64,
    /// Context-window facts, when the upstream has reported a `UsageUpdate`
    /// and the pipeline was given the connection's usage slot.
    pub cost_facts: Option<AcpCostFacts>,
}

/// A bookkeeping recorder for completed ACP turns. Registered into an
/// **always-run** list: every recorder runs for every successful turn.
///
/// Implementations map facts to whatever the deployment needs — a reward
/// signal, a metering event, a trace attribute. That mapping is deliberately
/// **not** in the SDK.
#[async_trait]
pub trait AcpSettlementRecorder: Send + Sync {
    /// Record one turn's facts.
    async fn record(&self, ctx: &AcpSettlementContext) -> Result<()>;
}
