//! Fresh baseline failures preserve both the catalog and its sole format marker.

use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use super::{migration::Migrator, run_migrations, store_format};

type Catalog = Vec<(String, String, String, Option<String>)>;

async fn catalog(db: &DatabaseConnection) -> Result<Catalog> {
    db.query_all(Statement::from_string(
        DatabaseBackend::Sqlite,
        "SELECT type,name,tbl_name,sql FROM main.sqlite_schema ORDER BY type,name",
    ))
    .await?
    .into_iter()
    .map(|row| {
        Ok((
            row.try_get("", "type")?,
            row.try_get("", "name")?,
            row.try_get("", "tbl_name")?,
            row.try_get("", "sql")?,
        ))
    })
    .collect()
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
    for fail_during_insert in [true, false] {
        let db = super::connect("sqlite::memory:").await?;
        // A temp-only sentinel proves that rollback kept the original physical
        // connection; losing this in-memory connection cannot fake atomicity.
        db.execute_unprepared("CREATE TEMP TABLE atomicity_connection(value TEXT NOT NULL); INSERT INTO atomicity_connection VALUES ('original')").await?;
        let before = catalog(&db).await?;
        ensure!(before.is_empty());
        let marker_seen = Arc::new(AtomicBool::new(false));
        let interrupt = Arc::new(AtomicBool::new(false));
        let rejected = Arc::new(AtomicBool::new(false));
        let mut connection = db.get_sqlite_connection_pool().acquire().await?;
        {
            let seen = marker_seen.clone();
            let interrupt = interrupt.clone();
            connection
                .lock_handle()
                .await?
                .set_update_hook(move |event| {
                    if event.database == "main" && event.table == "seaql_migrations" {
                        seen.store(true, Ordering::Release);
                        interrupt.store(fail_during_insert, Ordering::Release);
                    }
                });
        }
        if fail_during_insert {
            let rejected = rejected.clone();
            connection
                .lock_handle()
                .await?
                .set_progress_handler(1, move || {
                    if interrupt.swap(false, Ordering::AcqRel) {
                        rejected.store(true, Ordering::Release);
                        false
                    } else {
                        true
                    }
                });
        } else {
            let seen = marker_seen.clone();
            let rejected = rejected.clone();
            connection.lock_handle().await?.set_commit_hook(move || {
                if seen.load(Ordering::Acquire) {
                    rejected.store(true, Ordering::Release);
                    false
                } else {
                    true
                }
            });
        }
        drop(connection);
        // The original initializer owns DDL, marker publication and COMMIT.
        // Safe SQLx hooks only reject its original SQLite operation; they never
        // issue an alternate statement or manually implement a migration.
        let failed = run_migrations(&db).await;
        let mut connection = db.get_sqlite_connection_pool().acquire().await?;
        {
            let mut handle = connection.lock_handle().await?;
            handle.remove_progress_handler();
            handle.remove_update_hook();
            handle.remove_commit_hook();
        }
        drop(connection);
        ensure!(failed.is_err());
        ensure!(marker_seen.load(Ordering::Acquire));
        ensure!(rejected.load(Ordering::Acquire));
        ensure!(catalog(&db).await? == before);
        ensure!(store_format::inspect(&db).await? == store_format::State::Empty);
        let sentinel = db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT value FROM temp.atomicity_connection",
            ))
            .await?
            .context("physical connection replaced after failure")?;
        ensure!(sentinel.try_get::<String>("", "value")? == "original");
        run_migrations(&db).await?;
        ensure!(table_exists(&db, "tenancy_resource_lifecycle").await?);
        ensure!(table_exists(&db, "tenancy_usage_evidence").await?);
        let applied = Migrator::get_migration_models(&db).await?;
        ensure!(applied.len() == 1 && applied[0].version == super::store_format::GATEWAY_SQLITE);
        run_migrations(&db).await?;
        ensure!(Migrator::get_migration_models(&db).await? == applied);
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn validation_failure_rolls_back_created_tables_without_adopting_shadow() -> Result<()> {
    let db = super::connect("sqlite::memory:").await?;
    let before = catalog(&db).await?;
    db.execute_unprepared("CREATE TEMP TABLE tenancy_usage_evidence(untrusted BLOB); INSERT INTO temp.tenancy_usage_evidence VALUES (X'1234')").await?;
    ensure!(run_migrations(&db).await.is_err());
    ensure!(catalog(&db).await? == before);
    ensure!(!table_exists(&db, "seaql_migrations").await?);
    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT hex(untrusted) AS original FROM temp.tenancy_usage_evidence",
        ))
        .await?
        .context("original shadow disappeared")?;
    ensure!(row.try_get::<String>("", "original")? == "1234");
    db.execute_unprepared("DROP TABLE temp.tenancy_usage_evidence")
        .await?;
    run_migrations(&db).await?;
    ensure!(store_format::inspect(&db).await? == store_format::State::Current);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn unknown_existing_table_is_preserved_and_never_adopted() -> Result<()> {
    for forged_current_marker in [false, true] {
        let db = super::connect("sqlite::memory:").await?;
        db.execute_unprepared("CREATE TABLE tenancy_usage_evidence(untrusted BLOB); INSERT INTO tenancy_usage_evidence VALUES (X'1234')").await?;
        if forged_current_marker {
            db.execute_unprepared("CREATE TABLE seaql_migrations(version TEXT PRIMARY KEY,applied_at BIGINT NOT NULL)").await?;
            db.execute(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "INSERT INTO seaql_migrations VALUES (?,123)",
                [super::store_format::GATEWAY_SQLITE.into()],
            ))
            .await?;
        }
        let before = catalog(&db).await?;
        for _ in 0..2 {
            ensure!(run_migrations(&db).await.is_err());
            ensure!(catalog(&db).await? == before);
            ensure!(!table_exists(&db, "tenancy_resource_lifecycle").await?);
            let row = db
                .query_one(Statement::from_string(
                    DatabaseBackend::Sqlite,
                    "SELECT hex(untrusted) AS original FROM tenancy_usage_evidence",
                ))
                .await?
                .context("original unmanaged row")?;
            ensure!(row.try_get::<String>("", "original")? == "1234");
            if forged_current_marker {
                let row = db
                    .query_one(Statement::from_string(
                        DatabaseBackend::Sqlite,
                        "SELECT version,applied_at FROM seaql_migrations",
                    ))
                    .await?
                    .context("original marker disappeared")?;
                ensure!(
                    row.try_get::<String>("", "version")? == super::store_format::GATEWAY_SQLITE
                );
                ensure!(row.try_get::<i64>("", "applied_at")? == 123);
            } else {
                ensure!(!table_exists(&db, "seaql_migrations").await?);
            }
        }
        db.close().await?;
    }
    Ok(())
}
