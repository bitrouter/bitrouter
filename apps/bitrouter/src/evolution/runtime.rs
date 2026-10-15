//! Authenticated canonical admission and actual model execution bookkeeping.

use std::sync::{Arc, Mutex};

use crate::config::routing_table::ConfigRoutingTable;
use crate::gateway::error::BitrouterError;
use crate::gateway::pipeline::context::PipelineContext;
use crate::gateway::pipeline::hooks::HookDecision;
use crate::gateway::pipeline::hooks::PreRequestHook;
use crate::gateway::pipeline::hooks::RouteHook;
use crate::gateway::pipeline::types::RoutingTarget;
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use bitrouter_ai::event::PipelineEvent;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde::Serialize;

use super::catalog::{block_digest, request_compatible};
use super::control::{BlockDefinition, ControlState};
use super::service::{EvolutionService, RoutingDependencies};
use crate::gateway::observability::requests::GatewayRequestObserved;
use crate::policy_lock::{PolicyRoutingSnapshot, PolicyRuntime};
use crate::session_identity::RequestSessionContext;
use crate::storage::capture::sessions;
use crate::storage::evolution::EvolutionStore;
use crate::storage::session_identity::SessionIdentity;

use crate::gateway::metering::replay::CanonicalRequestReplayRejected;
use crate::storage::execution::{DecisionContext, EXECUTION_KIND, ExecutionRecord};

pub mod candidates;

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
    routing_generation: u64,
    policies: PolicyRoutingSnapshot,
    identity: SessionIdentity,
    control: Option<(u64, u64)>,
}

#[derive(Clone)]
pub struct EvolutionRuntime {
    pub(crate) db: DatabaseConnection,
    routing: Arc<ConfigRoutingTable>,
    policies: Arc<PolicyRuntime>,
    pub(crate) worker_status: Arc<Mutex<super::scheduler::WorkerStatus>>,
}

impl EvolutionRuntime {
    pub fn new(
        db: DatabaseConnection,
        routing: Arc<ConfigRoutingTable>,
        policies: Arc<PolicyRuntime>,
    ) -> Self {
        Self {
            worker_status: Arc::new(Mutex::new(Default::default())),
            db,
            routing,
            policies,
        }
    }

