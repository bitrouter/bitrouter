//! These tests execute migration DDL, not only generated SQL comparisons.
//!
//! The ignored server case requires BITROUTER_TEST_MIGRATION_DATABASE_URL naming
//! a dedicated PostgreSQL/MySQL database with NO existing eval tables. It refuses
//! existing tables, creates the real 010/011 predecessors, and drops only the four
//! tables it created. It does not reuse the populated database_backends fixture.

use super::*;
use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use std::time::Duration;

mod catalog;
mod fixture;
mod scenarios;

#[tokio::test]
async fn sqlite_migration030_preserves_legacy_resumes_and_refuses_owned_down() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("eval-migration030.sqlite");
    exercise_bounded(&format!("sqlite://{}?mode=rwc", path.display())).await?;
    ensure!(
        path.is_file(),
        "migration test must reopen a real SQLite file"
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_migration030_preserves_existing_migration_ledger() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("eval-ledger.sqlite");
    let db = connect(&format!("sqlite://{}?mode=rwc", path.display())).await?;
    let all = crate::db::migration::Migrator::migrations();
    let previous_count = all
        .iter()
        .position(|entry| entry.name() == Migration.name())
        .context("parent must register migration030")?;
    crate::db::migration::Migrator::up(&db, Some(previous_count.try_into()?)).await?;
    let before = crate::db::migration::Migrator::get_migration_models(&db).await?;
    fixture::seed(&db).await?;
    let bytes = fixture::historical(&db).await?;
    crate::db::migration::Migrator::up(&db, None).await?;
    let after = crate::db::migration::Migrator::get_migration_models(&db).await?;
    ensure!(
        after.starts_with(&before),
        "historical migration versions/timestamps changed"
    );
    ensure!(
        after
            .iter()
            .map(|row| row.version.as_str())
            .eq(all.iter().map(|entry| entry.name()))
    );
    crate::db::migration::Migrator::up(&db, None).await?;
    ensure!(crate::db::migration::Migrator::get_migration_models(&db).await? == after);
    fixture::assert_historical(&db, &bytes).await?;
    fixture::assert_all_null(&db).await?;
    db.close().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_MIGRATION_DATABASE_URL for a dedicated database without eval tables"]
async fn server_migration030_preserves_legacy_resumes_and_refuses_owned_down() -> Result<()> {
    let address = std::env::var("BITROUTER_TEST_MIGRATION_DATABASE_URL")
        .context("set BITROUTER_TEST_MIGRATION_DATABASE_URL; no populated-database fallback")?;
    let parsed = url::Url::parse(&address).context("invalid migration test database URL")?;
    match parsed.scheme() {
        "postgres" | "postgresql" => ensure!(cfg!(feature = "db-postgres"), "enable db-postgres"),
        "mysql" => ensure!(cfg!(feature = "db-mysql"), "enable db-mysql"),
        _ => anyhow::bail!("migration test URL must select PostgreSQL or MySQL"),
    }
    ensure!(
        !parsed.path().trim_matches('/').is_empty(),
        "migration test URL must name a dedicated database"
    );
    exercise_bounded(&address).await
}

async fn connect(address: &str) -> Result<DatabaseConnection> {
    let mut options = ConnectOptions::new(address);
    options
        .min_connections(1)
        .max_connections(1)
        .sqlx_logging(false);
    Database::connect(options)
        .await
        .map_err(|_| anyhow::anyhow!("could not connect to migration test database"))
}

async fn exercise_bounded(address: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(120), exercise(address))
        .await
        .context("migration030 scenarios exceeded two minutes")?
}

