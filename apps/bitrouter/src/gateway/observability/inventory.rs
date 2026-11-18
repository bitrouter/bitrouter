//! Service-owned, predispatch request reservations and their settlements.
//! Legacy coverage helpers also read the independent historical capture format.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, ensure};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, DatabaseConnection, DatabaseTransaction, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect,
};
use serde::{Deserialize, Serialize};

use crate::storage::capture::connections;
use crate::storage::evolution::digest;
use crate::storage::evolution::{EvolutionStore, records};
use crate::storage::execution::ExecutionOutcome;
use crate::storage::execution::{EXECUTION_KIND, ExecutionSettlement};
use crate::storage::session_identity::SessionIdentity;
use crate::storage::write::Write;

#[cfg(all(test, any(feature = "db-postgres", feature = "db-mysql")))]
mod server_tests;

pub const INVENTORY_VERSION: &str = "gateway-request-inventory-v1";
pub(crate) const COVERAGE_KIND: &str = "gateway_coverage";
pub(crate) const INVENTORY_KIND: &str = "gateway_request";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageRegistration {
    pub version: String,
    pub connection_id: String,
    pub runtime_epoch: String,
    pub registered_at: String,
    pub invalid_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryBinding {
    pub identity: SessionIdentity,
    pub connection_id: String,
    pub watermark: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayRequest {
    pub request_id: String,
    pub attempt_id: String,
    pub route_scope_id: String,
    pub controller_instance_id: String,
    pub runtime_epoch: String,
    pub admitted_at: String,
    /// Historical capture watermarks. Service-only reservations leave this empty.
    pub anchors: BTreeMap<String, i64>,
    pub binding: Option<InventoryBinding>,
    pub coverage_ready: bool,
    pub settlement: Option<ExecutionSettlement>,
}

#[derive(Debug, thiserror::Error)]
#[error("request ID already reserved")]
struct PriorRequest;

pub(crate) enum BeginResult {
    Duplicate,
    Tracked(Box<GatewayRequest>),
}

#[derive(Clone)]
pub struct GatewayInventory {
    pub(crate) db: DatabaseConnection,
    runtime_epoch: String,
    storage_budget: Option<Arc<OnceLock<crate::tenancy::storage::Budget>>>,
}

impl GatewayInventory {
    pub fn new(db: DatabaseConnection) -> Self {
        Self {
            db,
            runtime_epoch: uuid::Uuid::new_v4().to_string(),
            storage_budget: None,
        }
    }

    /// Share publication of the tenant owner's budget with serving and control clones.
    pub(crate) fn with_storage_budget(
        mut self,
        budget: Arc<OnceLock<crate::tenancy::storage::Budget>>,
    ) -> Self {
        self.storage_budget = Some(budget);
        self
    }

    pub(crate) fn store_for(
        db: DatabaseConnection,
        principal: &str,
        controller: &str,
    ) -> Result<EvolutionStore> {
        let scope = digest(&(INVENTORY_VERSION, principal, controller))?;
        EvolutionStore::new(db, &format!("gateway:{scope}"))
    }

    // A single service storage scope lets the historical lookup range over only
    // old ownership scopes. Metering and admission share a global request ID.
    fn service_store(&self) -> Result<EvolutionStore> {
        EvolutionStore::new(self.db.clone(), "gateway-request-admission-v2")
            .map(|store| store.with_storage_budget(self.storage_budget.clone()))
    }

    /// Read a service admission using its authenticated route scope and request ID.
    /// The controller and claimed session have no authority over this lookup.
    pub async fn request(
        &self,
        principal: &str,
        request_id: &str,
    ) -> Result<Option<GatewayRequest>> {
        let key = digest(&request_id)?;
        let row = self
            .service_store()?
            .get::<GatewayRequest>(INVENTORY_KIND, &key)
            .await?;
        row.filter(|(_, request)| request.route_scope_id == principal)
            .map(|(_, request)| {
                ensure!(
                    request.route_scope_id == principal && request.request_id == request_id,
                    "gateway request identity mismatch"
                );
                Ok(request)
            })
            .transpose()
    }

    /// M0 inventory and execution records have hashed keys under controller
    /// and capture-owner scopes. Probe their original keys without parsing
    /// unrelated payloads. In fresh stores both indexed ranges are empty.
    async fn imported_id(
        &self,
        tx: &DatabaseTransaction,
        service: &EvolutionStore,
        id: &str,
    ) -> Result<bool> {
        let scopes = records::Entity::find()
            .select_only()
            .columns([records::Column::ScopeId, records::Column::Kind])
            .distinct()
            .filter(crate::storage::legacy::scope("checkpoint_evolution"))
            .filter(records::Column::Kind.is_in([INVENTORY_KIND, EXECUTION_KIND]))
            .filter(
                sea_orm::Condition::any()
                    .add(records::Column::ScopeId.lt(&service.scope_id))
                    .add(records::Column::ScopeId.gt(&service.scope_id)),
            )
            .order_by_asc(records::Column::ScopeId)
            .order_by_asc(records::Column::Kind)
            .into_tuple::<(String, String)>();
        let mut pages = scopes.paginate(tx, 128);
        while let Some(scopes) = pages.fetch_and_next().await? {
            let mut execution_ids = Vec::new();
            let mut request_ids = Vec::new();
            for (scope, kind) in scopes {
                let key = digest(&(&scope, &kind, id))?;
                if kind == EXECUTION_KIND {
                    execution_ids.push(key);
                } else {
                    request_ids.push(key);
                }
            }
            // M0 executions have no authenticated route scope. If one owns
            // this ID, refuse it without reading its private session payload.
            if !execution_ids.is_empty()
                && records::Entity::find()
                    .select_only()
                    .column(records::Column::RecordId)
                    .filter(crate::storage::legacy::scope("checkpoint_evolution"))
                    .filter(records::Column::Kind.eq(EXECUTION_KIND))
                    .filter(records::Column::RecordId.is_in(execution_ids))
                    .into_tuple::<String>()
                    .one(tx)
                    .await?
                    .is_some()
            {
                return Ok(true);
            }
            if request_ids.is_empty() {
                continue;
            }
            let prior = records::Entity::find()
                .filter(crate::storage::legacy::scope("checkpoint_evolution"))
                .filter(records::Column::Kind.eq(INVENTORY_KIND))
                .filter(records::Column::RecordId.is_in(request_ids))
                .all(tx)
                .await?;
            if let Some(row) = prior.first() {
                let request: GatewayRequest = serde_json::from_str(&row.body)?;
                let original = Self::store_for(
                    self.db.clone(),
                    &request.route_scope_id,
                    &request.controller_instance_id,
                )?;
                ensure!(
                    request.request_id == id && original.scope_id == row.scope_id,
                    "invalid imported request identity"
                );
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) async fn begin(
        &self,
        principal: &str,
        controller: &str,
        request_id: &str,
    ) -> Result<BeginResult> {
        ensure!(
            !principal.trim().is_empty()
                && !controller.trim().is_empty()
                && !request_id.trim().is_empty(),
            "gateway request namespace is empty"
        );
        let store = self.service_store()?;
        let key = digest(&request_id)?;
        let candidate = GatewayRequest {
            request_id: request_id.into(),
            attempt_id: uuid::Uuid::new_v4().to_string(),
            route_scope_id: principal.into(),
            controller_instance_id: controller.into(),
            runtime_epoch: self.runtime_epoch.clone(),
            admitted_at: chrono::Utc::now().to_rfc3339(),
            anchors: BTreeMap::new(),
            binding: None,
            coverage_ready: false,
            settlement: None,
        };
        let (mut write, tx) = Write::begin(&self.db, &self.storage_budget, false).await?;
        let result = async {
            // Write before reading so concurrent SQLite transactions do not
            // upgrade stale read snapshots. initialize_charged reads the actual
            // winning row after a unique-key conflict on all three backends.
            let (_, saved) = store
                .initialize_charged(&mut write, &tx, INVENTORY_KIND, &key, None, &candidate)
                .await?;
            if saved.attempt_id != candidate.attempt_id {
                return Err(PriorRequest.into());
            }
            use crate::metering::entities::requests as metering;
            // Metering has a global request_id primary key, including old rows
            // without a route scope. No caller can replace an extant charge.
            if metering::Entity::find_by_id(request_id)
                .select_only()
                .column(metering::Column::RequestId)
                .into_tuple::<String>()
                .one(&tx)
                .await?
                .is_some()
                || self.imported_id(&tx, &store, request_id).await?
            {
                return Err(PriorRequest.into());
            }
            Ok(BeginResult::Tracked(Box::new(saved)))
        }
        .await;
        match write.finish(tx, result).await {
            Err(error) if error.is::<PriorRequest>() => Ok(BeginResult::Duplicate),
            other => other,
        }
    }

    pub(crate) async fn settle(
        &self,
        observed: &GatewayRequest,
        settlement: ExecutionSettlement,
    ) -> Result<()> {
        self.finish(observed, Some(settlement), None).await
    }

    pub(crate) async fn terminal(
        &self,
        observed: &GatewayRequest,
        outcome: ExecutionOutcome,
    ) -> Result<()> {
        self.finish(observed, None, Some(outcome)).await
    }

    async fn finish(
        &self,
        observed: &GatewayRequest,
        settlement: Option<ExecutionSettlement>,
        outcome: Option<ExecutionOutcome>,
    ) -> Result<()> {
        // Settlement and terminal publication complete this exact admitted
        // request; they cannot admit another upstream send.
        let (mut write, tx) = Write::begin(&self.db, &self.storage_budget, true).await?;
        let result = async {
            let store = self.service_store()?;
            let key = digest(&observed.request_id)?;
            let (row, mut request): (_, GatewayRequest) = store
                .lock_charged(&mut write, &tx, INVENTORY_KIND, &key)
                .await?;
            ensure!(
                request.attempt_id == observed.attempt_id
                    && request.route_scope_id == observed.route_scope_id,
                "gateway request attempt changed"
            );
            let previous = serde_json::to_string(&request.settlement)?;
            if let Some(settlement) = settlement
                && request.settlement.is_none()
            {
                request.settlement = Some(settlement);
            }
            if let Some(outcome) = outcome {
                let settlement = request
                    .settlement
                    .as_mut()
                    .context("request settlement is missing")?;
                if settlement.outcome.is_none() {
                    settlement.outcome = Some(outcome);
                }
            }
            if previous != serde_json::to_string(&request.settlement)? {
                write.replacement(&row, &request)?;
                store.save(&tx, row, &request).await?;
            }
            Ok(())
        }
        .await;
        write.finish(tx, result).await
    }
}

pub(crate) async fn coverage_matches_locked(
    store: &EvolutionStore,
    tx: &DatabaseTransaction,
    revisions: &BTreeMap<String, Option<i64>>,
) -> Result<bool> {
    for (id, expected) in revisions {
        let actual = store
            .get_current_in::<CoverageRegistration>(tx, COVERAGE_KIND, id)
            .await?
            .map(|(revision, _)| revision);
        if actual != *expected {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) async fn lock_connection(
    tx: &DatabaseTransaction,
    id: &str,
) -> Result<connections::Model> {
    connections::Entity::update_many()
        .filter(crate::storage::legacy::scope("acp_capture_connections"))
        .col_expr(
            connections::Column::Head,
            Expr::col(connections::Column::Head).into(),
        )
        .filter(connections::Column::ConnectionId.eq(id))
        .exec(tx)
        .await?;
    // Keep the write for SQLite, then read current state on MySQL even when
    // an earlier connection in this transaction established a read view.
    // https://dev.mysql.com/doc/refman/8.4/en/innodb-locking-reads.html
    connections::Entity::find_by_id(id)
        .filter(crate::storage::legacy::scope("acp_capture_connections"))
        .lock_exclusive()
        .one(tx)
        .await?
        .context("capture connection is missing")
}
