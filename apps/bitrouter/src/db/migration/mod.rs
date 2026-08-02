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
pub mod m20240101_000013_create_continuation_registry;

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
            Box::new(m20240101_000013_create_continuation_registry::Migration),
        ]
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    use sea_orm_migration::prelude::{MysqlQueryBuilder, PostgresQueryBuilder, SqliteQueryBuilder};
    use sea_orm_migration::{MigrationTrait, MigratorTrait, SchemaManager};

    use super::Migrator;

    use super::m20240101_000012_create_trajectory_ledger::Migration;
    use super::m20240101_000013_create_continuation_registry::{
        Migration as ContinuationMigration, provider_continuations_table,
    };

    #[tokio::test]
    async fn continuation_registry_migration_is_bounded_and_private() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        let manager = SchemaManager::new(&db);
        let migration = ContinuationMigration;

        migration.up(&manager).await?;
        assert!(manager.has_table("provider_continuations").await?);
        assert!(manager.has_table("provider_continuation_key_epoch").await?);

        let rows = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "PRAGMA table_info('provider_continuations')".to_owned(),
            ))
            .await?;
        let columns = rows
            .iter()
            .filter_map(|row| row.try_get::<String>("", "name").ok())
            .collect::<Vec<_>>();
        for required in [
            "owner_identity",
            "continuation_identity",
            "ciphertext",
            "nonce",
            "target_fingerprint",
            "key_id",
            "cipher_version",
            "created_at",
            "expires_at",
            "purge_after",
            "publication_state",
            "publication_generation",
            "publication_instance_id",
            "publication_lease_until",
        ] {
            assert!(columns.iter().any(|column| column == required));
        }
        for forbidden in ["owner_user_id", "request_id", "provider_response_id"] {
            assert!(!columns.iter().any(|column| column == forbidden));
        }
        let indexes = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT name FROM sqlite_master WHERE type = 'index' AND name = 'idx_provider_continuations_reconciliation'".to_owned(),
            ))
            .await?;
        assert_eq!(indexes.len(), 1);

        migration.down(&manager).await?;
        assert!(!manager.has_table("provider_continuations").await?);
        assert!(!manager.has_table("provider_continuation_key_epoch").await?);
        Ok(())
    }

    #[tokio::test]
    async fn continuation_registry_rejects_illegal_state_and_empty_generation() -> anyhow::Result<()>
    {
        let db = crate::db::connect("sqlite::memory:").await?;
        let manager = SchemaManager::new(&db);
        ContinuationMigration.up(&manager).await?;

        let illegal_state = db
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "INSERT INTO provider_continuations (continuation_identity, owner_identity, target_fingerprint, key_id, cipher_version, created_at, expires_at, purge_after, publication_state, publication_generation, publication_instance_id, publication_lease_until) VALUES ('c1', 'o', 't', 'k', 1, 'now', 'later', 'latest', 'forged', 'generation', 'instance', 'lease')".to_owned(),
            ))
            .await;
        assert!(
            illegal_state.is_err(),
            "the database must reject publication states outside provisional/delivering/active"
        );

        let empty_generation = db
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "INSERT INTO provider_continuations (continuation_identity, owner_identity, target_fingerprint, key_id, cipher_version, created_at, expires_at, purge_after, publication_state, publication_generation, publication_instance_id, publication_lease_until) VALUES ('c2', 'o', 't', 'k', 1, 'now', 'later', 'latest', 'active', '', 'instance', 'lease')".to_owned(),
            ))
            .await;
        assert!(
            empty_generation.is_err(),
            "the database must reject empty publication generation tokens"
        );
        for (identity, instance, lease) in [("c3", "", "lease"), ("c4", "instance", "")] {
            let empty_fence = db
                .execute(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "INSERT INTO provider_continuations (continuation_identity, owner_identity, target_fingerprint, key_id, cipher_version, created_at, expires_at, purge_after, publication_state, publication_generation, publication_instance_id, publication_lease_until) VALUES (?, 'o', 't', 'k', 1, 'now', 'later', 'latest', 'active', 'generation', ?, ?)",
                    [identity.into(), instance.into(), lease.into()],
                ))
                .await;
            assert!(
                empty_fence.is_err(),
                "the database must reject empty publication fencing tokens"
            );
        }
        Ok(())
    }

    #[test]
    fn continuation_registry_sql_is_portable_and_has_no_unauthenticated_backfill()
    -> anyhow::Result<()> {
        let statements = [
            provider_continuations_table().to_string(SqliteQueryBuilder),
            provider_continuations_table().to_string(PostgresQueryBuilder),
            provider_continuations_table().to_string(MysqlQueryBuilder),
        ];
        for statement in statements {
            let sql = statement.to_ascii_lowercase();
            assert!(sql.contains("publication_state"));
            assert!(sql.contains("publication_generation"));
            assert!(sql.contains("publication_instance_id"));
            assert!(sql.contains("publication_lease_until"));
            assert!(sql.contains("check"));
            assert!(sql.contains("provisional"));
            assert!(sql.contains("delivering"));
            assert!(sql.contains("active"));
            assert!(sql.contains("publication_generation") && sql.contains("<>"));
            assert!(sql.contains("publication_instance_id") && sql.contains("<>"));
            assert!(sql.contains("publication_lease_until") && sql.contains("<>"));
            assert!(
                !sql.contains("default"),
                "unpublished migration 013 must require authenticated values instead of backfilling an unauthenticated default: {statement}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn final_migrator_ends_with_the_authenticated_continuation_schema() -> anyhow::Result<()>
    {
        let migrations = Migrator::migrations();
        assert_eq!(
            migrations.last().map(|migration| migration.name()),
            Some("m20240101_000013_create_continuation_registry"),
            "the unpublished state-only 014 migration must not survive in the final sequence"
        );

        let db = crate::db::connect("sqlite::memory:").await?;
        Migrator::up(&db, None).await?;
        let columns = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "PRAGMA table_info('provider_continuations')".to_owned(),
            ))
            .await?
            .into_iter()
            .filter_map(|row| row.try_get::<String>("", "name").ok())
            .collect::<Vec<_>>();
        assert!(columns.iter().any(|column| column == "publication_state"));
        assert!(
            columns
                .iter()
                .any(|column| column == "publication_generation")
        );
        assert!(
            columns
                .iter()
                .any(|column| column == "publication_instance_id")
        );
        assert!(
            columns
                .iter()
                .any(|column| column == "publication_lease_until")
        );
        Ok(())
    }

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