    pub fn worker_status(&self) -> super::scheduler::WorkerStatus {
        match self.worker_status.lock() {
            Ok(status) => status.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Background publication validates the same live route dependencies as admission.
    pub async fn reconcile(
        &self,
        owner: &str,
        block_id: &str,
    ) -> Result<super::learning::LearningReport> {
        self.reconcile_experiment(owner, block_id, None).await
    }

    pub async fn reconcile_experiment(
        &self,
        owner: &str,
        block_id: &str,
        experiment_id: Option<&str>,
    ) -> Result<super::learning::LearningReport> {
        let service = self.service(owner)?;
        let state = service.state().await?;
        let block = state
            .experiment(block_id, experiment_id)
            .context("unknown experiment")?;
        let (generation, config) = self.routing.versioned_snapshot();
        let policies = self.policies.routing_snapshot();
        let dependency = block_digest(&config, &policies, &block.definition).await?;
        ensure!(
            dependency == block.routing_config_digest,
            "routing dependency changed; register a new experiment revision"
        );
        service
            .reconcile_checked(block_id, Some(&block.experiment_id), || {
                ensure!(
                    self.routing.generation() == generation
                        && self.policies.matches_routing_snapshot(&policies),
                    "routing reloaded before evolution publication; retry"
                );
                Ok(())
            })
            .await
    }

    pub fn service(&self, owner: &str) -> Result<EvolutionService> {
        EvolutionService::new(self.db.clone(), owner)
    }

    /// Operator-owned definitions are validated against the live route catalog.
    /// This registers an experiment but never enables the owner's mode.
    pub async fn register(&self, owner: &str, definition: BlockDefinition) -> Result<ControlState> {
        let (generation, config) = self.routing.versioned_snapshot();
        let policies = self.policies.routing_snapshot();
        let dependency = block_digest(&config, &policies, &definition).await?;
        ensure!(
            generation == self.routing.generation()
                && self.policies.matches_routing_snapshot(&policies),
            "routing changed during block validation; retry registration"
        );
        self.service(owner)?
            .register_checked(definition, dependency, None, || {
                ensure!(
                    generation == self.routing.generation()
                        && self.policies.matches_routing_snapshot(&policies),
                    "routing changed before block registration; retry"
                );
                Ok(())
            })
            .await
    }

    /// Start a new experiment while retaining old assignments and feedback.
    /// A changed live contract rebases to the configured selector; callers must
    /// explicitly supply those baseline routes in the reviewed definition.
    pub async fn revise(
        &self,
        owner: &str,
        definition: BlockDefinition,
        predecessor: String,
    ) -> Result<ControlState> {
        let service = self.service(owner)?;
        let state = service.state().await?;
        let previous = state
            .experiment(&definition.block_id, Some(&predecessor))
            .context("unknown predecessor experiment")?;
        let (generation, config) = self.routing.versioned_snapshot();
        let policies = self.policies.routing_snapshot();
        let dependency = block_digest(&config, &policies, &definition).await?;
        let previous_dependency = block_digest(&config, &policies, &previous.definition).await;
        let reset_to_configured = previous_dependency.as_ref().ok()
            != Some(&previous.routing_config_digest)
            || !state.external_dependencies_match(previous);
        service
            .revise_checked(
                super::service::revisions::RevisionRegistration {
                    definition,
                    routing_digest: dependency,
                    predecessor,
                    reset_to_configured,
                    expected_control: (state.generation, state.mode_epoch),
                },
                || {
                    ensure!(
                        self.routing.generation() == generation
                            && self.policies.matches_routing_snapshot(&policies),
                        "routing changed before experiment revision; retry"
                    );
                    Ok(())
                },
            )
            .await
    }

    pub async fn executions(&self, identity: &SessionIdentity) -> Result<Vec<ExecutionRecord>> {
        let store = EvolutionStore::new(self.db.clone(), &identity.owner)?;
        let key = identity.key()?;
        let mut rows: Vec<ExecutionRecord> = crate::storage::evolution::records::Entity::find()
            .filter(crate::storage::evolution::records::Column::ScopeId.eq(&store.scope_id))
            .filter(crate::storage::evolution::records::Column::Kind.eq(EXECUTION_KIND))
            .filter(crate::storage::evolution::records::Column::SessionKey.eq(key))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|row| serde_json::from_str(&row.body))
            .collect::<std::result::Result<_, _>>()?;
        rows.sort_by(|a, b| (&a.started_at, &a.request_id).cmp(&(&b.started_at, &b.request_id)));
        Ok(rows)
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
        let request = ExecutionRecord {
            request_id: ctx.request_id().into(),
            attempt_id: observed.attempt_id,
            identity: identity.clone(),
            connection_id: binding.connection_id.clone(),
            captured_watermark: binding.watermark,
            started_at: observed.admitted_at,
            intent: intent.clone(),
            dispatch_route: dispatch_route.clone(),
            route_guard_reason,
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
                    == intent.as_ref().map(|i| &i.context),
            "request ID was reused outside its canonical execution"
        );
        // A replay may inspect its existing intent, but must not perform a
        // second upstream execution under one cost/accounting identity.
        ensure!(
            persisted.attempt_id == request.attempt_id,
            "canonical request ID has already been dispatched or interrupted"
        );
        ctx.insert_extension(Arc::new(AdmissionFence {
            routing_generation,
            policies,
            identity,
            control: intent
                .as_ref()
                .map(|i| (i.control_generation, i.mode_epoch)),
        }));
        ctx.emit(CanonicalRequestBound { request });
        ctx.set_model(dispatch_route);
        Ok(())
    }
}

fn internal(error: anyhow::Error) -> BitrouterError {
    tracing::warn!(%error, "canonical evolution operation failed");
    BitrouterError::internal("canonical evolution state could not be verified")
}

#[async_trait]
impl PreRequestHook for EvolutionRuntime {
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

impl EvolutionRuntime {
    async fn revalidate_admission(
        &self,
        ctx: &PipelineContext,
    ) -> crate::gateway::error::Result<()> {
        let Some(fence) = ctx.extension::<AdmissionFence>() else {
            return Ok(());
        };
        let check = async {
            ensure!(
                self.routing.generation() == fence.routing_generation
                    && self.policies.matches_routing_snapshot(&fence.policies),
                "routing reloaded after canonical admission"
            );
            let session = sessions::Entity::find_by_id(fence.identity.key()?)
                .one(&self.db)
                .await?
                .context("canonical session disappeared")?;
            ensure!(
                !session.deleted,
                "canonical session was deleted before dispatch"
            );
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
impl RouteHook for EvolutionRuntime {
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

#[cfg(test)]
mod tests;
