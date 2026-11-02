//! Request-local ownership bridges the actual send gate to terminal observations.

use super::{
    key, order, refusal,
    state::{Admission, Key, Outcome, Runtime},
};
use crate::{
    gateway::{
        error::{BitrouterError, Result},
        pipeline::{
            context::{PipelineContext, StreamContext},
            hooks::{
                ExecutionHook, FallbackDecision, HookDecision, HopOutcome, ObserveHook, Phase,
                PreRequestHook, RequestOutcome, RouteHook,
            },
            stream::StreamInterest,
            types::{ExecutionResult, RoutingTarget},
        },
    },
    tenancy::execution::TenantExecution,
};
use async_trait::async_trait;
use bitrouter_ai::types::{StreamPart, UsageOrigin};
use std::sync::{Arc, Mutex, Weak};

#[cfg(test)]
mod processor_tests;

struct Active {
    key: Key,
    admission: Admission,
    output_tokens: Option<u64>,
}
struct Inner {
    execution: Weak<TenantExecution>,
    runtime: Arc<Runtime>,
    active: Mutex<Option<Active>>,
}

#[derive(Clone)]
pub(super) struct Profile(Arc<Inner>);

impl Profile {
    pub(super) fn new(execution: &Arc<TenantExecution>, runtime: Arc<Runtime>) -> Self {
        Self(Arc::new(Inner {
            execution: Arc::downgrade(execution),
            runtime,
            active: Mutex::new(None),
        }))
    }
    fn execution(&self) -> Result<Arc<TenantExecution>> {
        self.0
            .execution
            .upgrade()
            .ok_or_else(|| BitrouterError::Forbidden("tenant routing owner unavailable".into()))
    }
    fn attached_execution(&self, ctx: &PipelineContext) -> Result<Arc<TenantExecution>> {
        let execution = self.execution()?;
        if !ctx
            .extension::<TenantExecution>()
            .is_some_and(|attached| Arc::ptr_eq(&attached, &execution))
        {
            return Err(BitrouterError::Forbidden(
                "tenant routing execution mismatch".into(),
            ));
        }
        Ok(execution)
    }
    fn key(&self, target: &RoutingTarget) -> Result<Key> {
        let execution = self.execution()?;
        let credential = execution
            .route()
            .match_candidate(target)
            .map_err(|code| execution.reject(code))?;
        key(execution.installation_id(), &credential, target)
    }
    pub(super) fn admit(&self, ctx: &PipelineContext, target: &RoutingTarget) -> Result<SendGuard> {
        // Hooks may reconstruct a context after entry. Recheck its exact owner
        // at the send boundary; a copied routing profile is not send authority.
        let execution = self.attached_execution(ctx)?;
        execution
            .revalidate()
            .map_err(|code| execution.reject(code))?;
        let key = self.key(target)?;
        let metadata = execution
            .route()
            .routing_metadata(target)
            .map_err(|code| execution.reject(code))?;
        let mut active = self
            .0
            .active
            .lock()
            .map_err(|_| refusal(super::state::Refusal::Unavailable))?;
        if active.is_some() {
            return Err(BitrouterError::Forbidden(
                "prior routing attempt has no terminal evidence".into(),
            ));
        }
        let admission = self
            .0
            .runtime
            .admit(key.clone(), &metadata.limits())
            .map_err(refusal)?;
        *active = Some(Active {
            key,
            admission,
            output_tokens: None,
        });
        Ok(SendGuard {
            profile: self.clone(),
            committed: false,
        })
    }
    fn finish(&self, target: &RoutingTarget, outcome: Outcome) {
        // The admitted attempt owns this observation. Caller revocation blocks
        // new sends in admit(), but cannot erase an existing provider outcome.
        // Receipt settlement and continuation publication retain their own gates.
        let Ok(key) = self.key(target) else {
            return;
        };
        let Ok(mut slot) = self.0.active.lock() else {
            return;
        };
        if slot.as_ref().is_some_and(|active| active.key == key)
            && let Some(active) = slot.take()
        {
            active.admission.finish(outcome);
        }
    }
    pub(super) fn response_failed(&self, target: &RoutingTarget, error: &BitrouterError) {
        // This entry point is called only by the executor after observing an
        // actual non-success HTTP status. Generic hop/request errors can carry
        // synthetic 502 statuses for truncated bodies or SSE decoding failures;
        // those errors must never count as provider availability evidence.
        let outcome = match error {
            BitrouterError::Upstream {
                status: 500..=599, ..
            }
            | BitrouterError::UpstreamRateLimited { .. } => Outcome::Unavailable,
            BitrouterError::Upstream { .. }
            | BitrouterError::UpstreamBadRequest { .. }
            | BitrouterError::UpstreamPolicyViolation { .. }
            | BitrouterError::UpstreamPaymentRequired { .. }
            | BitrouterError::UpstreamAuth { .. } => Outcome::Neutral,
            _ => Outcome::Unknown,
        };
        self.finish(target, outcome);
    }
}

