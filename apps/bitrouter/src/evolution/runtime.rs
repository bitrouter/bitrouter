//! Authenticated canonical admission and actual model execution bookkeeping.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use bitrouter_sdk::config::ConfigRoutingTable;
use bitrouter_sdk::language_model::hooks::{HopOutcome, ObserveHook, Phase, RequestOutcome};
use bitrouter_sdk::language_model::{
    HookDecision, PipelineContext, PreRequestHook, RouteHook, RoutingTarget, SettlementContext,
    SettlementRecorder,
};
use bitrouter_sdk::language_model::{StreamContext, StreamPart};
use bitrouter_sdk::{BitrouterError, PipelineEvent};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};

use super::catalog::{block_digest, request_compatible};
use super::control::{BlockDefinition, ControlState};
use super::inventory::{BeginResult, GatewayInventory, GatewayRequest, InventoryBinding};
use super::service::{DecisionContext, EvolutionService, RouteIntent, RoutingDependencies};
use super::store::EvolutionStore;
use crate::acp_trajectory::{
    SessionIdentity,
    entities::{connections, events, sessions},
};
use crate::auth::events::ApiPrincipalEstablished;
use crate::metering::recorder::MeteringSettlementEvent;
use crate::policy_lock::{PolicyRoutingSnapshot, PolicyRuntime};
use crate::session_identity::RequestSessionContext;

pub(super) const EXECUTION_KIND: &str = "execution";

pub mod candidates;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionHop {
    pub provider: String,
    pub model: String,
    pub account: Option<String>,
    pub protocol: String,
    pub status: String,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionSettlement {
    pub settled_at: String,
    pub hops: Vec<ExecutionHop>,
    pub final_model: String,
    pub final_provider: String,
    pub error_code: Option<String>,
    pub duration_ms: u64,
    /// Filled by the terminal observer, including disconnects without an error.
    pub outcome: Option<ExecutionOutcome>,
    /// Metering's final observation; multiple attempts may have additional cost.
    pub final_metered_cost_micro_usd: Option<u64>,
    /// Complete request cost only when every attempted generation is covered.
    pub total_cost_micro_usd: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutcome {
    Completed,
    Failed,
    ClientDisconnected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub request_id: String,
    pub attempt_id: String,
    pub identity: SessionIdentity,
    pub connection_id: String,
    pub captured_watermark: i64,
    pub started_at: String,
    pub intent: Option<RouteIntent>,
    /// Effective route after the arm's request-capability guard.
    pub dispatch_route: String,
    pub route_guard_reason: Option<String>,
    /// None is an unfinished or interrupted request, never a zero-cost success.
    pub settlement: Option<ExecutionSettlement>,
}

#[derive(Debug, Serialize)]
struct CanonicalRequestBound {
    request: ExecutionRecord,
}

#[derive(Debug, Serialize)]
struct GatewayRequestObserved {
    request: GatewayRequest,
    #[serde(skip)]
    hops: Arc<Mutex<Vec<ExecutionHop>>>,
}

impl PipelineEvent for GatewayRequestObserved {
    fn event_name(&self) -> &'static str {
        "evolution.gateway_request_observed"
    }
}

impl PipelineEvent for CanonicalRequestBound {
    fn event_name(&self) -> &'static str {
        "evolution.canonical_request_bound"
    }
}

/// A durable execution already owns this request identity. Always-run
/// bookkeeping must not overwrite that execution with the rejected replay.
#[derive(Debug, Serialize)]
pub(crate) struct CanonicalRequestReplayRejected;

