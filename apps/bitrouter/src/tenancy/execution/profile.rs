//! Explicit hosted profile of the existing pipeline. No host hook, mutable
//! routing table, ambient credential plugin or server-tool runner is inherited.

use std::sync::{Arc, atomic::Ordering};

use async_trait::async_trait;
use bitrouter_ai::types::{OperationScope, StreamPart};

use super::TenantExecution;
use crate::gateway::{
    error::Result,
    pipeline::{
        builder::PipelineBuilder,
        context::{PipelineContext, StreamContext},
        execution::Pipeline,
        hooks::{HookDecision, ObserveHook, Phase, PreRequestHook, RequestOutcome},
    },
};

struct Entry(Arc<TenantExecution>);

struct PolicyCompletion {
    pending: crate::gateway::observability::decision::PendingEvalDecisionStore,
    #[cfg(feature = "core")]
    observation: Option<Arc<crate::tenancy::eval::observation::RequiredObservation>>,
}

#[async_trait]
impl ObserveHook for PolicyCompletion {
    async fn after_phase(&self, phase: Phase, _ctx: &PipelineContext) {
        #[cfg(feature = "core")]
        if matches!(phase, Phase::Route)
            && let Some(observation) = &self.observation
        {
            observation.route_completed();
        }
        #[cfg(not(feature = "core"))]
        let _ = phase;
    }
    async fn on_stream_part(&self, _ctx: &StreamContext, _part: &StreamPart) {}
    async fn on_request_end(&self, ctx: &PipelineContext, outcome: &RequestOutcome) {
        #[cfg(feature = "core")]
        if let Some(observation) = &self.observation {
            observation.before_decision_end(ctx, outcome);
        }
        #[cfg(not(feature = "core"))]
        let _ = outcome;
        if let Some(invocation) =
            ctx.extension::<crate::gateway::observability::decision::EvalInvocation>()
        {
            self.pending.remove(&invocation, ctx.caller().user_id());
        }
    }
}

#[async_trait]
impl PreRequestHook for Entry {
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision> {
        self.0.revalidate().map_err(|code| self.0.reject(code))?;
        let attached = ctx.extension::<TenantExecution>();
        if !attached
            .as_ref()
            .is_some_and(|attached| Arc::ptr_eq(attached, &self.0))
            || ctx.model() != self.0.route.reference().resource_id.as_str()
            || self
                .0
                .entered
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Err(self
                .0
                .reject(bitrouter_ai::tenancy::operation::ErrorCode::Forbidden));
        }
        ctx.set_caller(self.0.caller());
        #[cfg(feature = "core")]
        if let Some(observation) = self.0.observation.get() {
            observation.entered()?;
        }
        Ok(HookDecision::Allow)
    }

    async fn revalidate_context(&self, _ctx: &PipelineContext) -> Result<HookDecision> {
        self.0.revalidate().map_err(|code| self.0.reject(code))?;
        Ok(HookDecision::Allow)
    }
}

impl TenantExecution {
    pub(crate) fn attach(
        self: &Arc<Self>,
        pipeline: Arc<Pipeline>,
        request: &mut crate::gateway::pipeline::types::PipelineRequest,
    ) -> Result<()> {
        self.route.apply_generation_defaults(&mut request.input);
        #[cfg(feature = "core")]
        let registration = match self.observation.get() {
            Some(observation) => self.service.pipelines.register_observed(
                self.id(),
                pipeline,
                observation.clone(),
            )?,
            None => self.service.pipelines.register(self.id(), pipeline)?,
        };
        #[cfg(not(feature = "core"))]
        let registration = self.service.pipelines.register(self.id(), pipeline)?;
        registration.attach(request)?;
        request.extensions.insert(self.clone());
        Ok(())
    }

    pub(crate) fn pipeline(self: &Arc<Self>) -> Result<Arc<Pipeline>> {
        let mut builder = PipelineBuilder::new();
        builder
            .owned_drain()
            .served_operations(OperationScope::Both)
            .routing_table(self.route.clone())
            .executor(self.service.executor.clone())
            .pre_resolution_hook_for(Entry(self.clone()), OperationScope::Both);
        if let Some(policy) = self.route.policy() {
            if !self.route.dependencies().contains(policy.reference()) || policy.name().is_empty() {
                return Err(self.reject(bitrouter_ai::tenancy::operation::ErrorCode::Forbidden));
            }
            // Signed experiments still need qualified tenant exploration admission
            // and budget accounting. Fence only the selected named policy, in every
            // learning mode, without rewriting its retained artifact or digest.
            if policy
                .document()
                .policies
                .get(policy.name())
                .and_then(|definition| definition.optimization.as_ref())
                .and_then(|optimization| optimization.active.as_ref())
                .is_some()
            {
                return Err(
                    self.reject(bitrouter_ai::tenancy::operation::ErrorCode::CapabilityMismatch)
                );
            }
            let config = policy.configuration().map_err(|code| self.reject(code))?;
            let pending =
                crate::gateway::observability::decision::PendingEvalDecisionStore::default();
            let runtime = crate::policy_lock::PolicyRuntime::from_document(
                &config,
                Some(policy.document().clone()),
                pending.clone(),
                None,
            )
            .map_err(|_| {
                self.reject(bitrouter_ai::tenancy::operation::ErrorCode::CapabilityMismatch)
            })?;
            builder
                .model_selector(runtime)
                .route_hook(crate::policy_lock::PredictiveSingleTargetRouteHook);
            #[cfg(feature = "core")]
            if self.learning.mode != bitrouter_ai::tenancy::configuration::LearningMode::Off {
                let observation = crate::tenancy::eval::observation::RequiredObservation::new(
                    self.service.eval.clone(),
                    Arc::downgrade(self),
                );
                self.observation.set(observation.clone()).map_err(|_| {
                    self.reject(bitrouter_ai::tenancy::operation::ErrorCode::Forbidden)
                })?;
                builder
                    .observe_hook(
                        crate::gateway::observability::prediction::PredictiveResponseObserver::new(
                            pending.clone(),
                        ),
                    )
                    .settlement_recorder_for(
                        crate::core::eval::settlement::EvalSettlementRecorder::for_execution(
                            self,
                            pending.clone(),
                            observation,
                        ),
                        OperationScope::Both,
                    );
            }
            builder.observe_hook(PolicyCompletion {
                pending,
                #[cfg(feature = "core")]
                observation: self.observation.get().cloned(),
            });
        }
        Ok(Arc::new(builder.build()?))
    }
}
