//! Telemetry hook — real consumer of `acp::Pipeline` that emits a
//! [`RequestCompleted`] record on every successful ACP turn.
//!
//! ## Token accounting
//!
//! ACP's **stable** usage signal is `session/update UsageUpdate`, which reports
//! context-window occupancy (`used`/`size` tokens, optional cumulative cost) —
//! not per-turn input/output deltas (those exist only behind the schema crate's
//! `unstable_end_turn_token_usage` feature, which this workspace does not
//! enable). The upstream connection records the latest `UsageUpdate` into a
//! [`SharedContextUsage`] slot; the hook snapshots it into each record.
//!
//! ## Per-turn latency
//!
//! `AcpContext` stamps `started_at` at pipeline entry; `on_success` derives
//! `latency_ms` from it. This spans PreRequest → Route → Execute, i.e. the
//! whole turn as the pipeline sees it.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bitrouter_sdk::acp::{AcpContext, AcpResponse, ExecutionHook};
use bitrouter_sdk::error::Result;
use tokio::sync::mpsc::UnboundedSender;

/// Context-window occupancy reported by the upstream's latest `UsageUpdate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsage {
    /// Tokens currently in context.
    pub used: u64,
    /// Total context-window size in tokens.
    pub size: u64,
}

/// Shared slot holding the latest [`ContextUsage`]; written by the upstream
/// connection's `session/update` handler, read by [`TelemetryHook`].
pub type SharedContextUsage = Arc<Mutex<Option<ContextUsage>>>;

/// A completed ACP request-plane turn, emitted by [`TelemetryHook`] after each
/// successful `session/prompt` execution.
#[derive(Debug, Clone)]
pub struct RequestCompleted {
    /// The agent name that handled the turn.
    pub agent: String,
    /// The stop reason rendered as a string (e.g. `"EndTurn"`, `"MaxTokens"`).
    pub stop_reason: String,
    /// Wall-clock latency for the turn in milliseconds (pipeline entry →
    /// post-execute).
    pub latency_ms: u64,
    /// Context-window occupancy as of the latest upstream `UsageUpdate`, when
    /// the upstream has reported one.
    pub context: Option<ContextUsage>,
}

/// Records [`RequestCompleted`] events on every successful ACP execution turn.
///
/// Implements [`ExecutionHook`] so it is registered on an `acp::Pipeline` and
/// keeps the pipeline load-bearing (D5): every successful turn produces an
/// observable side-effect that the engine/CLI can consume via the channel.
pub struct TelemetryHook {
    sender: UnboundedSender<RequestCompleted>,
    usage: SharedContextUsage,
}

impl TelemetryHook {
    /// Create a hook that sends [`RequestCompleted`] events on `sender`,
    /// snapshotting `usage` (the upstream connection's latest context-usage
    /// slot) into each record.
    ///
    /// The companion receiver is typically created by the caller with
    /// `tokio::sync::mpsc::unbounded_channel()` and consumed by the engine
    /// or CLI to emit metrics, logs, or billing records.
    pub fn new(sender: UnboundedSender<RequestCompleted>, usage: SharedContextUsage) -> Self {
        Self { sender, usage }
    }
}

#[async_trait]
impl ExecutionHook for TelemetryHook {
    async fn on_success(&self, ctx: &AcpContext, response: &AcpResponse) -> Result<()> {
        let record = RequestCompleted {
            agent: ctx.request().agent.clone(),
            stop_reason: format!("{:?}", response.result.stop_reason),
            latency_ms: u64::try_from(ctx.started_at().elapsed().as_millis())
                .unwrap_or(u64::MAX),
            context: self.usage.lock().ok().and_then(|g| *g),
        };
        // A closed receiver means the consumer has gone away; treat it as a
        // non-fatal telemetry gap — do not propagate an error that would abort
        // the pipeline turn.
        let _ = self.sender.send(record);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use agent_client_protocol_schema::v1::{PromptResponse, StopReason};
    use bitrouter_sdk::acp::{
        AcpContext, AcpRequest, AcpRequestPayload, AcpResponse, ExecutionHook,
    };
    use bitrouter_sdk::caller::CallerContext;
    use tokio::sync::mpsc;

    use super::{ContextUsage, TelemetryHook};

    fn make_context_and_response(
        agent: &str,
        stop_reason: StopReason,
    ) -> (AcpContext, AcpResponse) {
        let req = AcpRequest::new(
            agent,
            AcpRequestPayload::Cancel {
                session_id: "s1".to_string(),
            },
            CallerContext::new("k", "u"),
        );
        let request_id = req.request_id.clone();
        let ctx = AcpContext::new(req);
        let response = AcpResponse {
            request_id,
            result: PromptResponse::new(stop_reason),
        };
        (ctx, response)
    }

    #[tokio::test]
    async fn telemetry_hook_emits_request_completed_on_success() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let usage = Arc::new(Mutex::new(Some(ContextUsage {
            used: 1500,
            size: 200_000,
        })));
        let hook = TelemetryHook::new(tx, usage);

        let (ctx, response) = make_context_and_response("my-agent", StopReason::EndTurn);
        hook.on_success(&ctx, &response).await.expect("on_success");

        let record = rx.try_recv().expect("should have received a record");
        assert_eq!(record.agent, "my-agent");
        assert_eq!(record.stop_reason, "EndTurn");
        // The latest context usage is snapshotted into the record.
        assert_eq!(
            record.context,
            Some(ContextUsage {
                used: 1500,
                size: 200_000,
            })
        );
    }

    #[tokio::test]
    async fn telemetry_hook_without_usage_reports_none() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let hook = TelemetryHook::new(tx, Arc::new(Mutex::new(None)));

        let (ctx, response) = make_context_and_response("my-agent", StopReason::MaxTokens);
        hook.on_success(&ctx, &response).await.expect("on_success");

        let record = rx.try_recv().expect("record");
        assert_eq!(record.context, None);
    }

    #[tokio::test]
    async fn telemetry_hook_closed_receiver_does_not_error() {
        let (tx, rx) = mpsc::unbounded_channel();
        // Drop the receiver — the hook must not propagate an error.
        drop(rx);
        let hook = TelemetryHook::new(tx, Arc::new(Mutex::new(None)));

        let (ctx, response) = make_context_and_response("my-agent", StopReason::MaxTokens);
        hook.on_success(&ctx, &response)
            .await
            .expect("on_success with closed receiver must be Ok");
    }
}
