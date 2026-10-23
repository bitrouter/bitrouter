//! Authenticated canonical admission and actual model execution bookkeeping.

use std::sync::Arc;

use crate::config::routing_table::ConfigRoutingTable;
use crate::gateway::error::BitrouterError;
use crate::gateway::pipeline::context::PipelineContext;
use crate::gateway::pipeline::hooks::HookDecision;
use crate::gateway::pipeline::hooks::PreRequestHook;
use crate::gateway::pipeline::hooks::RouteHook;
use crate::gateway::pipeline::types::RoutingTarget;
use anyhow::{Result, ensure};
use async_trait::async_trait;
use sea_orm::DatabaseConnection;

use super::catalog::{block_digest, request_compatible};
use super::store::{BlockPolicyStore, RoutingDependencies};
use crate::gateway::observability::binding::CanonicalSelection;
use crate::gateway::observability::requests::GatewayRequestObserved;
use crate::policy_lock::PolicyRuntime;
use crate::session_identity::RequestSessionContext;
use crate::storage::session_identity::SessionIdentity;

use crate::storage::execution::DecisionContext;

struct AdmissionFence {
    identity: SessionIdentity,
    control: Option<(u64, u64)>,
}

#[derive(Clone)]
pub struct BlockPolicyRouter {
    db: DatabaseConnection,
    routing: Arc<ConfigRoutingTable>,
    policies: Arc<PolicyRuntime>,
}

impl BlockPolicyRouter {
    pub fn new(
        db: DatabaseConnection,
        routing: Arc<ConfigRoutingTable>,
        policies: Arc<PolicyRuntime>,
    ) -> Self {
        Self {
            db,
            routing,
            policies,
        }
    }

    fn service(&self, owner: &str) -> Result<BlockPolicyStore> {
        BlockPolicyStore::new(self.db.clone(), owner)
    }
    async fn admit(&self, ctx: &mut PipelineContext) -> Result<()> {
        let Some(session) = ctx.extension::<RequestSessionContext>() else {
            return Ok(());
        };
        let Some(observed) = ctx.get_event::<GatewayRequestObserved>() else {
            return Ok(());
        };
        let observed = observed.request.clone();
        let Some(binding) = &observed.binding else {
            return Ok(());
        };
        let identity = binding.identity.clone();
        let service = self.service(&identity.owner)?;
        let state = service.state().await?;
        let (routing_generation, config) = self.routing.versioned_snapshot();
        let policies = self.policies.routing_snapshot();
        let mut dependencies = RoutingDependencies {
            trial_ready: observed.coverage_ready,
            expected_control_generation: Some(state.generation),
            ..RoutingDependencies::default()
        };
        for block in state
            .blocks
            .values()
            .chain(state.archived_experiments.values().map(|a| &a.block))
        {
            if block.definition.source == identity.source
                && let Ok(dependency) = block_digest(&config, &policies, &block.definition).await
            {
                let id = &block.definition.block_id;
                if state
                    .blocks
                    .get(id)
                    .is_some_and(|current| current.experiment_id == block.experiment_id)
                {
                    dependencies.current.insert(id.clone(), dependency);
                } else {
                    dependencies
                        .archived
                        .insert(block.experiment_id.clone(), dependency);
                }
            }
        }
        let bypass = if session.api_continuation_id.is_some() {
            Some("provider_continuation_precedence")
        } else if session
            .route_lease
            .as_ref()
            .is_some_and(|lease| lease.applied)
        {
            Some("session_route_override_precedence")
        } else {
            None
        };
        let intent = service
            .select_with_bypass(
                &identity,
                ctx.request_id(),
                DecisionContext {
                    selector: ctx.model().to_owned(),
                    fingerprint: crate::policy_table_router::PolicyTable::fingerprint(
                        ctx.require_generation_prompt()?,
                    ),
                },
                &dependencies,
                bypass,
            )
            .await?;
        let mut dispatch_route = intent
            .as_ref()
            .map(|intent| intent.selected_route.clone())
            .unwrap_or_else(|| ctx.model().to_owned());
        let mut route_guard_reason =
            (!observed.coverage_ready).then(|| "gateway_inventory_unavailable".to_owned());
        if dispatch_route != ctx.model()
            && !request_compatible(&config, &policies, &dispatch_route, ctx).await?
        {
            dispatch_route = ctx.model().to_owned();
            route_guard_reason = Some("candidate_request_capability_unverified".into());
        }
        ctx.insert_extension(Arc::new(AdmissionFence {
            identity,
            control: intent
                .as_ref()
                .map(|i| (i.control_generation, i.mode_epoch)),
        }));
        ctx.insert_extension(Arc::new(CanonicalSelection {
            routing_generation,
            policies,
            intent,
            dispatch_route,
            route_guard_reason,
        }));
        Ok(())
    }
}

fn internal(error: anyhow::Error) -> BitrouterError {
    tracing::warn!(%error, "canonical evolution operation failed");
    BitrouterError::internal("canonical evolution state could not be verified")
}

#[async_trait]
impl PreRequestHook for BlockPolicyRouter {
    async fn revalidate_context(
        &self,
        ctx: &PipelineContext,
    ) -> crate::gateway::error::Result<HookDecision> {
        // Reuse the original admission fence; never register or select again.
        self.revalidate_admission(ctx).await?;
        Ok(HookDecision::Allow)
    }

    async fn check(
        &self,
        ctx: &mut PipelineContext,
    ) -> crate::gateway::error::Result<HookDecision> {
        self.admit(ctx).await.map_err(internal)?;
        Ok(HookDecision::Allow)
    }
}

impl BlockPolicyRouter {
    async fn revalidate_admission(
        &self,
        ctx: &PipelineContext,
    ) -> crate::gateway::error::Result<()> {
        let Some(fence) = ctx.extension::<AdmissionFence>() else {
            return Ok(());
        };
        let check = async {
            if let Some(expected) = fence.control {
                let state = self.service(&fence.identity.owner)?.state().await?;
                ensure!(
                    (state.generation, state.mode_epoch) == expected,
                    "evolution mode or policy changed before dispatch"
                );
            }
            Ok(())
        }
        .await;
        check.map_err(internal)
    }
}

#[async_trait]
impl RouteHook for BlockPolicyRouter {
    async fn resolve(
        &self,
        _chain: &mut Vec<RoutingTarget>,
        ctx: &mut PipelineContext,
    ) -> crate::gateway::error::Result<()> {
        self.revalidate_admission(ctx).await
    }

    async fn revalidate_context(
        &self,
        _chain: &[RoutingTarget],
        ctx: &PipelineContext,
    ) -> crate::gateway::error::Result<()> {
        self.revalidate_admission(ctx).await
    }
}
