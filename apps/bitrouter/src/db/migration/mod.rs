//! Schema migrations, as `sea-orm-migration` Rust code.
//!
//! Each migration is a `MigrationTrait` impl that builds tables and
//! indexes through sea-orm's portable schema API — no hand-written SQL —
//! so the identical schema applies on SQLite, Postgres and MySQL alike.
//!
//! Migration ordering is the order of the [`Migrator::migrations`] vec;
//! applied migrations are recorded in the `seaql_migrations` table so a
//! re-run is a no-op.

pub mod m20240101_000001_create_auth_tables;
pub mod m20240101_000002_create_metering_tables;
pub mod m20240101_000003_rename_legacy_charge_column;
pub mod m20240101_000004_create_adequacy_table;
pub mod m20240101_000005_create_adequacy_exploration_table;
pub mod m20240101_000006_create_adequacy_semantic_success_table;
pub mod m20240101_000007_add_metering_evidence;
pub mod m20240101_000008_add_metering_reconciliation;
pub mod m20240101_000009_create_adequacy_reliability_events;
pub mod m20240101_000010_create_eval_exchange;
pub mod m20240101_000011_scope_eval_exchange;
pub mod m20240101_000012_create_trajectory_ledger;

use sea_orm_migration::{MigrationTrait, MigratorTrait};

/// The bitrouter schema migrator — owns the ordered list of every
/// migration the binary ships.
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20240101_000001_create_auth_tables::Migration),
            Box::new(m20240101_000002_create_metering_tables::Migration),
            Box::new(m20240101_000003_rename_legacy_charge_column::Migration),
            Box::new(m20240101_000004_create_adequacy_table::Migration),
            Box::new(m20240101_000005_create_adequacy_exploration_table::Migration),
            Box::new(m20240101_000006_create_adequacy_semantic_success_table::Migration),
            Box::new(m20240101_000007_add_metering_evidence::Migration),
            Box::new(m20240101_000008_add_metering_reconciliation::Migration),
            Box::new(m20240101_000009_create_adequacy_reliability_events::Migration),
            Box::new(m20240101_000010_create_eval_exchange::Migration),
            Box::new(m20240101_000011_scope_eval_exchange::Migration),
            Box::new(m20240101_000012_create_trajectory_ledger::Migration),
        ]
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    use sea_orm_migration::{MigrationTrait, SchemaManager};

    use super::m20240101_000012_create_trajectory_ledger::Migration;

    #[tokio::test]
    async fn trajectory_ledger_migration_creates_and_removes_only_its_objects() -> anyhow::Result<()>
    {
        let db = crate::db::connect("sqlite::memory:").await?;
        let manager = SchemaManager::new(&db);
        let migration = Migration;

        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "CREATE TABLE migration_test_sentinel (id TEXT PRIMARY KEY)".to_owned(),
        ))
        .await?;

        migration.up(&manager).await?;
        migration.up(&manager).await?;

        for table in [
            "trajectory_episodes",
            "trajectory_events",
            "trajectory_requests",
            "trajectory_outbox",
        ] {
            assert!(manager.has_table(table).await?);
        }
        for index in [
            "idx_trajectory_episodes_owner_correlation",
            "idx_trajectory_events_episode_sequence",
            "idx_trajectory_requests_owner_episode",
            "idx_trajectory_outbox_pending",
        ] {
            let rows = db
                .query_all(Statement::from_string(
                    DatabaseBackend::Sqlite,
                    format!(
                        "SELECT name FROM sqlite_master WHERE type = 'index' AND name = '{index}'"
                    ),
                ))
                .await?;
            assert_eq!(rows.len(), 1, "missing index {index}");
        }

        migration.down(&manager).await?;
        for table in [
            "trajectory_episodes",
            "trajectory_events",
            "trajectory_requests",
            "trajectory_outbox",
        ] {
            assert!(!manager.has_table(table).await?);
        }
        assert!(manager.has_table("migration_test_sentinel").await?);
        Ok(())
    }
}
