//! Canonical execution identity and live gateway dispatch fences.

use crate::gateway::observability::event::PipelineEvent;
use anyhow::{Context, ensure};
use async_trait::async_trait;
use sea_orm::DatabaseConnection;
use std::sync::{Arc, OnceLock};

use super::inventory::GatewayInventory;
use super::requests::{GatewayRequestObserved, internal};
use crate::config::routing_table::ConfigRoutingTable;
use crate::gateway::pipeline::context::PipelineContext;
use crate::gateway::pipeline::hooks::{HookDecision, PreRequestHook, RouteHook};
use crate::gateway::pipeline::types::RoutingTarget;
use crate::policy_source::{PolicyRoutingSnapshot, PolicyRuntime};

struct AdmissionFence {
    routing_generation: u64,
    policies: PolicyRoutingSnapshot,
}

/// Runs after optional learning selection and before defaults or dispatch.
#[derive(Clone)]
pub(crate) struct CanonicalRequestBinding {
    inventory: GatewayInventory,
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
            inventory: GatewayInventory::new(db),
            routing,
            policies,
        }
    }

    /// Attach the shared tenant budget before runtime hook clones are published.
    pub(crate) fn with_storage_budget(
        mut self,
        budget: Arc<OnceLock<crate::tenancy::storage::Budget>>,
    ) -> Self {
        self.inventory = self.inventory.with_storage_budget(budget);
        self
    }

    fn bind(&self, ctx: &mut PipelineContext) {
        if ctx.get_event::<GatewayRequestObserved>().is_some() {
            ctx.insert_extension(Arc::new(AdmissionFence {
                routing_generation: self.routing.generation(),
                policies: self.policies.routing_snapshot(),
            }));
        }
    }

    async fn revalidate(&self, ctx: &PipelineContext) -> crate::gateway::error::Result<()> {
        let Some(fence) = ctx.extension::<AdmissionFence>() else {
            return Ok(());
        };
        let check = async {
            ensure!(
                self.routing.generation() == fence.routing_generation
                    && self.policies.matches_routing_snapshot(&fence.policies),
                "routing reloaded after gateway admission"
            );
            let observed = &ctx
                .get_event::<GatewayRequestObserved>()
                .context("gateway observation missing")?
                .request;
            let current = self
                .inventory
                .request(&observed.route_scope_id, &observed.request_id)
                .await?
                .context("gateway reservation disappeared before dispatch")?;
            ensure!(
                current.attempt_id == observed.attempt_id && current.settlement.is_none(),
                "gateway reservation changed before dispatch"
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
        self.bind(ctx);
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
