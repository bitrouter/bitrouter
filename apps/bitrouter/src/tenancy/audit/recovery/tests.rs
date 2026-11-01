use super::*;
use crate::offline::{ValidationBudget, recovery::plan::OperationObservation};
use crate::tenancy::model::ReadContext;
use bitrouter_ai::tenancy::operation::{OperationResult, OperationStatus};
use sea_orm::{
    DatabaseBackend, DatabaseTransaction, DbErr, ExecResult, QueryResult, TransactionTrait,
};
use std::sync::Mutex;

#[derive(Default)]
struct Observation {
    body_queries: usize,
    body_rows: usize,
    max_body_rows: usize,
    max_body_bytes: usize,
}

// Observe actual result payloads, not just the eventual validation error. This
// detects a page of large bodies even if a later check correctly rejects it.
struct Observed<'a> {
    tx: &'a DatabaseTransaction,
    reads: Mutex<Observation>,
}
impl Observed<'_> {
    fn record(&self, rows: &[QueryResult]) -> Result<(), DbErr> {
        let mut body_rows = 0;
        let mut bytes = 0;
        for row in rows {
            if let Ok(Some(body)) = row.try_get::<Option<String>>("", "response_json") {
                body_rows += 1;
                bytes += body.len();
            }
        }
        let mut seen = self
            .reads
            .lock()
            .map_err(|_| DbErr::Custom("observation poisoned".into()))?;
        seen.body_queries += usize::from(body_rows != 0);
        seen.body_rows += body_rows;
        seen.max_body_rows = seen.max_body_rows.max(body_rows);
        seen.max_body_bytes = seen.max_body_bytes.max(bytes);
        Ok(())
    }
}
#[async_trait::async_trait]
impl ConnectionTrait for Observed<'_> {
    fn get_database_backend(&self) -> DatabaseBackend {
        self.tx.get_database_backend()
    }
    async fn execute(&self, _: Statement) -> Result<ExecResult, DbErr> {
        Err(DbErr::Custom("fixture read-only transaction".into()))
    }
    async fn execute_unprepared(&self, _: &str) -> Result<ExecResult, DbErr> {
        Err(DbErr::Custom("fixture read-only transaction".into()))
    }
    async fn query_one(&self, statement: Statement) -> Result<Option<QueryResult>, DbErr> {
        let row = self.tx.query_one(statement).await?;
        self.record(row.as_slice())?;
        Ok(row)
    }
    async fn query_all(&self, statement: Statement) -> Result<Vec<QueryResult>, DbErr> {
        let rows = self.tx.query_all(statement).await?;
        self.record(&rows)?;
        Ok(rows)
    }
}

async fn fixture() -> Result<(sea_orm::DatabaseConnection, Provisioning)> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let mut state = store::initial_header(&db, chrono::Utc::now()).await?;
    let mut observed = Provisioning {
        format: "fixture".into(),
        installation_id: state.installation.clone(),
        operations: Vec::new(),
        configurations: Vec::new(),
        active: Vec::new(),
        resources: Vec::new(),
    };
    let tx = db.begin().await?;
    for index in 1..=65_u64 {
        state.operation_ids = index;
        state.revision = index.try_into()?;
        let result = OperationResult {
            operation_id: format!("operation-{index:03}").try_into()?,
            status: OperationStatus::Applied,
            authority_revision: Some(state.revision),
            resource_revision: Some("r1".try_into()?),
            resource_id: Some("resource".try_into()?),
            error: None,
        };
        let principal = PrincipalId::for_control_credential("fixture")?;
        let operation = store::Operation {
            principal: principal.clone(),
            id: result.operation_id.clone(),
            context: ReadContext::Installation,
            digest: "a".repeat(64),
            result: result.clone(),
            response: store::StoredResponse::Operation(result.clone()),
            created_at: chrono::Utc::now(),
            safety: false,
        };
        store::write_in(&tx, &state, &[], &operation).await?;
        observed.operations.push(OperationObservation {
            principal_id: principal,
            result,
        });
    }
    // Whitespace is legal JSON and survives in retained bytes. Every body fits
    // separately, while a 64-body page exceeds the smaller working allowance.
    tx.execute_unprepared(
        "UPDATE tenancy_operations SET response_json=printf('%131072s',response_json)",
    )
    .await?;
    tx.commit().await?;
    Ok((db, observed))
}

/// Read all pages successfully without ever transferring a page of bodies.
#[tokio::test]
async fn fresh_readback_transfers_only_one_preflighted_operation_at_a_time() -> Result<()> {
    let (db, observed) = fixture().await?;
    let tx = db.begin().await?;
    {
        let reader = Observed {
            tx: &tx,
            reads: Mutex::new(Observation::default()),
        };
        let check = || Ok(());
        let state = fresh_operations(
            &reader,
            &observed.installation_id,
            &observed,
            &OperationBudget::new(&check),
            ValidationBudget::default(),
        )
        .await?;
        ensure!(state.operation_ids == 65);
        {
            let seen = reader
                .reads
                .lock()
                .map_err(|_| anyhow::anyhow!("observation poisoned"))?;
            ensure!(seen.body_queries >= 65 && seen.body_rows >= 65);
            ensure!(seen.max_body_rows == 1 && seen.max_body_bytes == 128 * 1024);
        }
    }
    tx.rollback().await?;
    db.close().await?;
    Ok(())
}

/// A later refusal must not be preceded by a transfer above the working limit.
#[tokio::test]
async fn fresh_small_budget_refuses_without_materializing_history_bodies() -> Result<()> {
    let (db, observed) = fixture().await?;
    let tx = db.begin().await?;
    {
        let reader = Observed {
            tx: &tx,
            reads: Mutex::new(Observation::default()),
        };
        let check = || Ok(());
        let limit = ValidationBudget {
            rows: 32,
            bytes: 1024 * 1024,
        };
        let error = fresh_operations(
            &reader,
            &observed.installation_id,
            &observed,
            &OperationBudget::new(&check),
            limit,
        )
        .await
        .err()
        .context("small budget unexpectedly accepted")?;
        ensure!(error.is::<store::directory::CapacityExceeded>());
        {
            let seen = reader
                .reads
                .lock()
                .map_err(|_| anyhow::anyhow!("observation poisoned"))?;
            // The authority scan can cross every page; the independent fixed
            // evidence envelope then refuses. Total history is not the budget.
            ensure!(seen.body_rows >= 65);
            ensure!(seen.max_body_rows == 1 && seen.max_body_bytes <= limit.bytes);
        }
    }
    tx.rollback().await?;
    db.close().await?;
    Ok(())
}
