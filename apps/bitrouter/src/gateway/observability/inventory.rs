//! Service-owned, predispatch request reservations and their settlements.

use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, ensure};
use sea_orm::{DatabaseConnection, EntityTrait, QuerySelect};
use serde::{Deserialize, Serialize};

use crate::storage::execution::ExecutionOutcome;
use crate::storage::execution::ExecutionSettlement;
use crate::storage::gateway_records::GatewayRecords;
use crate::storage::gateway_records::digest;
#[cfg(all(test, any(feature = "db-postgres", feature = "db-mysql")))]
use crate::storage::gateway_records::records;
use crate::storage::write::Write;

#[cfg(all(test, any(feature = "db-postgres", feature = "db-mysql")))]
mod server_tests;

pub(crate) const INVENTORY_KIND: &str = "gateway_request";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayRequest {
    pub request_id: String,
    pub attempt_id: String,
    pub route_scope_id: String,
    pub controller_instance_id: String,
    pub runtime_epoch: String,
    pub admitted_at: String,
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

    // Admission and metering share one service-owned request namespace.
    fn service_store(&self) -> Result<GatewayRecords> {
        GatewayRecords::new(self.db.clone(), "gateway-request-admission")
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
            // Metering has a global request_id primary key. No caller may
            // replace an extant charge, even if its admission is missing.
            if metering::Entity::find_by_id(request_id)
                .select_only()
                .column(metering::Column::RequestId)
                .into_tuple::<String>()
                .one(&tx)
                .await?
                .is_some()
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
