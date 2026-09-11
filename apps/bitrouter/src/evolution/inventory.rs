//! An acknowledged inventory of gateway requests, including unresolved identity.
//!
//! Capture is registered before its first event. Every authenticated model
//! request under that controller is inserted before dispatch, even when it
//! cannot be attributed to a native session. Coverage revisions fence resource
//! snapshots and policy publication without changing canonical content heads.

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, DatabaseConnection, DatabaseTransaction, EntityTrait, QueryFilter,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};

use super::rubric::digest;
use super::runtime::{EXECUTION_KIND, ExecutionOutcome, ExecutionRecord, ExecutionSettlement};
use super::store::{EvolutionStore, records};
use crate::acp_trajectory::{
    SessionIdentity,
    entities::{connections, sessions},
};

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
    /// All recordings under the authenticated controller at admission.
    pub anchors: BTreeMap<String, i64>,
    pub binding: Option<InventoryBinding>,
    pub coverage_ready: bool,
    pub settlement: Option<ExecutionSettlement>,
}

pub(crate) enum BeginResult {
    Untracked,
    Duplicate,
    Tracked(Box<GatewayRequest>),
}

#[derive(Clone)]
pub struct GatewayInventory {
    pub(crate) db: DatabaseConnection,
    runtime_epoch: String,
}

impl GatewayInventory {
    pub fn new(db: DatabaseConnection) -> Self {
        Self {
            db,
            runtime_epoch: uuid::Uuid::new_v4().to_string(),
        }
    }

    pub(crate) fn scoped_store(&self, principal: &str, controller: &str) -> Result<EvolutionStore> {
        Self::store_for(self.db.clone(), principal, controller)
    }

    pub(crate) fn store_for(
        db: DatabaseConnection,
        principal: &str,
        controller: &str,
    ) -> Result<EvolutionStore> {
        let scope = digest(&(INVENTORY_VERSION, principal, controller))?;
        EvolutionStore::new(db, &format!("gateway:{scope}"))
    }

    /// Called by the local control socket, before the recorder forwards any
    /// traffic. Re-registering a used connection cannot manufacture coverage.
    pub async fn register_capture(
        &self,
        id: &str,
        principal: &str,
        controller: &str,
    ) -> Result<CoverageRegistration> {
        let tx = self.db.begin().await?;
        let row = lock_connection(&tx, id).await?;
        ensure!(
            row.head == 0 && row.state == "recording",
            "coverage registration must precede capture"
        );
        ensure!(
            row.route_scope_id.as_deref() == Some(principal)
                && row.controller_instance_id.as_deref() == Some(controller),
            "recording namespace mismatch"
        );
        let inventory = self.scoped_store(principal, controller)?;
        for request in records::Entity::find()
            .filter(records::Column::ScopeId.eq(&inventory.scope_id))
            .filter(records::Column::Kind.eq(INVENTORY_KIND))
            .all(&tx)
            .await?
        {
            let request: GatewayRequest = serde_json::from_str(&request.body)?;
            ensure!(
                !request.anchors.contains_key(id),
                "coverage registration must precede gateway requests"
            );
        }
        let store = EvolutionStore::new(self.db.clone(), &row.owner)?;
        let fresh = CoverageRegistration {
            version: INVENTORY_VERSION.into(),
            connection_id: id.into(),
            runtime_epoch: self.runtime_epoch.clone(),
            registered_at: chrono::Utc::now().to_rfc3339(),
            invalid_reason: None,
        };
        let (_, registered): (_, CoverageRegistration) = store
            .initialize_in(&tx, COVERAGE_KIND, id, None, &fresh)
            .await?;
        ensure!(
            registered.runtime_epoch == self.runtime_epoch && registered.invalid_reason.is_none(),
            "recording belongs to a different gateway runtime; reconnect capture"
        );
        tx.commit().await?;
        Ok(registered)
    }