/// One unsent reservation. Only `sent` transfers ownership into the request.
pub(crate) struct SendGuard {
    profile: Profile,
    committed: bool,
}
impl SendGuard {
    /// After receipt arming and immediately before transport; no await between.
    pub(crate) fn sent(mut self) -> Result<()> {
        self.profile
            .0
            .active
            .lock()
            .map_err(|_| refusal(super::state::Refusal::Unavailable))?
            .as_mut()
            .ok_or_else(|| BitrouterError::Forbidden("routing reservation unavailable".into()))?
            .admission
            .sent()
            .map_err(refusal)?;
        self.committed = true;
        Ok(())
    }
}
impl Drop for SendGuard {
    fn drop(&mut self) {
        if !self.committed
            && let Ok(mut active) = self.profile.0.active.lock()
        {
            active.take();
        }
    }
}

#[async_trait]
impl PreRequestHook for Profile {
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision> {
        self.attached_execution(ctx)?;
        ctx.insert_extension(Arc::new(self.clone()));
        Ok(HookDecision::Allow)
    }
    async fn revalidate_context(&self, ctx: &PipelineContext) -> Result<HookDecision> {
        self.attached_execution(ctx)?;
        if !ctx
            .extension::<Self>()
            .is_some_and(|attached| Arc::ptr_eq(&attached.0, &self.0))
        {
            return Err(BitrouterError::Forbidden(
                "tenant routing context mismatch".into(),
            ));
        }
        Ok(HookDecision::Allow)
    }
}

#[async_trait]
impl RouteHook for Profile {
    async fn resolve(
        &self,
        chain: &mut Vec<RoutingTarget>,
        ctx: &mut PipelineContext,
    ) -> Result<()> {
        let execution = self.execution()?;
        let candidates = chain
            .iter()
            .map(|target| {
                let key = self.key(target)?;
                let metadata = execution
                    .route()
                    .routing_metadata(target)
                    .map_err(|code| execution.reject(code))?;
                Ok((key, metadata.relative_weight))
            })
            .collect::<Result<Vec<_>>>()?;
        let sort = execution
            .route()
            .routing_sort(ctx.model())
            .map_err(|code| execution.reject(code))?;
        let indices = self
            .0
            .runtime
            .order(&candidates, order(sort))
            .map_err(refusal)?;
        let mut retained: Vec<_> = std::mem::take(chain).into_iter().map(Some).collect();
        for index in indices {
            let target = retained
                .get_mut(index)
                .and_then(Option::take)
                .ok_or_else(|| BitrouterError::internal("invalid tenant routing permutation"))?;
            chain.push(target);
        }
        Ok(())
    }
    async fn after_resolve(
        &self,
        chain: &[RoutingTarget],
        ctx: &mut PipelineContext,
    ) -> Result<()> {
        // Recheck the final immutable chain after later route hooks, without
        // reserving send capacity or broadening retained membership.
        RouteHook::revalidate_context(self, chain, ctx).await
    }
    async fn revalidate_context(
        &self,
        chain: &[RoutingTarget],
        _ctx: &PipelineContext,
    ) -> Result<()> {
        for target in chain {
            self.key(target)?;
        }
        Ok(())
    }
}

