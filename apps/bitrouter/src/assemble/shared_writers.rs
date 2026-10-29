//! Shared assembly excludes the local durable observation and accounting stores.

use async_trait::async_trait;

use crate::gateway::error::{BitrouterError, Result};
use crate::gateway::observability::decision::{EvalInvocation, PendingEvalDecisionStore};
use crate::gateway::pipeline::context::PipelineContext;
use crate::gateway::pipeline::hooks::{HookDecision, PreRequestHook};
use crate::gateway::pipeline::settlement::{SettlementContext, SettlementRecorder};

pub(super) fn validate_policies(policies: &[crate::policy::Policy]) -> anyhow::Result<()> {
    anyhow::ensure!(
        policies
            .iter()
            .all(|policy| policy.max_spend_micro_usd.is_none()
                && policy.max_requests_per_minute.is_none()),
        "shared access policy cannot use local spend/rate counters; configure shared admission limits"
    );
    Ok(())
}

/// Local capture/evolution and reserved judge admission have no shared authority.
/// Reject those claims before a removed hook could silently change routing.
pub(super) struct RequestScope;

impl RequestScope {
    fn validate(ctx: &PipelineContext) -> Result<HookDecision> {
        if ctx.headers().contains_key("x-bitrouter-controller-id") {
            return Err(BitrouterError::bad_request(
                "shared serving does not support local capture/evolution controller claims",
            ));
        }
        if ctx.request_id().starts_with("brjudge_") {
            return Err(BitrouterError::bad_request(
                "shared serving does not support local reserved judge requests",
            ));
        }
        Ok(HookDecision::Allow)
    }
}

#[async_trait]
impl PreRequestHook for RequestScope {
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision> {
        Self::validate(ctx)
    }

    async fn revalidate_context(&self, ctx: &PipelineContext) -> Result<HookDecision> {
        Self::validate(ctx)
    }
}

/// Release the optional process-local observation cache without a durable write.
/// Original projection evidence is retained independently by shared custody.
pub(super) struct DiscardPending {
    pending: PendingEvalDecisionStore,
}

impl DiscardPending {
    pub(super) fn new(pending: PendingEvalDecisionStore) -> Self {
        Self { pending }
    }
}

#[async_trait]
impl SettlementRecorder for DiscardPending {
    async fn record(&self, context: &mut SettlementContext) -> Result<()> {
        if let Some(invocation) = context.get_event::<EvalInvocation>() {
            self.pending.remove(invocation, context.caller.user_id());
        }
        Ok(())
    }
}
