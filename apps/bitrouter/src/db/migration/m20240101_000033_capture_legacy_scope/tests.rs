//! Real predecessor DDL and byte-preserving namespace isolation on all backends.
mod sqlite_dependencies;
use super::*;
use anyhow::{Context, Result, ensure};
use sea_orm::{DatabaseConnection, EntityTrait};

mod fixture;
#[cfg(any(feature = "db-postgres", feature = "db-mysql"))]
mod server;

async fn predecessors(db: &DatabaseConnection) -> Result<()> {
    let manager = SchemaManager::new(db);
    for table in TABLES {
        ensure!(
            !manager.has_table(table).await?,
            "fixture requires absent {table}"
        );
    }
    super::super::m20240101_000018_create_acp_capture::Migration
        .up(&manager)
        .await?;
    super::super::m20240101_000019_create_acp_checkpoints::Migration
        .up(&manager)
        .await?;
    super::super::m20240101_000020_create_checkpoint_evolution::Migration
        .up(&manager)
        .await?;
    Ok(())
}

async fn round_trip(db: &DatabaseConnection) -> Result<()> {
    let manager = SchemaManager::new(db);
    predecessors(db).await?;
    fixture::seed(db).await?;
    let before = fixture::historical(db).await?;
    // A bad definition in the final table must prevent additions to earlier tables.
    fixture::add(
        &manager,
        TABLES[8],
        ColumnDef::new(Alias::new("creator_principal_id"))
            .text()
            .to_owned(),
    )
    .await?;
    ensure!(Migration.up(&manager).await.is_err());
    ensure!(!manager.has_column(TABLES[0], COLUMNS[0]).await?);
    ensure!(fixture::historical(db).await? == before);
    fixture::drop(&manager, TABLES[8], "creator_principal_id").await?;
    // Resume after one real DDL step, including MySQL's implicit DDL commit.
    fixture::add(
        &manager,
        TABLES[0],
        identity(manager.get_database_backend(), COLUMNS[0], 128, true),
    )
    .await?;
    Migration.up(&manager).await?;
    Migration.up(&manager).await?;
    fixture::complete(&manager).await?;
    fixture::all_null(db).await?;
    ensure!(fixture::historical(db).await? == before);
    for table in TABLES {
        for column in COLUMNS {
            fixture::metadata(db, table, column, Some("one-marked-field")).await?;
            ensure!(
                Migration.down(&manager).await.is_err(),
                "removed {table}.{column}"
            );
            fixture::complete(&manager).await?;
            ensure!(fixture::historical(db).await? == before);
            fixture::metadata(db, table, column, None).await?;
        }
    }
    // Namespace comparison is exact, including case, normalization and spaces.
    for column in COLUMNS {
        let value = "Tenant-é ";
        fixture::metadata(db, TABLES[8], column, Some(value)).await?;
        for (probe, expected) in [
            (value, 2),
            ("tenant-é ", 0),
            ("Tenant-e\u{301} ", 0),
            ("Tenant-é", 0),
        ] {
            let query = Query::select()
                .column(Alias::new(column))
                .from(Alias::new(TABLES[8]))
                .and_where(Expr::col(Alias::new(column)).eq(probe))
                .to_owned();
            ensure!(
                db.query_all(db.get_database_backend().build(&query))
                    .await?
                    .len()
                    == expected
            );
        }
        fixture::metadata(db, TABLES[8], column, None).await?;
    }
    // A dependency in the last table must be found before ANY index/column drop.
    let extra = Index::create()
        .name("fixture_creator_dependency")
        .table(Alias::new(TABLES[8]))
        .col(Alias::new(COLUMNS[2]))
        .to_owned();
    manager.create_index(extra).await?;
    ensure!(Migration.down(&manager).await.is_err());
    fixture::complete(&manager).await?;
    manager
        .drop_index(
            Index::drop()
                .name("fixture_creator_dependency")
                .table(Alias::new(TABLES[8]))
                .to_owned(),
        )
        .await?;
    // Resume a partial downgrade without touching any predecessor bytes or indexes.
    manager.drop_index(INDEXES[8].drop()).await?;
    fixture::drop(&manager, TABLES[8], COLUMNS[2]).await?;
    Migration.down(&manager).await?;
    Migration.down(&manager).await?;
    fixture::absent(&manager).await?;
    ensure!(fixture::historical(db).await? == before);
    Migration.up(&manager).await?;
    fixture::complete(&manager).await?;
    fixture::all_null(db).await?;
    ensure!(fixture::historical(db).await? == before);
    Ok(())
}

