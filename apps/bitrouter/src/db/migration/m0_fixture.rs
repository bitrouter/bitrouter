//! Recreate the pre-split M0 schema with the original component DDL. Test-only.
use crate::db::store_format;
use anyhow::{Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};
use sea_orm_migration::{MigratorTrait, SchemaManager};

pub(crate) async fn install(db: &DatabaseConnection) -> Result<()> {
    ensure!(
        store_format::inspect(db).await? == store_format::State::Empty,
        "M0 fixture needs an empty database"
    );
    let manager = &SchemaManager::new(db);
    super::baseline::service_tables(manager).await?;
    super::m20240101_000004_create_adequacy_table::initialize_current(manager).await?;
    super::m20240101_000005_create_adequacy_exploration_table::initialize_current(manager).await?;
    super::m20240101_000006_create_adequacy_semantic_success_table::initialize_current(manager)
        .await?;
    super::m20240101_000009_create_adequacy_reliability_events::initialize_current(manager).await?;
    super::m20240101_000018_create_acp_capture::initialize_current(manager).await?;
    super::m20240101_000019_create_acp_checkpoints::initialize_current(manager).await?;
    super::m20240101_000022_create_bro_execution_records::initialize_current(manager).await?;
    super::m20240101_000023_create_bro_acceptance_keys::initialize_current(manager).await?;
    super::m20240101_000024_create_bro_runtime_ownership::initialize_current(manager).await?;
    super::m20240101_000025_create_bro_execution_index::initialize_current(manager).await?;
    super::m20240101_000026_bro_runtime_format::initialize_current(manager).await?;
    super::m20240101_000027_bro_managed_sessions::initialize_current(manager).await?;
    super::m20240101_000033_capture_legacy_scope::initialize_current(manager).await?;
    super::m20240101_000036_native_harness_scope::initialize_current(manager).await?;
    super::Migrator::install(db).await?;
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        if db.get_database_backend() == DbBackend::Postgres {
            "INSERT INTO seaql_migrations(version,applied_at) VALUES($1,$2)"
        } else {
            "INSERT INTO seaql_migrations(version,applied_at) VALUES(?,?)"
        },
        [store_format::BASELINE.into(), 1_i64.into()],
    ))
    .await?;
    Ok(())
}
