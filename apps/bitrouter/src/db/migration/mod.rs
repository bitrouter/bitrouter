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
// 000014 is deliberately skipped: it was `add_continuation_publication_state`,
// withdrawn before release and folded into 000013. Leaving the slot burned
// keeps the sequence unambiguous for anyone reading git history.
pub mod m20240101_000015_add_metering_launch_id;
pub mod m20240101_000016_add_acp_metering_identity;
pub mod m20240101_000017_add_metering_route_scope;

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
            Box::new(m20240101_000015_add_metering_launch_id::Migration),
            Box::new(m20240101_000016_add_acp_metering_identity::Migration),
            Box::new(m20240101_000017_add_metering_route_scope::Migration),
        ]
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    use sea_orm_migration::prelude::{MysqlQueryBuilder, PostgresQueryBuilder, SqliteQueryBuilder};
    use sea_orm_migration::{MigrationTrait, MigratorTrait, SchemaManager};

    use super::Migrator;

    use super::m20240101_000012_create_trajectory_ledger::{
        Migration, trajectory_outbox_delivery_order_index, trajectory_prefix_index_table,
        trajectory_requests_owner_full_input_digest_index,
    };
    use super::m20240101_000013_create_continuation_registry::{
        Migration as ContinuationMigration, provider_continuations_table,
    };

    #[tokio::test]
    async fn acp_metering_identity_columns_are_nullable_and_content_free() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        Migrator::up(&db, None).await?;
        let rows = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "PRAGMA table_info('requests')".to_owned(),
            ))
            .await?;
        let columns = rows
            .iter()
            .filter_map(|row| {
                let name = row.try_get::<String>("", "name").ok()?;
                let not_null = row.try_get::<i64>("", "notnull").ok()?;
                Some((name, not_null))
            })
            .collect::<std::collections::HashMap<_, _>>();

        for column in [
            "agent_harness",
            "controller_instance_id",
            "acp_session_id",
            "native_root_session_id",
            "native_agent_thread_id",
            "native_parent_agent_thread_id",
            "native_turn_id",
            "route_lease_id",
            "session_identity_json",
        ] {
            assert_eq!(columns.get(column), Some(&0), "{column} must be nullable");
        }
        for forbidden in [
            "prompt",
            "messages",
            "transcript",
            "authorization",
            "cookie",
        ] {
            assert!(!columns.contains_key(forbidden));
        }
        Ok(())
    }

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
        // The guard is about one specific withdrawn migration, not about the
        // sequence never growing again: `add_continuation_publication_state`
        // was folded into 000013 before release and must never reappear.
        // Asserting on its *name* keeps that promise while leaving the schema
        // free to move forward.
        assert!(
            !migrations.iter().any(|migration| migration.name()
                == "m20240101_000014_add_continuation_publication_state"),
            "the unpublished state-only 014 migration must not survive in the final sequence"
        );
        assert!(
            migrations.iter().any(
                |migration| migration.name() == "m20240101_000013_create_continuation_registry"
            ),
            "the continuation registry migration carries that folded-in state"
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
            "trajectory_prefix_index",
            "trajectory_outbox",
        ] {
            assert!(manager.has_table(table).await?);
        }
        for index in [
            "idx_trajectory_episodes_owner_correlation",
            "idx_trajectory_events_episode_sequence",
            "idx_trajectory_requests_owner_episode",
            "idx_trajectory_requests_owner_full_input_digest",
            "idx_trajectory_outbox_pending",
            "idx_trajectory_outbox_delivery_order",
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
            "trajectory_prefix_index",
            "trajectory_outbox",
        ] {
            assert!(!manager.has_table(table).await?);
        }
        assert!(manager.has_table("migration_test_sentinel").await?);
        Ok(())
    }

    #[tokio::test]
    async fn trajectory_prefix_lookup_uses_the_owner_digest_index() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        let manager = SchemaManager::new(&db);
        Migration.up(&manager).await?;

        let details = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "EXPLAIN QUERY PLAN SELECT episode_id, full_input_digest, ambiguous FROM trajectory_prefix_index WHERE owner_user_id = 'owner-a' AND full_input_digest IN ('digest-a', 'digest-b')".to_owned(),
            ))
            .await?
            .into_iter()
            .map(|row| row.try_get::<String>("", "detail"))
            .collect::<Result<Vec<_>, _>>()?
            .join("\n");

        assert!(
            details.contains("sqlite_autoindex_trajectory_prefix_index_1"),
            "prefix summary query did not use its owner/digest primary key: {details}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn trajectory_global_outbox_drain_uses_delivery_order_index() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        let manager = SchemaManager::new(&db);
        Migration.up(&manager).await?;

        let details = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "EXPLAIN QUERY PLAN SELECT outbox_id FROM trajectory_outbox WHERE delivered_at IS NULL ORDER BY attempts, created_at, outbox_id LIMIT 10".to_owned(),
            ))
            .await?
            .into_iter()
            .map(|row| row.try_get::<String>("", "detail"))
            .collect::<Result<Vec<_>, _>>()?
            .join("\n");

        assert!(
            details.contains("idx_trajectory_outbox_delivery_order"),
            "global outbox drain did not use the delivery-order index: {details}"
        );
        assert!(
            !details.contains("USE TEMP B-TREE FOR ORDER BY"),
            "global outbox drain still sorts outside the index: {details}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn trajectory_owner_outbox_inspection_keeps_its_owner_index() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        let manager = SchemaManager::new(&db);
        Migration.up(&manager).await?;

        let details = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "EXPLAIN QUERY PLAN SELECT outbox_id FROM trajectory_outbox WHERE owner_user_id = 'owner-a' AND delivered_at IS NULL ORDER BY attempts, created_at, outbox_id".to_owned(),
            ))
            .await?
            .into_iter()
            .map(|row| row.try_get::<String>("", "detail"))
            .collect::<Result<Vec<_>, _>>()?
            .join("\n");

        assert!(
            details.contains("idx_trajectory_outbox_pending"),
            "owner outbox inspection did not retain its owner index: {details}"
        );
        assert!(
            !details.contains("USE TEMP B-TREE FOR ORDER BY"),
            "owner outbox inspection still sorts outside the index: {details}"
        );
        Ok(())
    }

    #[test]
    fn trajectory_outbox_delivery_index_sql_is_portable_and_query_ordered() -> anyhow::Result<()> {
        let statements = [
            trajectory_outbox_delivery_order_index().to_string(SqliteQueryBuilder),
            trajectory_outbox_delivery_order_index().to_string(PostgresQueryBuilder),
            trajectory_outbox_delivery_order_index().to_string(MysqlQueryBuilder),
        ];
        for statement in statements {
            let sql = statement.to_ascii_lowercase();
            let delivered = sql.rfind("delivered_at").ok_or_else(|| {
                anyhow::anyhow!("delivered-at column missing from index SQL: {statement}")
            })?;
            let attempts = sql.rfind("attempts").ok_or_else(|| {
                anyhow::anyhow!("attempts column missing from index SQL: {statement}")
            })?;
            let created = sql.rfind("created_at").ok_or_else(|| {
                anyhow::anyhow!("created-at column missing from index SQL: {statement}")
            })?;
            let outbox = sql.rfind("outbox_id").ok_or_else(|| {
                anyhow::anyhow!("outbox-id column missing from index SQL: {statement}")
            })?;
            assert!(sql.contains("idx_trajectory_outbox_delivery_order"));
            assert!(
                delivered < attempts && attempts < created && created < outbox,
                "delivery index columns must match the global drain order: {statement}"
            );
        }
        Ok(())
    }

    #[test]
    fn trajectory_request_lookup_index_sql_is_portable_and_owner_digest_ordered()
    -> anyhow::Result<()> {
        let statements = [
            trajectory_requests_owner_full_input_digest_index().to_string(SqliteQueryBuilder),
            trajectory_requests_owner_full_input_digest_index().to_string(PostgresQueryBuilder),
            trajectory_requests_owner_full_input_digest_index().to_string(MysqlQueryBuilder),
        ];
        for statement in statements {
            let sql = statement.to_ascii_lowercase();
            let owner = sql.rfind("owner_user_id").ok_or_else(|| {
                anyhow::anyhow!("owner column missing from index SQL: {statement}")
            })?;
            let digest = sql.rfind("full_input_digest").ok_or_else(|| {
                anyhow::anyhow!("full-input digest column missing from index SQL: {statement}")
            })?;
            assert!(sql.contains("idx_trajectory_requests_owner_full_input_digest"));
            assert!(
                owner < digest,
                "owner must lead digest in index SQL: {statement}"
            );
        }
        Ok(())
    }

    #[test]
    fn trajectory_prefix_summary_table_sql_is_portable_and_owner_digest_keyed() -> anyhow::Result<()>
    {
        let statements = [
            trajectory_prefix_index_table().to_string(SqliteQueryBuilder),
            trajectory_prefix_index_table().to_string(PostgresQueryBuilder),
            trajectory_prefix_index_table().to_string(MysqlQueryBuilder),
        ];
        for statement in statements {
            let sql = statement.to_ascii_lowercase();
            let owner = sql.rfind("owner_user_id").ok_or_else(|| {
                anyhow::anyhow!("owner column missing from prefix table SQL: {statement}")
            })?;
            let digest = sql.rfind("full_input_digest").ok_or_else(|| {
                anyhow::anyhow!("digest column missing from prefix table SQL: {statement}")
            })?;
            assert!(sql.contains("trajectory_prefix_index"));
            assert!(sql.contains("episode_id"));
            assert!(sql.contains("ambiguous"));
            assert!(sql.contains("primary key"));
            assert!(
                owner < digest,
                "owner must lead digest in the prefix-table primary key: {statement}"
            );
        }
        Ok(())
    }
}
