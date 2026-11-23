//! Receipt restart guarantees and unsupported-ledger refusal.
use super::*;
use sea_orm_migration::MigratorTrait;

#[tokio::test]
async fn current_baseline_preserves_receipt_usage_and_identity_across_two_restarts() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let address = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("current.sqlite").display()
    );
    let db = crate::db::connect(&address).await?;
    crate::db::run_migrations(&db).await?;
    let original = candidate("installation", "tenant", "execution", "principal")?;
    ensure!(commit(&db, &original).await?);
    let ledger = crate::db::migration::Migrator::get_migration_models(&db).await?;
    db.close().await?;
    for _ in 0..2 {
        let db = crate::db::connect(&address).await?;
        crate::db::run_migrations(&db).await?;
        let saved = lookup_candidate(
            &db,
            &original.receipt.installation_id,
            &original.receipt.tenant_id,
            &original.receipt.execution_id,
        )
        .await?
        .context("current receipt missing")?;
        ensure!(
            saved.json == original.json
                && saved.private_json == original.private_json
                && saved.digest == original.digest
        );
        let actual = &saved.usage_evidence;
        let expected = &original.usage_evidence;
        ensure!(actual.json == expected.json && actual.digest == expected.digest);
        ensure!(crate::db::migration::Migrator::get_migration_models(&db).await? == ledger);
        ensure!(
            !commit(&db, &original).await?,
            "restart duplicated the terminal receipt"
        );
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn unsupported_writer_ledgers_are_refused_without_rewriting_evidence() -> Result<()> {
    for marker in [
        "m20240101_000045_ordered_receipts",
        "m20240101_000047_shared_admission_health",
        "m20240101_000048_native_generate_content",
    ] {
        let db = crate::db::connect("sqlite::memory:").await?;
        db.execute_unprepared("CREATE TABLE seaql_migrations(version TEXT PRIMARY KEY,applied_at BIGINT NOT NULL); CREATE TABLE preserved(value TEXT); INSERT INTO preserved VALUES ('immutable-receipt')").await?;
        db.execute(sea_orm::Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO seaql_migrations VALUES (?,1)",
            [marker.into()],
        ))
        .await?;
        ensure!(crate::db::run_migrations(&db).await.is_err());
        let row = db
            .query_one(sea_orm::Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT value FROM preserved",
            ))
            .await?
            .context("preserved evidence")?;
        ensure!(row.try_get::<String>("", "value")? == "immutable-receipt");
        let row = db
            .query_one(sea_orm::Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT version FROM seaql_migrations",
            ))
            .await?
            .context("old marker")?;
        ensure!(row.try_get::<String>("", "version")? == marker);
        db.close().await?;
    }
    Ok(())
}
