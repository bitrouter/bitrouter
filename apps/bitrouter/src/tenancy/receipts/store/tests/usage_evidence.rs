//! Actual shared-budget reads, with SQL observations before any payload transfer.
use super::*;
use crate::storage::operation_budget::{Error as BudgetError, OperationBudget};
use crate::tenancy::receipts::usage_evidence::{Snapshot, storage};
use sea_orm::{DbErr, ExecResult, Statement};
use std::sync::atomic::{AtomicUsize, Ordering};

fn captured(id: &str) -> Result<Candidate> {
    candidate("installation", "tenant", id, "creator")?.with_usage_evidence(Snapshot::default())
}

#[tokio::test]
async fn native_and_recovery_reads_fit_128_small_sidecars_on_one_partly_used_budget() -> Result<()>
{
    let db = open().await?;
    let tx = db.begin().await?;
    let mut values = Vec::new();
    for index in 0..128 {
        let value = captured(&format!("execution-{index}"))?;
        ensure!(insert(&tx, &value).await?);
        values.push(value);
    }
    tx.commit().await?;
    let check = || Ok(());
    let budget = OperationBudget::new(&check);
    // Other cohort participants already consumed this same operation budget.
    // No per-receipt reset or larger profile is available to the reader.
    budget.charge(0, 24 * 1024 * 1024)?;
    let tx = db.begin().await?;
    for value in &values {
        let receipt = &value.receipt;
        let loaded = native_namespace_candidate_in(
            &tx,
            &receipt.installation_id,
            &receipt.tenant_id,
            &receipt.execution_id,
            &budget,
        )
        .await?
        .context("native receipt")?;
        ensure!(loaded.digest == value.digest);
        ensure!(
            loaded.usage_evidence.context("native sidecar")?.json
                == value
                    .usage_evidence
                    .as_ref()
                    .context("original sidecar")?
                    .json
        );
    }
    let (_, used) = budget.usage()?;
    ensure!((24 * 1024 * 1024..32 * 1024 * 1024).contains(&used));
    budget.charge(0, 32 * 1024 * 1024 - used)?;
    let receipt = &values[0].receipt;
    let error = native_namespace_candidate_in(
        &tx,
        &receipt.installation_id,
        &receipt.tenant_id,
        &receipt.execution_id,
        &budget,
    )
    .await
    .err()
    .context("shared cap")?;
    ensure!(matches!(
        error.downcast_ref::<BudgetError>(),
        Some(BudgetError::Capacity)
    ));

    let recovery = crate::tenancy::session::recovery::Budget::default();
    recovery.charge(0, 24 * 1024 * 1024)?;
    for value in &values {
        let receipt = &value.receipt;
        let loaded = recovery_lookup(
            &tx,
            &receipt.installation_id,
            &receipt.tenant_id,
            &receipt.execution_id,
            &recovery,
        )
        .await?
        .context("recovery receipt")?;
        ensure!(loaded.digest == value.digest && loaded.usage_evidence.is_some());
    }
    ensure!(recovery.charge(0, 8 * 1024 * 1024).is_err());
    tx.rollback().await?;
    db.close().await?;
    Ok(())
}

/// Observe the actual SeaORM query/results. Optional corruption occurs only
/// after scalar preflight has returned and before the payload SELECT executes.
struct Probe<'a> {
    db: &'a DatabaseConnection,
    mutation: Option<Statement>,
    payload_queries: AtomicUsize,
    transferred: AtomicUsize,
}
impl<'a> Probe<'a> {
    fn new(db: &'a DatabaseConnection) -> Self {
        Self {
            db,
            mutation: None,
            payload_queries: AtomicUsize::new(0),
            transferred: AtomicUsize::new(0),
        }
    }
}
#[async_trait::async_trait]
impl ConnectionTrait for Probe<'_> {
    fn get_database_backend(&self) -> DatabaseBackend {
        self.db.get_database_backend()
    }
    async fn execute(&self, statement: Statement) -> std::result::Result<ExecResult, DbErr> {
        self.db.execute(statement).await
    }
    async fn execute_unprepared(&self, sql: &str) -> std::result::Result<ExecResult, DbErr> {
        self.db.execute_unprepared(sql).await
    }
    async fn query_one(
        &self,
        statement: Statement,
    ) -> std::result::Result<Option<QueryResult>, DbErr> {
        self.db.query_one(statement).await
    }
    async fn query_all(
        &self,
        statement: Statement,
    ) -> std::result::Result<Vec<QueryResult>, DbErr> {
        let sql = statement.to_string();
        let metadata = sql.contains("AS \"evidence_bytes\"");
        let payload = sql.contains("AS \"evidence_json\"");
        if payload {
            self.payload_queries.fetch_add(1, Ordering::SeqCst);
        }
        let rows = self.db.query_all(statement).await?;
        if payload {
            for row in &rows {
                if let Some(json) = row.try_get::<Option<String>>("", "evidence_json")? {
                    self.transferred.fetch_add(json.len(), Ordering::SeqCst);
                }
            }
        }
        if metadata && let Some(mutation) = &self.mutation {
            self.db.execute(mutation.clone()).await?;
        }
        Ok(rows)
    }
}