async fn exercise(address: &str) -> Result<()> {
    let db = connect(address).await?;
    let manager = SchemaManager::new(&db);
    // Refusal happens before any creation or cleanup. Never repurpose an
    // already migrated backend integration database for destructive down tests.
    for table in TABLES {
        ensure!(
            !manager.has_table(table).await?,
            "dedicated database already contains {table}"
        );
    }
    let prepared = async {
        crate::db::migration::m20240101_000010_create_eval_exchange::Migration
            .up(&manager)
            .await?;
        crate::db::migration::m20240101_000011_scope_eval_exchange::Migration
            .up(&manager)
            .await?;
        fixture::seed(&db).await?;
        let historical = fixture::historical(&db).await?;
        scenarios::incompatible_columns(&db, &historical).await?;
        scenarios::unexpected_unique_constraint(&db, &historical).await?;
        catalog::unique_expression_dependencies(&db, &historical).await?;
        catalog::postgres_referencing_foreign_key(&db, &historical).await?;
        catalog::postgres_index_options(&db, &historical).await?;
        scenarios::index_name_collision(&db, &historical).await?;
        scenarios::mysql_oversize_parent(&db, &historical).await?;
        scenarios::partial_up(&db, &historical).await?;
        scenarios::incompatible_indexes(&db, &historical).await?;
        scenarios::exact_comparison(&db).await?;
        scenarios::down_refuses_every_ownership_field(&db, &historical).await?;
        // Persist one malformed partial row through pool teardown. Neither up
        // nor restart may backfill its missing fields or erase its provenance.
        fixture::set_metadata(&db, "eval_snapshots", [None, None, Some("creator-restart")]).await?;
        fixture::assert_historical(&db, &historical).await?;
        Ok::<_, anyhow::Error>(historical)
    }
    .await;
    let historical = match prepared {
        Ok(historical) => historical,
        Err(error) => {
            let cleaned = fixture::cleanup(&db).await;
            let closed = db.close().await;
            cleaned?;
            closed?;
            return Err(error);
        }
    };
    db.close().await?;

    let db = connect(address).await?;
    let outcome = async {
        let manager = SchemaManager::new(&db);
        fixture::assert_historical(&db, &historical).await?;
        Migration.up(&manager).await?;
        ensure!(
            fixture::metadata_count(
                &db,
                "eval_snapshots",
                "creator_principal_id",
                "creator-restart"
            )
            .await?
                == 2
        );
        ensure!(
            Migration.down(&manager).await.is_err(),
            "restart lost partial ownership"
        );
        fixture::assert_complete(&manager).await?;
        fixture::set_metadata(&db, "eval_snapshots", [None, None, None]).await?;
        scenarios::sqlite_query_plans(&db).await?;
        // Resume a partially committed downgrade as well as a complete rerun.
        manager.drop_index(INDEXES[3].drop()).await?;
        fixture::drop_column(&manager, "eval_snapshots", "creator_principal_id").await?;
        Migration.down(&manager).await?;
        fixture::assert_absent(&manager).await?;
        fixture::assert_historical(&db, &historical).await?;
        fixture::assert_legacy_indexes(&manager).await?;
        // A partially completed down/up must also be restartable. No historical
        // table, primary key, owner column or legacy index is migration030's own.
        Migration.down(&manager).await?;
        Migration.up(&manager).await?;
        fixture::assert_complete(&manager).await?;
        fixture::assert_all_null(&db).await?;
        fixture::assert_historical(&db, &historical).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleaned = fixture::cleanup(&db).await;
    let closed = db.close().await;
    outcome?;
    cleaned?;
    closed?;
    Ok(())
}

#[test]
fn sqlite_definition_parser_does_not_confuse_literals_or_constraints() -> Result<()> {
    let ddl = "CREATE TABLE \"fixture\" (\"legacy\" TEXT DEFAULT 'tenant_id,(' /* ) */, \
               [tenant_id] VARCHAR(128) COLLATE BINARY, \
               CHECK ([tenant_id] IS NULL OR length([tenant_id]) <= 128))";
    let declarations = sqlite_declarations(ddl)?;
    ensure!(declarations.len() == 3);
    let expected = sqlite_tokens("tenant_id varchar(128) collate binary")?;
    ensure!(declarations.get(1) == Some(&expected));
    ensure!(sqlite_declarations("CREATE TABLE f (\"unterminated").is_err());
    ensure!(sqlite_declarations("CREATE TABLE f (/* unterminated").is_err());
    Ok(())
}
