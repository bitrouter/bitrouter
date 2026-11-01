//! Startup migration failures leave both schema and ledger at the prior version.

use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;

use super::{migration::Migrator, run_migrations};

const LIFECYCLE: &str = "m20240101_000039_resource_lifecycle";
const EVIDENCE: &str = "m20240101_000040_usage_evidence";

async fn previous_database() -> Result<DatabaseConnection> {
    let db = super::connect("sqlite::memory:").await?;
    let previous = Migrator::migrations()
        .iter()
        .position(|migration| migration.name() == LIFECYCLE)
        .context("resource lifecycle migration registration")?;
    Migrator::up(&db, Some(previous.try_into()?)).await?;
    Ok(db)
}

async fn table_exists(db: &DatabaseConnection, name: &str) -> Result<bool> {
    Ok(db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT 1 FROM main.sqlite_schema WHERE type='table' AND name=?",
            [name.into()],
        ))
        .await?
        .is_some())
}

#[tokio::test]
async fn failed_ledger_insert_rolls_back_ddl_and_retry_succeeds() -> Result<()> {
    for failed_version in [LIFECYCLE, EVIDENCE] {
        let db = previous_database().await?;
        let before = Migrator::get_migration_models(&db).await?;
        db.execute_unprepared(&format!(
            "CREATE TRIGGER reject_migration BEFORE INSERT ON seaql_migrations \
             WHEN NEW.version='{failed_version}' BEGIN SELECT RAISE(ABORT,'injected ledger failure'); END"
        ))
        .await?;
        ensure!(run_migrations(&db).await.is_err());
        ensure!(Migrator::get_migration_models(&db).await? == before);
        ensure!(!table_exists(&db, "tenancy_resource_lifecycle").await?);
        ensure!(!table_exists(&db, "tenancy_usage_evidence").await?);
        db.execute_unprepared("DROP TRIGGER reject_migration")
            .await?;
        run_migrations(&db).await?;
        ensure!(table_exists(&db, "tenancy_resource_lifecycle").await?);
        ensure!(table_exists(&db, "tenancy_usage_evidence").await?);
        let applied = Migrator::get_migration_models(&db).await?;
        ensure!(applied.len() == Migrator::migrations().len());
        run_migrations(&db).await?;
        ensure!(Migrator::get_migration_models(&db).await? == applied);
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn validation_failure_rolls_back_created_tables_without_adopting_shadow() -> Result<()> {
    let db = previous_database().await?;
    let before = Migrator::get_migration_models(&db).await?;
    // The one-connection fixture retains the temporary object. Validation must
    // reject it, preserving it while rolling back any newly created main table.
    db.execute_unprepared("CREATE TEMP TABLE tenancy_usage_evidence(untrusted BLOB)")
        .await?;
    ensure!(run_migrations(&db).await.is_err());
    ensure!(Migrator::get_migration_models(&db).await? == before);
    ensure!(!table_exists(&db, "tenancy_resource_lifecycle").await?);
    ensure!(!table_exists(&db, "tenancy_usage_evidence").await?);
    db.execute_unprepared("INSERT INTO temp.tenancy_usage_evidence VALUES (X'1234')")
        .await?;
    db.execute_unprepared("DROP TABLE temp.tenancy_usage_evidence")
        .await?;
    run_migrations(&db).await?;
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn unknown_existing_table_is_preserved_and_never_adopted() -> Result<()> {
    let db = previous_database().await?;
    let before = Migrator::get_migration_models(&db).await?;
    db.execute_unprepared("CREATE TABLE tenancy_usage_evidence(untrusted BLOB)")
        .await?;
    db.execute_unprepared("INSERT INTO tenancy_usage_evidence VALUES (X'1234')")
        .await?;
    for _ in 0..2 {
        ensure!(run_migrations(&db).await.is_err());
        ensure!(Migrator::get_migration_models(&db).await? == before);
        ensure!(!table_exists(&db, "tenancy_resource_lifecycle").await?);
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT hex(untrusted) AS original FROM tenancy_usage_evidence",
            ))
            .await?
            .context("original unmanaged row")?;
        ensure!(row.try_get::<String>("", "original")? == "1234");
    }
    db.close().await?;
    Ok(())
}
