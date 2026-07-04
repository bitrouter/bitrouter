//! Telemetry hook — real consumer of `acp::Pipeline` that emits a
//! [`RequestCompleted`] record on every successful ACP turn.
//!
//! ## Token accounting (v1 limitation)
//!
//! `PromptResponse` carries no token-usage fields at the stable feature level
//! (the `usage` field is behind the `unstable_end_turn_token_usage` crate
//! feature, which bitrouter-substrate does not enable). Token counts are
//! delivered via `session/update UsageUpdate` notifications on the callback
//! plane (`UpstreamConnection::subscribe_updates`), not in the prompt result.
//! Until a follow-up wires `UsageUpdate` into the hook context,
//! `prompt_tokens` and `completion_tokens` are emitted as `0` (best-effort).
//!
//! ## Per-turn latency (v1 limitation)
//!
//! `ExecutionHook::on_success` does not receive a pre-turn timestamp; the
//! pipeline currently has no `PreRequestHook` that stashes a start time in
//! `AcpContext`. `latency_ms` is emitted as `0` until a follow-up adds a
//! timing pre-request hook that records `Instant::now()` into the context.

use async_trait::async_trait;
use bitrouter_sdk::acp::{AcpContext, AcpResponse, ExecutionHook};
use bitrouter_sdk::error::Result;
use tokio::sync::mpsc::UnboundedSender;

/// A completed ACP request-plane turn, emitted by [`TelemetryHook`] after each
/// successful `session/prompt` execution.
#[derive(Debug, Clone)]
pub struct RequestCompleted {
    /// The agent name that handled the turn.
    pub agent: String,
    /// The stop reason rendered as a string (e.g. `"EndTurn"`, `"MaxTokens"`).
    pub stop_reason: String,
    /// Input (prompt) tokens for this turn.
    ///
    /// **Always `0` in v1.** Token usage arrives via `session/update
    /// UsageUpdate` notifications, not the `PromptResponse`. A follow-up will
    /// wire `UsageUpdate` into the telemetry pipeline.
    pub prompt_tokens: u64,
    /// Output (completion) tokens for this turn.
    ///
    /// **Always `0` in v1.** See [`prompt_tokens`](Self::prompt_tokens).
    pub completion_tokens: u64,
    /// Wall-clock latency for the turn in milliseconds.
    ///
    /// **Always `0` in v1.** The pipeline does not yet stash a per-turn start
    /// timestamp in `AcpContext`. A follow-up timing `PreRequestHook` will
    /// record `Instant::now()` and populate this field.
    pub latency_ms: u32,
}

/// Records [`RequestCompleted`] events on every successful ACP execution turn.
///
/// Implements [`ExecutionHook`] so it is registered on an `acp::Pipeline` and
/// keeps the pipeline load-bearing (D5): every successful turn produces an
/// observable side-effect that the engine/CLI can consume via the channel.
pub struct TelemetryHook {
    sender: UnboundedSender<RequestCompleted>,
}

impl TelemetryHook {
    /// Create a hook that sends [`RequestCompleted`] events on `sender`.
    ///
    /// The companion receiver is typically created by the caller with
    /// `tokio::sync::mpsc::unbounded_channel()` and consumed by the engine
    /// or CLI to emit metrics, logs, or billing records.
    pub fn new(sender: UnboundedSender<RequestCompleted>) -> Self {
        Self { sender }
    }
}

#[async_trait]
impl ExecutionHook for TelemetryHook {
    async fn on_success(&self, ctx: &AcpContext, response: &AcpResponse) -> Result<()> {
        let record = RequestCompleted {
            agent: ctx.request().agent.clone(),
            stop_reason: format!("{:?}", response.result.stop_reason),
            // Token usage is not available on PromptResponse at the stable
            // feature level; see module-level doc for the follow-up path.
            prompt_tokens: 0,
            completion_tokens: 0,
            // Per-turn latency requires a timing PreRequestHook; see module doc.
            latency_ms: 0,
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
    use agent_client_protocol_schema::v1::{PromptResponse, StopReason};
    use bitrouter_sdk::acp::{
        AcpContext, AcpRequest, AcpRequestPayload, AcpResponse, ExecutionHook,
    };
    use bitrouter_sdk::caller::CallerContext;
    use tokio::sync::mpsc;

    use super::TelemetryHook;

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
        let hook = TelemetryHook::new(tx);

        let (ctx, response) = make_context_and_response("my-agent", StopReason::EndTurn);
        hook.on_success(&ctx, &response).await.expect("on_success");

        let record = rx.try_recv().expect("should have received a record");
        assert_eq!(record.agent, "my-agent");
        assert_eq!(record.stop_reason, "EndTurn");
        // Token / latency fields are 0 in v1 (best-effort placeholders).
        assert_eq!(record.prompt_tokens, 0);
        assert_eq!(record.completion_tokens, 0);
        assert_eq!(record.latency_ms, 0);
    }

    #[tokio::test]
    async fn telemetry_hook_closed_receiver_does_not_error() {
        let (tx, rx) = mpsc::unbounded_channel();
        // Drop the receiver — the hook must not propagate an error.
        drop(rx);
        let hook = TelemetryHook::new(tx);

        let (ctx, response) = make_context_and_response("my-agent", StopReason::MaxTokens);
        hook.on_success(&ctx, &response)
            .await
            .expect("on_success with closed receiver must be Ok");
    }
}
