//! The adequacy observe hook — writes the ledger from each request's outcome.
//!
//! Registered as an [`ObserveHook`], it runs after every request. On
//! `on_request_end` it recomputes the request fingerprint, maps the served model
//! back to its tier, and — only for a *downgrade* (a tier other than the
//! escalation tier) — records whether the outcome was a hard failure. Repeated
//! failures on a downgrade pin the fingerprint in the [`AdequacyLedger`], so
//! future requests with that fingerprint escalate.

use std::sync::Arc;

use async_trait::async_trait;

use bitrouter_sdk::language_model::types::StreamPart;
use bitrouter_sdk::language_model::{
    ObserveHook, Phase, PipelineContext, RequestOutcome, StreamContext,
};

use crate::adequacy::AdequacyLedger;
use crate::policy_table_router::PolicyTable;

/// Feeds the [`AdequacyLedger`] from request outcomes against the shared
/// [`PolicyTable`].
pub struct AdequacyObserveHook {
    table: Arc<PolicyTable>,
    ledger: Arc<AdequacyLedger>,
}

impl AdequacyObserveHook {
    /// Build the hook over the shared policy table and ledger.
    pub fn new(table: Arc<PolicyTable>, ledger: Arc<AdequacyLedger>) -> Self {
        Self { table, ledger }
    }
}

#[async_trait]
impl ObserveHook for AdequacyObserveHook {
    // Per-phase and per-stream-part observation are unused — all the work is at
    // request end, where the served model and final outcome are both known.
    async fn after_phase(&self, _phase: Phase, _ctx: &PipelineContext) {}

    async fn on_stream_part(&self, _ctx: &StreamContext, _part: &StreamPart) {}

    async fn on_request_end(&self, ctx: &PipelineContext, outcome: &RequestOutcome) {
        // Credit the outcome only to a *genuine* policy-router downgrade: the
        // served model must map to exactly the tier the static table resolves
        // for this request (so a caller's explicit route, a coincidental model
        // match, or an adequacy escalation is not mistaken for a downgrade)...
        let Some(served_tier) = self.table.tier_of_model(ctx.model()) else {
            return;
        };
        if self.table.static_tier(ctx.prompt()) != Some(served_tier) {
            return;
        }
        // ...and that tier must be a downgrade, not the escalation tier (a
        // request already escalated failing is not a downgrade to pin).
        if Some(served_tier) == self.table.escalation_tier() {
            return;
        }
        let inadequate = match outcome {
            // A hard failure — an upstream error (including a mid-stream stream
            // error, which the pipeline now surfaces as `Failed`), a route /
            // auth / policy failure, etc.
            RequestOutcome::Failed(_) => true,
            // The client hanging up tells us nothing about the tier — skip.
            RequestOutcome::ClientDisconnected => return,
            // A completed request got a response: the downgrade held.
            RequestOutcome::Completed => false,
        };
        let fingerprint = PolicyTable::fingerprint(ctx.prompt());
        self.ledger.observe(&fingerprint, inadequate).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use bitrouter_sdk::BitrouterError;
    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::config::PolicyTableConfig;
    use bitrouter_sdk::language_model::types::{
        Content, GenerationParams, Message, PipelineRequest, Prompt, ProviderMetadata, Role,
    };

    // A table: `opening` → capable (= the escalation tier, via default_tier),
    // `after_read_file` → cheap (a downgrade).
    fn table() -> Arc<PolicyTable> {
        let cfg = PolicyTableConfig {
            tiers: HashMap::from([
                ("cheap".to_string(), "vendor/cheap".to_string()),
                ("capable".to_string(), "vendor/capable".to_string()),
            ]),
            fingerprints: HashMap::from([
                ("opening".to_string(), "capable".to_string()),
                ("after_read_file".to_string(), "cheap".to_string()),
            ]),
            default_tier: Some("capable".to_string()),
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
            adequacy: Default::default(),
        };
        PolicyTable::from_config(&cfg).expect("configured")
    }

    fn user(text: &str) -> Message {
        Message::text(Role::User, text)
    }

    fn assistant_calls(tool: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![Content::ToolCall {
                id: format!("call_{tool}"),
                name: tool.to_string(),
                arguments: "{}".to_string(),
                provider_executed: false,
                dynamic: false,
                provider_metadata: ProviderMetadata::new(),
            }],
        }
    }

    fn prompt(messages: Vec<Message>) -> Prompt {
        Prompt {
            model: String::new(),
            system: None,
            system_provider_metadata: ProviderMetadata::new(),
            messages,
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    /// A context for a request the pipeline served on `served_model`.
    fn ctx(served_model: &str, messages: Vec<Message>) -> PipelineContext {
        let request = PipelineRequest::new(
            served_model.to_string(),
            CallerContext::new("k", "u"),
            prompt(messages),
        );
        PipelineContext::new(request)
    }

    fn failed() -> RequestOutcome {
        RequestOutcome::Failed(BitrouterError::internal("upstream boom"))
    }

    fn read_step() -> Vec<Message> {
        vec![user("fix the bug"), assistant_calls("read_file")]
    }

    #[tokio::test]
    async fn a_failed_downgrade_pins_the_fingerprint() {
        let ledger = Arc::new(AdequacyLedger::in_memory(1, 0));
        let hook = AdequacyObserveHook::new(table(), ledger.clone());
        // `after_read_file` → cheap is the static downgrade, and the request was
        // served by the cheap model — a genuine downgrade. A hard failure pins it.
        hook.on_request_end(&ctx("vendor/cheap", read_step()), &failed())
            .await;
        assert!(ledger.is_pinned("after_read_file"));
    }

    #[tokio::test]
    async fn a_failure_not_matching_the_static_downgrade_is_ignored() {
        // The over-attribution guard: an `opening` request served by the cheap
        // model (e.g. the caller routed there) is NOT the static decision
        // (opening → capable), so its failure must not pin anything.
        let ledger = Arc::new(AdequacyLedger::in_memory(1, 0));
        let hook = AdequacyObserveHook::new(table(), ledger.clone());
        hook.on_request_end(&ctx("vendor/cheap", vec![user("start")]), &failed())
            .await;
        assert!(!ledger.is_pinned("opening"));
        assert!(!ledger.is_pinned("after_read_file"));
    }

    #[tokio::test]
    async fn a_failure_on_the_escalation_tier_is_ignored() {
        // `opening` → capable, which is the escalation tier; a failure there is
        // not a downgrade to pin.
        let ledger = Arc::new(AdequacyLedger::in_memory(1, 0));
        let hook = AdequacyObserveHook::new(table(), ledger.clone());
        hook.on_request_end(&ctx("vendor/capable", vec![user("start")]), &failed())
            .await;
        assert!(!ledger.is_pinned("opening"));
    }

    #[tokio::test]
    async fn a_completed_downgrade_does_not_pin() {
        let ledger = Arc::new(AdequacyLedger::in_memory(1, 0));
        let hook = AdequacyObserveHook::new(table(), ledger.clone());
        hook.on_request_end(
            &ctx("vendor/cheap", read_step()),
            &RequestOutcome::Completed,
        )
        .await;
        assert!(!ledger.is_pinned("after_read_file"));
    }

    #[tokio::test]
    async fn a_client_disconnect_is_ignored() {
        let ledger = Arc::new(AdequacyLedger::in_memory(1, 0));
        let hook = AdequacyObserveHook::new(table(), ledger.clone());
        hook.on_request_end(
            &ctx("vendor/cheap", read_step()),
            &RequestOutcome::ClientDisconnected,
        )
        .await;
        assert!(!ledger.is_pinned("after_read_file"));
    }
}