    pub(crate) async fn begin(
        &self,
        principal: &str,
        controller: &str,
        request_id: &str,
        mut binding: Option<InventoryBinding>,
    ) -> Result<BeginResult> {
        ensure!(
            !principal.trim().is_empty()
                && !controller.trim().is_empty()
                && !request_id.trim().is_empty(),
            "gateway request namespace is empty"
        );
        let store = self.scoped_store(principal, controller)?;
        if store
            .get::<GatewayRequest>(INVENTORY_KIND, request_id)
            .await?
            .is_some()
        {
            return Ok(BeginResult::Duplicate);
        }
        let mut candidates = connections::Entity::find()
            .filter(connections::Column::RouteScopeId.eq(principal))
            .filter(connections::Column::ControllerInstanceId.eq(controller))
            .filter(connections::Column::State.eq("recording"))
            .all(&self.db)
            .await?;
        if candidates.is_empty() {
            return Ok(BeginResult::Untracked);
        }
        candidates.sort_by(|a, b| a.connection_id.cmp(&b.connection_id));
        let tx = self.db.begin().await?;
        let mut anchors = BTreeMap::new();
        for candidate in candidates {
            let row = lock_connection(&tx, &candidate.connection_id).await?;
            if row.state == "recording" {
                anchors.insert(row.connection_id, row.head);
            }
        }
        if anchors.is_empty() {
            tx.commit().await?;
            return Ok(BeginResult::Untracked);
        }
        if let Some(proposed) = binding.as_mut() {
            let key = proposed.identity.key()?;
            sessions::Entity::update_many()
                .col_expr(
                    sessions::Column::Head,
                    Expr::col(sessions::Column::Head).into(),
                )
                .filter(sessions::Column::SessionKey.eq(&key))
                .exec(&tx)
                .await?;
            match sessions::Entity::find_by_id(&key).one(&tx).await? {
                Some(row) if !row.deleted && anchors.contains_key(&proposed.connection_id) => {
                    proposed.watermark = row.head;
                }
                _ => binding = None,
            }
        }
        if records::Entity::find_by_id(store.id(INVENTORY_KIND, request_id)?)
            .one(&tx)
            .await?
            .is_some()
        {
            tx.commit().await?;
            return Ok(BeginResult::Duplicate);
        }
        let coverage_ready = self.touch_coverage(&tx, &anchors).await?;
        let request = GatewayRequest {
            request_id: request_id.into(),
            attempt_id: uuid::Uuid::new_v4().to_string(),
            route_scope_id: principal.into(),
            controller_instance_id: controller.into(),
            runtime_epoch: self.runtime_epoch.clone(),
            admitted_at: chrono::Utc::now().to_rfc3339(),
            anchors,
            binding,
            coverage_ready,
            settlement: None,
        };
        let session_key = request
            .binding
            .as_ref()
            .map(|bound| bound.identity.key())
            .transpose()?;
        let (_, saved): (_, GatewayRequest) = store
            .initialize_in(&tx, INVENTORY_KIND, request_id, session_key, &request)
            .await?;
        if saved.attempt_id != request.attempt_id {
            tx.commit().await?;
            return Ok(BeginResult::Duplicate);
        }
        tx.commit().await?;
        Ok(BeginResult::Tracked(Box::new(saved)))
    }

    /// Source connections are locked before inventory/execution rows. The
    /// learner takes the same locks when validating a resource snapshot.
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
        let tx = self.db.begin().await?;
        for id in observed.anchors.keys() {
            lock_connection(&tx, id).await?;
        }
        let store =
            self.scoped_store(&observed.route_scope_id, &observed.controller_instance_id)?;
        let (row, mut request): (_, GatewayRequest) = store
            .lock(&tx, INVENTORY_KIND, &observed.request_id)
            .await?;
        ensure!(
            request.attempt_id == observed.attempt_id,
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
            self.touch_coverage(&tx, &request.anchors).await?;
            if let Some(binding) = &request.binding {
                let bound_store = EvolutionStore::new(self.db.clone(), &binding.identity.owner)?;
                let id = bound_store.id(EXECUTION_KIND, &request.request_id)?;
                if records::Entity::find_by_id(&id).one(&tx).await?.is_some() {
                    let (bound_row, mut execution): (_, ExecutionRecord) = bound_store
                        .lock(&tx, EXECUTION_KIND, &request.request_id)
                        .await?;
                    ensure!(
                        execution.attempt_id == request.attempt_id,
                        "bound execution attempt mismatch"
                    );
                    execution.settlement = request.settlement.clone();
                    bound_store.save(&tx, bound_row, &execution).await?;
                }
            }
            store.save(&tx, row, &request).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn touch_coverage(
        &self,
        tx: &DatabaseTransaction,
        anchors: &BTreeMap<String, i64>,
    ) -> Result<bool> {
        let mut complete = true;
        for id in anchors.keys() {
            let connection = connections::Entity::find_by_id(id)
                .one(tx)
                .await?
                .context("capture disappeared")?;
            let store = EvolutionStore::new(self.db.clone(), &connection.owner)?;
            let record_id = store.id(COVERAGE_KIND, id)?;
            if records::Entity::find_by_id(record_id)
                .one(tx)
                .await?
                .is_none()
            {
                complete = false;
                continue;
            }
            let (row, mut coverage): (_, CoverageRegistration) =
                store.lock(tx, COVERAGE_KIND, id).await?;
            if coverage.runtime_epoch != self.runtime_epoch {
                coverage.invalid_reason = Some("gateway_runtime_changed_during_capture".into());
            }
            complete &= coverage.version == INVENTORY_VERSION && coverage.invalid_reason.is_none();
            // The generic record revision is the write fence, even if only an
            // unassigned request arrived and canonical content did not change.
            store.save(tx, row, &coverage).await?;
        }
        Ok(complete)
    }
}

/// Callers publishing a snapshot first lock these capture connections. Missing
/// registration is itself fenced, so a later acknowledgement cannot be hidden.
pub(crate) async fn coverage_matches(
    store: &EvolutionStore,
    db: &impl sea_orm::ConnectionTrait,
    revisions: &BTreeMap<String, Option<i64>>,
) -> Result<bool> {
    for (id, expected) in revisions {
        let actual = store
            .get_in::<CoverageRegistration>(db, COVERAGE_KIND, id)
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
        .col_expr(
            connections::Column::Head,
            Expr::col(connections::Column::Head).into(),
        )
        .filter(connections::Column::ConnectionId.eq(id))
        .exec(tx)
        .await?;
    connections::Entity::find_by_id(id)
        .one(tx)
        .await?
        .context("capture connection is missing")
}