impl PipelineEvent for CanonicalRequestReplayRejected {
    fn event_name(&self) -> &'static str {
        "evolution.request_replay_rejected"
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
    inventory: GatewayInventory,
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
            inventory: GatewayInventory::new(db.clone()),
            db,
            routing,
            policies,
        }
    }

    /// The control listener must share this exact runtime epoch with serving.
    pub fn inventory(&self) -> GatewayInventory {
        self.inventory.clone()
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
        let mut rows: Vec<ExecutionRecord> = super::store::records::Entity::find()
            .filter(super::store::records::Column::ScopeId.eq(&store.scope_id))
            .filter(super::store::records::Column::Kind.eq(EXECUTION_KIND))
            .filter(super::store::records::Column::SessionKey.eq(key))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|row| serde_json::from_str(&row.body))
            .collect::<std::result::Result<_, _>>()?;
        rows.sort_by(|a, b| (&a.started_at, &a.request_id).cmp(&(&b.started_at, &b.request_id)));
        Ok(rows)
    }

    async fn identify(
        &self,
        ctx: &PipelineContext,
        session: &RequestSessionContext,
    ) -> Result<Option<(SessionIdentity, String, i64)>> {
        let Some(controller) = &session.claimed_controller_instance_id else {
            return Ok(None);
        };
        let principal = if ctx.caller().is_local() {
            "local"
        } else {
            let Some(auth) = ctx.get_event::<ApiPrincipalEstablished>() else {
                return Ok(None);
            };
            auth.route_scope_id.as_str()
        };
        let connections = connections::Entity::find()
            .filter(connections::Column::ControllerInstanceId.eq(controller))
            .filter(connections::Column::RouteScopeId.eq(principal))
            .filter(connections::Column::State.eq("recording"))
            .all(&self.db)
            .await?;
        // An explicit ACP identity is authoritative within the authenticated
        // recorder scope. Otherwise prefer the exact recorded thread to a root.
        let claims = if let Some(acp) = &session.acp_session_id {
            vec![acp]
        } else {
            session
                .native
                .agent_thread_id
                .iter()
                .chain(session.native.root_session_id.iter())
                .collect()
        };
        for claim in claims {
            let mut matches = BTreeMap::new();
            for connection in &connections {
                let identity = SessionIdentity {
                    owner: connection.owner.clone(),
                    source: connection.source.clone(),
                    native_session_id: claim.clone(),
                };
                let key = identity.key()?;
                let Some(row) = sessions::Entity::find_by_id(&key).one(&self.db).await? else {
                    continue;
                };
                if row.deleted {
                    continue;
                }
                let observed = events::Entity::find()
                    .filter(events::Column::SessionKey.eq(&key))
                    .filter(events::Column::ConnectionId.eq(&connection.connection_id))
                    .one(&self.db)
                    .await?
                    .is_some();
                if observed {
                    matches.insert(key, (identity, connection.connection_id.clone(), row.head));
                }
            }
            if matches.len() > 1 {
                return Ok(None);
            }
            if let Some(binding) = matches.into_values().next() {
                return Ok(Some(binding));
            }
        }
        Ok(None)
    }

    async fn admit(&self, ctx: &mut PipelineContext) -> Result<()> {
        let Some(session) = ctx.extension::<RequestSessionContext>() else {
            return Ok(());
        };
        let Some(controller) = session.claimed_controller_instance_id.as_deref() else {
            return Ok(());
        };
        let principal = if ctx.caller().is_local() {
            "local".to_owned()
        } else if let Some(auth) = ctx.get_event::<ApiPrincipalEstablished>() {
            auth.route_scope_id.clone()
        } else {
            return Ok(());
        };
        let binding =
            self.identify(ctx, &session)
                .await?
                .map(|(identity, connection_id, watermark)| InventoryBinding {
                    identity,
                    connection_id,
                    watermark,
                });
        if let Some(binding) = &binding {
            let store = EvolutionStore::new(self.db.clone(), &binding.identity.owner)?;
            if store
                .get::<ExecutionRecord>(EXECUTION_KIND, ctx.request_id())
                .await?
                .is_some()
            {
                ctx.emit(CanonicalRequestReplayRejected);
                anyhow::bail!("canonical request ID already has an execution record");
            }
        }
        let observed = match self
            .inventory
            .begin(&principal, controller, ctx.request_id(), binding)
            .await?
        {
            BeginResult::Untracked => return Ok(()),
            BeginResult::Duplicate => {
                ctx.emit(CanonicalRequestReplayRejected);
                anyhow::bail!("gateway request ID already has an execution record");
            }
            BeginResult::Tracked(request) => *request,
        };
        // Insert before routing/evolution work: even a failed admission or an
        // unresolved session must remain visible to the accounting boundary.
        ctx.emit(GatewayRequestObserved {
            request: observed.clone(),
            hops: Arc::new(Mutex::new(Vec::new())),
        });
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
                    fingerprint: crate::policy_table_router::PolicyTable::fingerprint(ctx.prompt()),
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
    async fn check(&self, ctx: &mut PipelineContext) -> bitrouter_sdk::Result<HookDecision> {
        self.admit(ctx).await.map_err(internal)?;
        Ok(HookDecision::Allow)
    }
}