#[async_trait]
impl ObserveHook for Profile {
    async fn after_phase(&self, _phase: Phase, _ctx: &PipelineContext) {}
    fn stream_interest(&self) -> StreamInterest {
        StreamInterest::none().with_usage().with_finish()
    }
    async fn on_stream_part(&self, _ctx: &StreamContext, part: &StreamPart) {
        if let StreamPart::Usage { usage }
        | StreamPart::ResponseCompleted {
            usage: Some(usage), ..
        } = part
            && let Ok(mut active) = self.0.active.lock()
            && let Some(active) = active.as_mut()
        {
            active.output_tokens = observed_output(usage);
        }
    }
    async fn on_hop_end(
        &self,
        _ctx: &PipelineContext,
        target: &RoutingTarget,
        outcome: HopOutcome<'_>,
    ) {
        match outcome {
            HopOutcome::Generated(result) => {
                let outcome = if result.result.generation().is_some_and(|result| {
                    matches!(
                        result.finish_reason,
                        Some(bitrouter_ai::types::FinishReason::Error(_))
                    )
                }) {
                    Outcome::Neutral
                } else {
                    Outcome::Success {
                        output_tokens: result.result.usage().and_then(observed_output),
                    }
                };
                self.finish(target, outcome);
            }
            HopOutcome::Failed(_) => self.finish(target, Outcome::Unknown),
            HopOutcome::StreamStarted => {}
        }
    }
    async fn on_request_end(&self, ctx: &PipelineContext, outcome: &RequestOutcome) {
        let Some(target) = ctx.serving_target() else {
            return;
        };
        match outcome {
            RequestOutcome::Completed if ctx.stream_terminal_succeeded() => {
                let output_tokens = self
                    .0
                    .active
                    .lock()
                    .ok()
                    .and_then(|active| active.as_ref().and_then(|active| active.output_tokens));
                self.finish(&target, Outcome::Success { output_tokens });
            }
            RequestOutcome::Completed => self.finish(&target, Outcome::Unknown),
            RequestOutcome::Failed(_) => self.finish(&target, Outcome::Unknown),
            RequestOutcome::ClientDisconnected => self.finish(&target, Outcome::Unknown),
        }
    }
}

fn observed_output(usage: &bitrouter_ai::types::Usage) -> Option<u64> {
    // Legacy canonical usage cannot distinguish an absent output count from
    // explicit zero. Only positive provider counts are usable throughput
    // observations here; neither estimated nor absent quantities become zero.
    (usage.origin == UsageOrigin::ProviderReported && usage.completion_tokens > 0)
        .then_some(usage.completion_tokens)
}

fn fallback_allowed(error: &BitrouterError, receipt_blocks: bool) -> bool {
    if receipt_blocks {
        return false;
    }
    matches!(
        error,
        BitrouterError::Upstream {
            status: 500..=599 | 408 | 429,
            ..
        } | BitrouterError::UpstreamRateLimited { .. }
            | BitrouterError::UpstreamUnavailable
            | BitrouterError::UpstreamPaymentRequired { .. }
    )
}

#[async_trait]
impl ExecutionHook for Profile {
    async fn on_success(&self, _ctx: &PipelineContext, _result: &ExecutionResult) -> Result<()> {
        Ok(())
    }
    async fn on_failure(&self, _ctx: &PipelineContext, error: &BitrouterError) -> FallbackDecision {
        // The pipeline itself checks this fence first. Repeat it here so this
        // profile never becomes a weaker fallback authority when composed.
        let blocked = self
            .execution()
            .map_or(true, |execution| execution.receipt_blocks_fallback());
        if fallback_allowed(error, blocked) {
            FallbackDecision::TryNext
        } else {
            FallbackDecision::Fail(error.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_execution_and_receipt_fences_never_allow_fallback() {
        for error in [
            BitrouterError::UpstreamTimeout,
            BitrouterError::UpstreamInvalidResponse {
                message: String::new(),
                usage: None,
            },
            BitrouterError::Cancelled,
            BitrouterError::Internal(String::new()),
            BitrouterError::UpstreamBadRequest {
                error: serde_json::json!({}),
            },
        ] {
            assert!(!fallback_allowed(&error, false));
        }
        for error in [
            BitrouterError::UpstreamUnavailable,
            BitrouterError::UpstreamRateLimited {
                retry_after: None,
                detail: None,
            },
            BitrouterError::Upstream {
                status: 503,
                message: String::new(),
            },
        ] {
            assert!(fallback_allowed(&error, false));
            assert!(!fallback_allowed(&error, true));
        }
    }
}