async fn bounded(
    probe: &Probe<'_>,
    value: &mut Candidate,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    let expected = value
        .usage_evidence
        .as_ref()
        .context("original capture")?
        .digest
        .clone();
    storage::validate_bounded(probe, value, Some(&expected), &|rows, bytes| {
        Ok(budget.charge(rows, bytes)?)
    })
    .await
}
async fn restore(db: &DatabaseConnection, value: &Candidate) -> Result<()> {
    let evidence = value.usage_evidence.as_ref().context("original capture")?;
    db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE tenancy_usage_evidence SET evidence_json=?,byte_count=?,schema_version=1,evidence_digest=?,receipt_digest=?",
        [evidence.json.clone().into(), i64::try_from(evidence.json.len())?.into(), evidence.digest.clone().into(), value.digest.clone().into()]
    )).await?;
    Ok(())
}

#[tokio::test]
async fn evidence_preflight_charges_actual_bytes_and_preserves_typed_capacity_refusal() -> Result<()>
{
    let db = open().await?;
    let mut value = captured("execution")?;
    commit(&db, &value).await?;
    let bytes = value.usage_evidence.as_ref().context("capture")?.json.len();
    let check = || Ok(());
    let budget = OperationBudget::new(&check);
    let probe = Probe::new(&db);
    bounded(&probe, &mut value, &budget).await?;
    ensure!(budget.usage()? == (4, 1024 + 3 * (bytes + 128) + 4096));
    ensure!(
        probe.payload_queries.load(Ordering::SeqCst) == 1
            && probe.transferred.load(Ordering::SeqCst) == bytes
    );

    for (rows, limit) in [(4096, 1024), (2, 32 * 1024 * 1024)] {
        let budget = OperationBudget::with_limits(&check, rows, limit);
        let probe = Probe::new(&db);
        let error = bounded(&probe, &mut value, &budget)
            .await
            .err()
            .context("bounded refusal")?;
        ensure!(matches!(
            error.downcast_ref::<BudgetError>(),
            Some(BudgetError::Capacity)
        ));
        ensure!(probe.payload_queries.load(Ordering::SeqCst) == 0);
    }
    let rejected = || Err(bitrouter_ai::tenancy::operation::ErrorCode::Forbidden);
    let probe = Probe::new(&db);
    let error = bounded(&probe, &mut value, &OperationBudget::new(&rejected))
        .await
        .err()
        .context("authority refusal")?;
    ensure!(matches!(
        error.downcast_ref::<BudgetError>(),
        Some(BudgetError::Rejected(_))
    ));
    ensure!(probe.payload_queries.load(Ordering::SeqCst) == 0);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn oversized_or_misstated_evidence_is_refused_before_payload_transfer() -> Result<()> {
    let db = open().await?;
    let original = captured("execution")?;
    commit(&db, &original).await?;
    let check = || Ok(());
    for mutation in [
        "UPDATE tenancy_usage_evidence SET evidence_json=printf('%262145s','x'),byte_count=1",
        "UPDATE tenancy_usage_evidence SET evidence_json=printf('%262145s','x'),byte_count=262145",
        "UPDATE tenancy_usage_evidence SET byte_count=1",
        "UPDATE tenancy_usage_evidence SET byte_count=printf('%300000s','x')",
        "UPDATE tenancy_usage_evidence SET schema_version=printf('%300000s','x')",
        "UPDATE tenancy_usage_evidence SET evidence_digest=printf('%300000s','x')",
    ] {
        restore(&db, &original).await?;
        db.execute_unprepared(mutation).await?;
        let probe = Probe::new(&db);
        let budget = OperationBudget::new(&check);
        ensure!(
            bounded(&probe, &mut captured("execution")?, &budget)
                .await
                .is_err(),
            "{mutation}"
        );
        ensure!(
            probe.payload_queries.load(Ordering::SeqCst) == 0
                && probe.transferred.load(Ordering::SeqCst) == 0
        );
        ensure!(budget.usage()? == (2, 1024));
    }
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn changed_length_cannot_transfer_more_than_preflight_reserved() -> Result<()> {
    let db = open().await?;
    let original = captured("execution")?;
    commit(&db, &original).await?;
    let bytes = original
        .usage_evidence
        .as_ref()
        .context("capture")?
        .json
        .len();
    let check = || Ok(());
    for size in [bytes + 1, 262144, 262145] {
        restore(&db, &original).await?;
        let mut probe = Probe::new(&db);
        probe.mutation = Some(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE tenancy_usage_evidence SET evidence_json=?,byte_count=?",
            ["x".repeat(size).into(), i64::try_from(size)?.into()],
        ));
        let budget = OperationBudget::new(&check);
        ensure!(
            bounded(&probe, &mut captured("execution")?, &budget)
                .await
                .is_err()
        );
        ensure!(
            probe.payload_queries.load(Ordering::SeqCst) == 1
                && probe.transferred.load(Ordering::SeqCst) == 0,
            "CASE must withhold a changed cell before transfer"
        );
        ensure!(budget.usage()? == (4, 1024 + 3 * (bytes + 128) + 4096));
    }
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn budgeted_evidence_still_checks_presence_multiplicity_canonical_bytes_and_bindings()
-> Result<()> {
    let db = open().await?;
    let original = captured("execution")?;
    commit(&db, &original).await?;
    let check = || Ok(());
    for mutation in [
        "UPDATE tenancy_usage_evidence SET receipt_digest=printf('%064d',0)",
        "UPDATE tenancy_usage_evidence SET evidence_digest=printf('%064d',0)",
        "UPDATE tenancy_usage_evidence SET evidence_json=evidence_json || ' ',byte_count=byte_count+1",
        "UPDATE tenancy_usage_evidence SET schema_version=2",
    ] {
        restore(&db, &original).await?;
        db.execute_unprepared(mutation).await?;
        ensure!(
            bounded(
                &Probe::new(&db),
                &mut captured("execution")?,
                &OperationBudget::new(&check)
            )
            .await
            .is_err(),
            "{mutation}"
        );
    }
    restore(&db, &original).await?;
    let probe = Probe::new(&db);
    let budget = OperationBudget::new(&check);
    ensure!(
        storage::validate_bounded(
            &probe,
            &mut captured("execution")?,
            None,
            &|rows, bytes| Ok(budget.charge(rows, bytes)?)
        )
        .await
        .is_err(),
        "orphan sidecar"
    );
    ensure!(probe.payload_queries.load(Ordering::SeqCst) == 0);
    db.execute_unprepared("ALTER TABLE tenancy_usage_evidence RENAME TO original_evidence")
        .await?;
    db.execute_unprepared("CREATE TABLE tenancy_usage_evidence AS SELECT * FROM original_evidence")
        .await?;
    db.execute_unprepared("INSERT INTO tenancy_usage_evidence SELECT * FROM original_evidence")
        .await?;
    let probe = Probe::new(&db);
    ensure!(
        bounded(
            &probe,
            &mut captured("execution")?,
            &OperationBudget::new(&check)
        )
        .await
        .is_err(),
        "duplicate sidecar"
    );
    ensure!(probe.payload_queries.load(Ordering::SeqCst) == 0);
    db.execute_unprepared("DELETE FROM tenancy_usage_evidence")
        .await?;
    ensure!(
        bounded(
            &Probe::new(&db),
            &mut captured("execution")?,
            &OperationBudget::new(&check)
        )
        .await
        .is_err(),
        "missing bound sidecar"
    );
    let mut historical = candidate("installation", "tenant", "execution", "creator")?;
    let budget = OperationBudget::new(&check);
    storage::validate_bounded(&Probe::new(&db), &mut historical, None, &|rows, bytes| {
        Ok(budget.charge(rows, bytes)?)
    })
    .await?;
    ensure!(historical.usage_evidence.is_none());
    db.close().await?;
    Ok(())
}
