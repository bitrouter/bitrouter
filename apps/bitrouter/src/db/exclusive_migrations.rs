//! Exclusive startup preflight runs before DDL and skips current-schema reads.
use super::*;
use anyhow::{Context, Result, ensure};
use sea_orm::Statement;
use std::cell::Cell;

async fn fixture() -> Result<(tempfile::TempDir, OwnedDatabase)> {
    let directory = tempfile::tempdir()?;
    let database = connect_owned_tenancy(&format!(
        "sqlite://{}",
        directory.path().join("preflight.sqlite").display()
    ))
    .await?;
    Ok((directory, database))
}

async fn has_ledger(db: &DatabaseConnection) -> Result<bool> {
    Ok(db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT 1 FROM main.sqlite_schema WHERE name='seaql_migrations'",
        ))
        .await?
        .is_some())
}

#[tokio::test]
async fn new_schema_refusal_precedes_even_migration_ledger_creation() -> Result<()> {
    let (_directory, owned) = fixture().await?;
    let calls = Cell::new(0);
    ensure!(!has_ledger(&owned.db).await?);
    ensure!(
        owned
            .migrate_with(|encoded| {
                calls.set(calls.get() + 1);
                ensure!(encoded >= 1024 * 1024);
                anyhow::bail!("injected insufficient free space")
            })
            .await
            .is_err()
    );
    ensure!(calls.get() == 1 && !has_ledger(&owned.db).await?);
    owned.db.clone().close().await?;
    Ok(())
}

#[tokio::test]
async fn pending_migration_estimates_the_current_wal_visible_image_before_writing() -> Result<()> {
    let (_directory, owned) = fixture().await?;
    let previous = migration::Migrator::migrations()
        .iter()
        .position(|migration| migration.name() == "m20240101_000044_capacity_upgrade")
        .context("capacity migration")?;
    migration::Migrator::up(&owned.db, Some(u32::try_from(previous)?)).await?;
    owned
        .db
        .execute_unprepared("PRAGMA wal_autocheckpoint=0")
        .await?;
    owned
        .db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "PRAGMA wal_checkpoint(PASSIVE)",
        ))
        .await?
        .context("checkpoint result")?;
    owned
        .db
        .execute_unprepared("CREATE TABLE preflight_payload (payload BLOB NOT NULL)")
        .await?;
    owned
        .db
        .execute_unprepared("INSERT INTO preflight_payload VALUES (zeroblob(1048576))")
        .await?;
    let row = owned.db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT page_count AS pages,page_size AS size FROM pragma_page_count(),pragma_page_size()"))
        .await?.context("page inventory")?;
    let image = u64::try_from(row.try_get::<i64>("", "pages")?)?
        * u64::try_from(row.try_get::<i64>("", "size")?)?;
    ensure!(
        image > std::fs::metadata(&owned.owner.path)?.len(),
        "fixture must retain uncheckpointed pages"
    );
    let before = migration::Migrator::get_migration_models(&owned.db).await?;
    let seen = Cell::new(0);
    ensure!(
        owned
            .migrate_with(|encoded| {
                seen.set(encoded);
                anyhow::bail!("injected insufficient free space")
            })
            .await
            .is_err()
    );
    ensure!(seen.get() == image + 1024 * 1024);
    ensure!(migration::Migrator::get_migration_models(&owned.db).await? == before);
    ensure!(
        owned
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT 1 FROM main.sqlite_schema WHERE name='tenancy_capacity_upgrades'"
            ))
            .await?
            .is_none()
    );
    owned.db.clone().close().await?;
    Ok(())
}

#[tokio::test]
async fn current_migrations_need_neither_write_access_nor_free_space_probe() -> Result<()> {
    let (_directory, owned) = fixture().await?;
    owned.migrate_with(|_| Ok(())).await?;
    owned.db.execute_unprepared("PRAGMA query_only=ON").await?;
    let calls = Cell::new(0);
    owned
        .migrate_with(|_| {
            calls.set(calls.get() + 1);
            anyhow::bail!("injected insufficient free space")
        })
        .await?;
    ensure!(calls.get() == 0);
    owned.db.execute_unprepared("PRAGMA query_only=OFF").await?;
    owned
        .db
        .execute_unprepared(
            "CREATE TEMP TABLE seaql_migrations AS SELECT * FROM main.seaql_migrations",
        )
        .await?;
    ensure!(
        owned
            .migrate_with(|_| {
                calls.set(calls.get() + 1);
                Ok(())
            })
            .await
            .is_err()
    );
    ensure!(calls.get() == 0, "temporary ledger must not be adopted");
    owned.db.clone().close().await?;
    Ok(())
}
