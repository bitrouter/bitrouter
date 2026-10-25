//! Canonical execution identity and live gateway dispatch fences.

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use bitrouter_ai::event::PipelineEvent;
use sea_orm::{DatabaseConnection, EntityTrait, QueryFilter};
use serde::Serialize;
use std::sync::Arc;

use super::requests::{GatewayRequestObserved, internal};
use crate::config::routing_table::ConfigRoutingTable;
use crate::gateway::metering::replay::CanonicalRequestReplayRejected;
use crate::gateway::pipeline::context::PipelineContext;
use crate::gateway::pipeline::hooks::{HookDecision, PreRequestHook, RouteHook};
use crate::gateway::pipeline::types::RoutingTarget;
use crate::policy_lock::{PolicyRoutingSnapshot, PolicyRuntime};
use crate::storage::capture::sessions;
use crate::storage::evolution::EvolutionStore;
use crate::storage::execution::{EXECUTION_KIND, ExecutionRecord, RouteIntent};
use crate::storage::session_identity::SessionIdentity;

/// An optional learning selection retains the exact gateway inputs it used.
/// This is application-local context, not a public protocol or live core handle.
pub(crate) struct CanonicalSelection {
    pub(crate) routing_generation: u64,
    pub(crate) policies: PolicyRoutingSnapshot,
    pub(crate) intent: Option<RouteIntent>,
    pub(crate) dispatch_route: String,
    pub(crate) route_guard_reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct CanonicalRequestBound {
    request: ExecutionRecord,
}

impl PipelineEvent for CanonicalRequestBound {
    fn event_name(&self) -> &'static str {
        "evolution.canonical_request_bound"
    }
}

struct AdmissionFence {
    identity: SessionIdentity,
    selection: Arc<CanonicalSelection>,
}

/// Runs after optional learning selection and before defaults or dispatch.
#[derive(Clone)]
pub(crate) struct CanonicalRequestBinding {
    db: DatabaseConnection,
    routing: Arc<ConfigRoutingTable>,
    policies: Arc<PolicyRuntime>,
}

impl CanonicalRequestBinding {
    pub(crate) fn new(
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

    async fn bind(&self, ctx: &mut PipelineContext) -> Result<()> {
        let Some(observation) = ctx.get_event::<GatewayRequestObserved>() else {
            return Ok(());
        };
        let observed = &observation.request;
        let Some(binding) = &observed.binding else {
            return Ok(());
        };
        let identity = binding.identity.clone();
        let selection = ctx.extension::<CanonicalSelection>().unwrap_or_else(|| {
            Arc::new(CanonicalSelection {
                routing_generation: self.routing.generation(),
                policies: self.policies.routing_snapshot(),
                intent: None,
                dispatch_route: ctx.model().to_owned(),
                route_guard_reason: (!observed.coverage_ready)
                    .then(|| "gateway_inventory_unavailable".to_owned()),
            })
        });
        let request = ExecutionRecord {
            request_id: ctx.request_id().into(),
            attempt_id: observed.attempt_id.clone(),
            identity: identity.clone(),
            connection_id: binding.connection_id.clone(),
            captured_watermark: binding.watermark,
            started_at: observed.admitted_at.clone(),
            intent: selection.intent.clone(),
            dispatch_route: selection.dispatch_route.clone(),
            route_guard_reason: selection.route_guard_reason.clone(),
            settlement: None,
        };
        let store = EvolutionStore::new(self.db.clone(), &identity.owner)?;
        let (_, persisted): (_, ExecutionRecord) = store
            .initialize(
                EXECUTION_KIND,
                ctx.request_id(),
                Some(identity.key()?),
                &request,
            )
            .await?;
        if persisted.attempt_id != request.attempt_id {
            ctx.emit(CanonicalRequestReplayRejected);
        }
        ensure!(
            persisted.identity == identity
                && persisted.intent.as_ref().map(|i| &i.context)
                    == selection.intent.as_ref().map(|i| &i.context),
            "request ID was reused outside its canonical execution"
        );
        // A replay may inspect its existing intent, but must not perform a
        // second upstream execution under one cost/accounting identity.
        ensure!(
            persisted.attempt_id == request.attempt_id,
            "canonical request ID has already been dispatched or interrupted"
        );
        ctx.insert_extension(Arc::new(AdmissionFence {
            identity,
            selection: selection.clone(),
        }));
        ctx.emit(CanonicalRequestBound { request });
        ctx.set_model(selection.dispatch_route.clone());
        Ok(())
    }

    async fn revalidate(&self, ctx: &PipelineContext) -> crate::gateway::error::Result<()> {
        let Some(fence) = ctx.extension::<AdmissionFence>() else {
            return Ok(());
        };
        let check = async {
            ensure!(
                self.routing.generation() == fence.selection.routing_generation
                    && self
                        .policies
                        .matches_routing_snapshot(&fence.selection.policies),
                "routing reloaded after canonical admission"
            );
            let session = sessions::Entity::find_by_id(fence.identity.key()?)
                .filter(crate::storage::legacy::scope("acp_canonical_sessions"))
                .one(&self.db)
                .await?
                .context("canonical session disappeared")?;
            ensure!(
                !session.deleted,
                "canonical session was deleted before dispatch"
            );
            Ok(())
        }
        .await;
        check.map_err(internal)
    }
}

#[async_trait]
impl PreRequestHook for CanonicalRequestBinding {
    async fn check(
        &self,
        ctx: &mut PipelineContext,
    ) -> crate::gateway::error::Result<HookDecision> {
        self.bind(ctx).await.map_err(internal)?;
        Ok(HookDecision::Allow)
    }

    async fn revalidate_context(
        &self,
        ctx: &PipelineContext,
    ) -> crate::gateway::error::Result<HookDecision> {
        self.revalidate(ctx).await?;
        Ok(HookDecision::Allow)
    }
}

#[async_trait]
impl RouteHook for CanonicalRequestBinding {
    async fn resolve(
        &self,
        _chain: &mut Vec<RoutingTarget>,
        ctx: &mut PipelineContext,
    ) -> crate::gateway::error::Result<()> {
        self.revalidate(ctx).await
    }

    async fn revalidate_context(
        &self,
        _chain: &[RoutingTarget],
        ctx: &PipelineContext,
    ) -> crate::gateway::error::Result<()> {
        self.revalidate(ctx).await
    }
}
