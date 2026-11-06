use super::*;
use anyhow::{Context, ensure};
use futures::FutureExt;
use sea_orm::ConnectionTrait;

async fn fixture() -> Result<(
    DatabaseConnection,
    crate::tenancy::Service,
    Arc<OnceLock<Budget>>,
)> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    db.execute_unprepared("CREATE TABLE observability_custody (value INTEGER NOT NULL)")
        .await?;
    let service = crate::tenancy::Service::open(db.clone(), None).await?;
    let cell = Arc::new(OnceLock::new());
    cell.set(service.storage_budget())
        .map_err(|_| anyhow::anyhow!("budget already initialized"))?;
    Ok((db, service, cell))
}

fn healthy(budget: &Budget) -> Result<()> {
    budget
        .reserve(0, false)
        .map_err(capacity_error)?
        .complete()
        .map_err(capacity_error)?;
    Ok(())
}

async fn count(db: &DatabaseConnection) -> Result<i64> {
    db.query_one(sea_orm::Statement::from_string(
        sea_orm::DatabaseBackend::Sqlite,
        "SELECT COUNT(*) AS count FROM observability_custody",
    ))
    .await?
    .context("count missing")?
    .try_get("", "count")
    .map_err(Into::into)
}

#[tokio::test]
async fn acknowledged_business_rollback_refunds_without_storage_fence() -> Result<()> {
    let (db, service, cell) = fixture().await?;
    let (write, tx) = Write::begin(&db, &Some(cell), false).await?;
    tx.execute_unprepared("INSERT INTO observability_custody VALUES (1)")
        .await?;
    let rejected: Result<()> = Err(anyhow::anyhow!("attempt identity changed"));
    ensure!(write.finish(tx, rejected).await.is_err());
    ensure!(count(&db).await? == 0);
    healthy(&service.storage_budget())?;
    service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn sql_failure_stays_fenced_after_acknowledged_rollback() -> Result<()> {
    let (db, service, cell) = fixture().await?;
    let (write, tx) = Write::begin(&db, &Some(cell), false).await?;
    tx.execute_unprepared("INSERT INTO observability_custody VALUES (1)")
        .await?;
    let result: Result<()> = tx
        .execute_unprepared("INSERT INTO missing_private_table VALUES (2)")
        .await
        .map(|_| ())
        .map_err(Into::into);
    let error = write
        .finish(tx, result)
        .await
        .err()
        .context("SQL error was accepted")?;
    ensure!(error.to_string() == "internal error: local storage SQL failed");
    ensure!(count(&db).await? == 0);
    ensure!(matches!(
        service.storage_budget().reserve(0, false),
        Err(ErrorCode::StorageUnavailable)
    ));
    // SQL rollback was acknowledged even though storage health remains fenced.
    service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn dropping_a_pending_begin_fences_without_releasing_custody() -> Result<()> {
    let (db, service, cell) = fixture().await?;
    let held = db.begin().await?;
    let shared = Some(cell);
    let mut writing = Box::pin(Write::begin(&db, &shared, false));
    ensure!(writing.as_mut().now_or_never().is_none());
    drop(writing);
    held.rollback().await?;
    ensure!(matches!(
        service.storage_budget().reserve(0, false),
        Err(ErrorCode::StorageUnavailable)
    ));
    ensure!(service.shutdown().await.is_err());
    Ok(())
}

#[tokio::test]
async fn encoded_program_limit_rolls_back_prior_mutations_without_fencing() -> Result<()> {
    let (db, service, cell) = fixture().await?;
    let (mut write, tx) = Write::begin(&db, &Some(cell), false).await?;
    tx.execute_unprepared("INSERT INTO observability_custody VALUES (1)")
        .await?;
    // An old row can exceed the remaining program allowance even if its new
    // payload shrinks. Count actual encoded data, not the replacement alone.
    let old = records::Model {
        record_id: "row".into(),
        scope_id: "scope".into(),
        kind: "kind".into(),
        session_key: None,
        revision: 1,
        body: "x".repeat(MAX_BYTES),
    };
    let result = write.replacement(&old, &"small");
    let error = write
        .finish(tx, result)
        .await
        .err()
        .context("oversize row accepted")?;
    ensure!(matches!(
        error.downcast_ref::<BitrouterError>(),
        Some(BitrouterError::RateLimited { .. })
    ));
    ensure!(count(&db).await? == 0);
    healthy(&service.storage_budget())?;
    service.shutdown().await?;
    Ok(())
}