#[async_trait]
impl RouteHook for EvolutionRuntime {
    async fn resolve(
        &self,
        _chain: &mut Vec<RoutingTarget>,
        ctx: &mut PipelineContext,
    ) -> bitrouter_sdk::Result<()> {
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
impl ObserveHook for EvolutionRuntime {
    async fn after_phase(&self, _phase: Phase, _ctx: &PipelineContext) {}
    async fn on_stream_part(&self, _ctx: &StreamContext, _part: &StreamPart) {}
    async fn on_request_end(&self, ctx: &PipelineContext, outcome: &RequestOutcome) {
        let Some(bound) = ctx.get_event::<GatewayRequestObserved>() else {
            return;
        };
        let outcome = match outcome {
            RequestOutcome::Completed => ExecutionOutcome::Completed,
            RequestOutcome::Failed(_) => ExecutionOutcome::Failed,
            RequestOutcome::ClientDisconnected => ExecutionOutcome::ClientDisconnected,
        };
        let save = self.inventory.terminal(&bound.request, outcome).await;
        if let Err(error) = save {
            tracing::warn!(%error, "canonical execution terminal was not persisted");
        }
    }

    async fn on_hop_start(&self, ctx: &PipelineContext, target: &RoutingTarget) {
        if let Some(bound) = ctx.get_event::<GatewayRequestObserved>() {
            let mut hops = match bound.hops.lock() {
                Ok(hops) => hops,
                Err(poisoned) => poisoned.into_inner(),
            };
            hops.push(ExecutionHop {
                provider: target.provider_name.clone(),
                model: target.service_id.clone(),
                account: target.account_label.clone(),
                protocol: target.api_protocol.as_str().into(),
                status: "attempting".into(),
                error_code: None,
            });
        }
    }

    async fn on_hop_end(
        &self,
        ctx: &PipelineContext,
        _target: &RoutingTarget,
        outcome: HopOutcome<'_>,
    ) {
        if let Some(bound) = ctx.get_event::<GatewayRequestObserved>() {
            let mut hops = match bound.hops.lock() {
                Ok(hops) => hops,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let Some(hop) = hops.last_mut() {
                let (status, error) = match outcome {
                    HopOutcome::Generated(_) => ("completed", None),
                    HopOutcome::StreamStarted => ("stream_started", None),
                    HopOutcome::Failed(error) => ("failed", Some(error.error_code().into())),
                };
                hop.status = status.into();
                hop.error_code = error;
            }
        }
    }
}

#[async_trait]
impl SettlementRecorder for EvolutionRuntime {
    async fn record(&self, ctx: &mut SettlementContext) -> bitrouter_sdk::Result<()> {
        let Some(bound) = ctx.get_event::<GatewayRequestObserved>() else {
            return Ok(());
        };
        let mut hops = match bound.hops.lock() {
            Ok(hops) => hops.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let metered = ctx
            .get_event::<MeteringSettlementEvent>()
            .filter(|m| m.request_id == ctx.request_id);
        let final_cost = metered.and_then(|m| m.cost_micro_usd);
        if let Some(hop) = hops.last_mut().filter(|hop| hop.status == "stream_started") {
            hop.status = if ctx.error.is_some() {
                "failed"
            } else if ctx.finish_reason.is_some() {
                "completed"
            } else {
                "incomplete"
            }
            .into();
            hop.error_code = ctx.error.as_ref().map(|error| error.error_code().into());
        }
        let total_cost = if hops.is_empty() && ctx.error.is_some() {
            Some(0)
        } else if hops.len() == 1
            && ctx.usage_origin == bitrouter_sdk::language_model::UsageOrigin::ProviderReported
        {
            final_cost
        } else {
            None
        };
        let settlement = ExecutionSettlement {
            settled_at: chrono::Utc::now().to_rfc3339(),
            hops,
            final_model: ctx.model_id.clone(),
            final_provider: ctx.provider_id.clone(),
            error_code: ctx.error.as_ref().map(|error| error.error_code().into()),
            duration_ms: ctx.request_duration_ms,
            outcome: None,
            final_metered_cost_micro_usd: final_cost,
            total_cost_micro_usd: total_cost,
        };
        let save = self.inventory.settle(&bound.request, settlement).await;
        save.map_err(internal)
    }
}

#[cfg(test)]
mod tests;