#[tokio::test]
async fn sqlite_nine_tables_preserve_bytes_resume_and_refuse_populated_down() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("capture-legacy.sqlite");
    let address = format!("sqlite://{}?mode=rwc", path.display());
    let db = crate::db::connect(&address).await?;
    round_trip(&db).await?;
    let before = fixture::historical(&db).await?;
    fixture::metadata(&db, TABLES[8], COLUMNS[2], Some("creator-restart")).await?;
    db.close().await?;
    let db = crate::db::connect(&address).await?;
    let manager = SchemaManager::new(&db);
    Migration.up(&manager).await?;
    ensure!(Migration.down(&manager).await.is_err());
    fixture::complete(&manager).await?;
    ensure!(fixture::historical(&db).await? == before);
    fixture::metadata(&db, TABLES[8], COLUMNS[2], None).await?;
    Migration.down(&manager).await?;
    fixture::absent(&manager).await?;
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn sqlite_quoted_foreign_key_on_last_table_refuses_before_any_ddl() -> Result<()> {
    for column in ["tenant_id", "TENANT_ID"] {
        let db = crate::db::connect("sqlite::memory:").await?;
        predecessors(&db).await?;
        db.execute_unprepared("DROP TABLE acp_effective_assessments")
            .await?;
        db.execute_unprepared(&format!("CREATE TABLE acp_effective_assessments (session_key varchar(64) PRIMARY KEY, revision_id varchar(64), tenant_id varchar(128) COLLATE BINARY, FOREIGN KEY ('{column}') REFERENCES acp_canonical_sessions(session_key))")).await?;
        let manager = SchemaManager::new(&db);
        ensure!(Migration.up(&manager).await.is_err());
        ensure!(!manager.has_column(TABLES[0], COLUMNS[0]).await?);
        ensure!(!manager.has_index(INDEXES[0].table, INDEXES[0].name).await?);
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn sqlite_migration_ledger_and_public_legacy_models_stay_unchanged() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    let all = crate::db::migration::Migrator::migrations();
    let before_index = all
        .iter()
        .position(|entry| entry.name() == Migration.name())
        .context("parent must register migration033 after 031/032")?;
    crate::db::migration::Migrator::up(&db, Some(before_index.try_into()?)).await?;
    fixture::seed(&db).await?;
    let rows = crate::storage::capture::sessions::Entity::find()
        .all(&db)
        .await?;
    let before = fixture::historical(&db).await?;
    let ledger = crate::db::migration::Migrator::get_migration_models(&db).await?;
    crate::db::migration::Migrator::up(&db, None).await?;
    let after = crate::db::migration::Migrator::get_migration_models(&db).await?;
    ensure!(after.starts_with(&ledger));
    ensure!(
        after
            .iter()
            .map(|row| row.version.as_str())
            .eq(all.iter().map(|entry| entry.name()))
    );
    ensure!(fixture::historical(&db).await? == before);
    let current = crate::storage::capture::sessions::Entity::find()
        .all(&db)
        .await?;
    ensure!(serde_json::to_value(current)? == serde_json::to_value(rows)?);
    crate::db::migration::Migrator::up(&db, None).await?;
    ensure!(crate::db::migration::Migrator::get_migration_models(&db).await? == after);
    db.close().await?;
    Ok(())
}
