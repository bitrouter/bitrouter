//! Observe driver transfers, including a mutation after scalar preflight.
use super::*;
use sea_orm::{DbErr, ExecResult, QueryResult};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Probe<'a> {
    db: &'a DatabaseConnection,
    mutation: Option<Statement>,
    payload_queries: AtomicUsize,
    transferred: AtomicUsize,
}
#[async_trait::async_trait]
impl ConnectionTrait for Probe<'_> {
    fn get_database_backend(&self) -> DatabaseBackend {
        DatabaseBackend::Sqlite
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
        let metadata = sql.contains("AS stored_bytes");
        let payload = sql.contains("AS payload");
        if payload {
            self.payload_queries.fetch_add(1, Ordering::SeqCst);
        }
        let rows = self.db.query_all(statement).await?;
        if payload {
            for row in &rows {
                if let Some(bytes) = row.try_get::<Option<Vec<u8>>>("", "payload")? {
                    self.transferred.fetch_add(bytes.len(), Ordering::SeqCst);
                }
            }
        }
        if metadata && let Some(mutation) = &self.mutation {
            self.db.execute(mutation.clone()).await?;
        }
        Ok(rows)
    }
}

#[tokio::test]
async fn oversized_or_misstated_archives_are_refused_before_blob_transfer() -> Result<()> {
    for mutation in [
        "UPDATE tenancy_receipt_archive SET payload=zeroblob(1310721)",
        "UPDATE tenancy_receipt_archive SET payload=CAST(payload AS TEXT)",
        "UPDATE tenancy_receipt_archive SET receipt_bytes=1048577",
        "UPDATE tenancy_receipt_archive SET usage_bytes=-1",
        "UPDATE tenancy_receipt_archive SET format_version=printf('%300000s','x')",
        "UPDATE tenancy_receipt_archive SET codec=printf('%300000s','x')",
    ] {
        let db = open().await?;
        let original = captured("execution")?;
        commit(&db, &original).await?;
        relocate(&db, &original).await?;
        db.execute_unprepared(mutation).await?;
        let probe = Probe {
            db: &db,
            mutation: None,
            payload_queries: AtomicUsize::new(0),
            transferred: AtomicUsize::new(0),
        };
        let r = &original.receipt;
        ensure!(
            load(
                &probe,
                &r.installation_id,
                &r.tenant_id,
                &r.execution_id,
                NO_CHARGE
            )
            .await
            .is_err()
        );
        ensure!(
            probe.payload_queries.load(Ordering::SeqCst) == 0
                && probe.transferred.load(Ordering::SeqCst) == 0
        );
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn growth_after_preflight_is_withheld_and_budget_rejection_precedes_transfer() -> Result<()> {
    let db = open().await?;
    let original = captured("execution")?;
    commit(&db, &original).await?;
    let prepared = relocate(&db, &original).await?;
    let r = &original.receipt;
    let probe = Probe {
        db: &db,
        mutation: None,
        payload_queries: AtomicUsize::new(0),
        transferred: AtomicUsize::new(0),
    };
    let reject_payload = |rows, _: usize| {
        ensure!(rows != 2, "test budget refusal");
        Ok(())
    };
    ensure!(
        load(
            &probe,
            &r.installation_id,
            &r.tenant_id,
            &r.execution_id,
            &reject_payload
        )
        .await
        .is_err()
    );
    ensure!(probe.payload_queries.load(Ordering::SeqCst) == 0);
    let probe = Probe {
        mutation: Some(sqlite(
            "UPDATE tenancy_receipt_archive SET payload=zeroblob(?)",
            [i64::try_from(prepared.stored_bytes() + 1)?.into()],
        )),
        ..probe
    };
    ensure!(
        load(
            &probe,
            &r.installation_id,
            &r.tenant_id,
            &r.execution_id,
            NO_CHARGE
        )
        .await
        .is_err()
    );
    ensure!(
        probe.payload_queries.load(Ordering::SeqCst) == 1
            && probe.transferred.load(Ordering::SeqCst) == 0
    );
    db.close().await?;
    Ok(())
}
