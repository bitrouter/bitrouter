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
    // Leave a genuinely empty catalog with a large WAL-visible freelist. The
    // current baseline remains pending without constructing an older schema.
    owned
        .db
        .execute_unprepared("DROP TABLE preflight_payload")
        .await?;
    ensure!(store_format::inspect(&owned.db).await? == store_format::State::Empty);
    let row = owned.db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT page_count AS pages,page_size AS size FROM pragma_page_count(),pragma_page_size()"))
        .await?.context("page inventory")?;
    let image = u64::try_from(row.try_get::<i64>("", "pages")?)?
        * u64::try_from(row.try_get::<i64>("", "size")?)?;
    ensure!(
        image > std::fs::metadata(&owned.owner.path)?.len(),
        "fixture must retain uncheckpointed pages"
    );
    let seen = Cell::new(0);
    let calls = Cell::new(0);
    ensure!(
        owned
            .migrate_with(|encoded| {
                calls.set(calls.get() + 1);
                seen.set(encoded);
                anyhow::bail!("injected insufficient free space")
            })
            .await
            .is_err()
    );
    ensure!(calls.get() == 1);
    ensure!(seen.get() == image + 1024 * 1024);
    ensure!(!has_ledger(&owned.db).await?);
    ensure!(store_format::inspect(&owned.db).await? == store_format::State::Empty);
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
