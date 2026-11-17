//! Exclusive startup preflight runs before DDL and skips current-schema reads.
use super::*;
use anyhow::{Context, Result, ensure};
use sea_orm::Statement;
use sea_orm::sqlx::{self, Connection as _, Row as _};
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
    {
        let mut connection = owned.db.get_sqlite_connection_pool().acquire().await?;
        // Finalize connector catalog statements before checkpointing. Keep the
        // checkpoint uncached and consume it through SQLITE_DONE before DDL;
        // an active SQLite cursor can otherwise lock this same connection.
        connection.clear_cached_statements().await?;
        sqlx::raw_sql("PRAGMA wal_autocheckpoint=0")
            .execute(&mut *connection)
            .await?;
        let checkpoint = sqlx::raw_sql("PRAGMA wal_checkpoint(PASSIVE)")
            .fetch_all(&mut *connection)
            .await
            .context("draining checkpoint before WAL fixture writes")?;
        ensure!(checkpoint.len() == 1 && checkpoint[0].try_get::<i64, _>("busy")? == 0);
        drop(checkpoint);
        // Leave a genuinely empty catalog with a large WAL-visible freelist.
        // No checkpoint follows these writes: the current baseline is pending
        // without constructing or adopting an older schema.
        for sql in [
            "CREATE TABLE preflight_payload (payload BLOB NOT NULL)",
            "INSERT INTO preflight_payload VALUES (zeroblob(1048576))",
            "DROP TABLE preflight_payload",
        ] {
            sqlx::raw_sql(sql)
                .execute(&mut *connection)
                .await
                .with_context(|| format!("building WAL fixture: {sql}"))?;
        }
    }
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
