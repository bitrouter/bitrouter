//! Gateway-owned canonical request admission, observation and settlement.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use bitrouter_ai::event::PipelineEvent;
use bitrouter_ai::types::StreamPart;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde::Serialize;

use super::inventory::{BeginResult, GatewayInventory, GatewayRequest, InventoryBinding};
use crate::auth::events::ApiPrincipalEstablished;
use crate::gateway::error::BitrouterError;
use crate::gateway::metering::replay::CanonicalRequestReplayRejected;
use crate::gateway::pipeline::context::{PipelineContext, StreamContext};
use crate::gateway::pipeline::hooks::{
    HookDecision, HopOutcome, ObserveHook, Phase, PreRequestHook, RequestOutcome,
};
use crate::gateway::pipeline::settlement::{SettlementContext, SettlementRecorder};
use crate::gateway::pipeline::types::RoutingTarget;
use crate::metering::recorder::MeteringSettlementEvent;
use crate::session_identity::RequestSessionContext;
use crate::storage::capture::{connections, events, sessions};
use crate::storage::evolution::EvolutionStore;
use crate::storage::execution::{
    EXECUTION_KIND, ExecutionHop, ExecutionOutcome, ExecutionRecord, ExecutionSettlement,
};
use crate::storage::session_identity::SessionIdentity;

#[derive(Debug, Serialize)]
pub(crate) struct GatewayRequestObserved {
    pub(crate) request: GatewayRequest,
    #[serde(skip)]
    hops: Arc<Mutex<Vec<ExecutionHop>>>,
}

impl PipelineEvent for GatewayRequestObserved {
    fn event_name(&self) -> &'static str {
        "evolution.gateway_request_observed"
    }
}

/// Request evidence independent of optional policy learning and capture workers.
#[derive(Clone)]
pub struct GatewayRequestTracker {
    db: DatabaseConnection,
    inventory: GatewayInventory,
}

impl GatewayRequestTracker {
    pub fn new(db: DatabaseConnection) -> Self {
        Self {
            inventory: GatewayInventory::new(db.clone()),
            db,
        }
    }

    /// The control listener must share this exact runtime epoch with serving.
    pub fn inventory(&self) -> GatewayInventory {
        self.inventory.clone()
    }

    pub(crate) async fn identify(
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
            .filter(crate::storage::legacy::scope("acp_capture_connections"))
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
                let Some(row) = sessions::Entity::find_by_id(&key)
                    .filter(crate::storage::legacy::scope("acp_canonical_sessions"))
                    .one(&self.db)
                    .await?
                else {
                    continue;
                };
                if row.deleted {
                    continue;
                }
                let observed = events::Entity::find()
                    .filter(crate::storage::legacy::scope("acp_capture_events"))
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
            request: observed,
            hops: Arc::new(Mutex::new(Vec::new())),
        });
        Ok(())
    }
}

pub(super) fn internal(error: anyhow::Error) -> BitrouterError {
    tracing::warn!(%error, "canonical evolution operation failed");
    BitrouterError::internal("canonical evolution state could not be verified")
}

#[async_trait]
impl PreRequestHook for GatewayRequestTracker {
    async fn revalidate_context(
        &self,
        _ctx: &PipelineContext,
    ) -> crate::gateway::error::Result<HookDecision> {
        // Authorized history removal keeps the request identity and metadata
        // fixed. Reuse its inventory observation; admitting again would reject
        // this same execution as a replay. Live policy fences belong to their
        // original preparation/route hooks and are revalidated separately.
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

#[async_trait]
impl ObserveHook for GatewayRequestTracker {
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
impl SettlementRecorder for GatewayRequestTracker {
    async fn record(&self, ctx: &mut SettlementContext) -> crate::gateway::error::Result<()> {
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
            && ctx.usage_origin == bitrouter_ai::types::UsageOrigin::ProviderReported
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
