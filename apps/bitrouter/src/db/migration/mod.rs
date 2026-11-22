//! Fresh current schema construction and read-only storage validators.
//!
//! The sole baseline is installed only on an empty dataset. Existing datasets
//! must carry its exact marker and pass the current catalog validators before
//! startup. Component builders do not participate in a historical ledger.

mod m20240101_000001_create_auth_tables;
mod m20240101_000002_create_metering_tables;

mod m20240101_000004_create_adequacy_table;
mod m20240101_000005_create_adequacy_exploration_table;
mod m20240101_000006_create_adequacy_semantic_success_table;
mod m20240101_000007_add_metering_evidence;
mod m20240101_000008_add_metering_reconciliation;
mod m20240101_000009_create_adequacy_reliability_events;
mod m20240101_000010_create_eval_exchange;
mod m20240101_000011_scope_eval_exchange;
mod m20240101_000012_create_trajectory_ledger;
mod m20240101_000013_create_continuation_registry;
mod m20240101_000015_add_metering_launch_id;
mod m20240101_000016_add_acp_metering_identity;
mod m20240101_000017_add_metering_route_scope;

mod m20240101_000018_create_acp_capture;
mod m20240101_000019_create_acp_checkpoints;
mod m20240101_000020_create_checkpoint_evolution;
mod m20240101_000021_add_router_request_identity;
mod m20240101_000022_create_bro_execution_records;
mod m20240101_000023_create_bro_acceptance_keys;
mod m20240101_000024_create_bro_runtime_ownership;
mod m20240101_000025_create_bro_execution_index;
mod m20240101_000026_bro_runtime_format;
mod m20240101_000027_bro_managed_sessions;
mod m20240101_000028_resource_tenancy;
mod m20240101_000029_tenancy_configurations;
mod m20240101_000030_eval_storage_scope;
mod m20240101_000031_native_continuation_epoch;
mod m20240101_000032_trajectory_storage_scope;
mod m20240101_000033_capture_legacy_scope;
mod m20240101_000034_continuation_storage_scope;
mod m20240101_000035_execution_receipts;
mod m20240101_000036_native_harness_scope;
mod m20240101_000037_owned_oauth;
mod m20240101_000038_eval_import_provenance;
mod m20240101_000039_resource_lifecycle;
mod m20240101_000040_usage_evidence;
mod m20240101_000042_receipt_archive;
mod m20240101_000043_unknown_obligations;
mod m20240101_000044_capacity_upgrade;

mod m20240101_000045_ordered_receipts;
mod m20240101_000049_admission_index;

// Optional diagnostic schema is deliberately outside Migrator::migrations.
pub(crate) mod provider_technical;

mod baseline;

use sea_orm_migration::{MigrationTrait, MigratorTrait};

/// Validate the current SQLite receipt/archive catalog without writes.
pub(crate) async fn validate_receipt_archive(
    db: &impl sea_orm::ConnectionTrait,
) -> Result<(), sea_orm::DbErr> {
    m20240101_000042_receipt_archive::validate(db).await
}

/// Validate the complete current SQLite OAuth and obligation storage catalog.
pub(crate) async fn validate_owned_oauth(
    db: &impl sea_orm::ConnectionTrait,
) -> Result<(), sea_orm::DbErr> {
    m20240101_000037_owned_oauth::validate(db).await
}

/// Validate the current SQLite obligation layout without deriving facts.
pub(crate) async fn validate_unknown_obligations(
    db: &impl sea_orm::ConnectionTrait,
) -> Result<(), sea_orm::DbErr> {
    m20240101_000043_unknown_obligations::validate(db).await
}

/// Validate current usage evidence and its immutable receipt storage.
pub(crate) async fn validate_usage_evidence(
    db: &impl sea_orm::ConnectionTrait,
) -> Result<(), sea_orm::DbErr> {
    m20240101_000040_usage_evidence::validate(db).await
}

/// Validate the current authority catalog without changing authority state.
pub(crate) async fn validate_authority(
    db: &impl sea_orm::ConnectionTrait,
) -> Result<(), sea_orm::DbErr> {
    m20240101_000044_capacity_upgrade::validate_authority(db).await
}

/// Validate the current dispatch intent and receipt format catalog.
pub(crate) async fn validate_ordered_receipts(
    db: &impl sea_orm::ConnectionTrait,
) -> Result<(), sea_orm::DbErr> {
    m20240101_000045_ordered_receipts::validate(db).await
}

/// Validate the admission witness catalog and its ordered lifecycle head index.
///
/// # Errors
/// Rejects missing or incompatible current schema objects without changing data.
pub(crate) async fn validate_admission_index(
    db: &impl sea_orm::ConnectionTrait,
) -> Result<(), sea_orm::DbErr> {
    m20240101_000049_admission_index::validate(db).await
}

/// Installs the fresh baseline or validates an existing current dataset.
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    async fn up<'c, C>(db: C, _steps: Option<u32>) -> Result<(), sea_orm::DbErr>
    where
        C: sea_orm_migration::IntoSchemaManagerConnection<'c>,
    {
        baseline::open(db.into_schema_manager_connection()).await
    }

    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(baseline::Migration)]
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    use sea_orm_migration::prelude::{MysqlQueryBuilder, PostgresQueryBuilder, SqliteQueryBuilder};
    use sea_orm_migration::{MigratorTrait, SchemaManager};

    use super::Migrator;

    use super::m20240101_000012_create_trajectory_ledger::{
        trajectory_outbox_delivery_order_index, trajectory_prefix_index_table,
        trajectory_requests_owner_full_input_digest_index,
    };
    use super::m20240101_000013_create_continuation_registry::provider_continuations_table;

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
    async fn router_request_identity_columns_are_nullable() -> anyhow::Result<()> {
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

        for column in ["router_id", "binding_digest", "original_selector"] {
            assert_eq!(columns.get(column), Some(&0), "{column} must be nullable");
        }
        Ok(())
    }

    #[tokio::test]
    async fn continuation_registry_migration_is_bounded_and_private() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        let manager = SchemaManager::new(&db);

        Migrator::up(&db, None).await?;
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

        Migrator::up(&db, None).await?;
        assert!(manager.has_table("provider_continuations").await?);
        assert!(manager.has_table("provider_continuation_key_epoch").await?);
        Ok(())
    }

    #[tokio::test]
    async fn continuation_registry_rejects_illegal_state_and_empty_generation() -> anyhow::Result<()>
    {
        let db = crate::db::connect("sqlite::memory:").await?;
        Migrator::up(&db, None).await?;

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
        assert_eq!(migrations.len(), 1);
        assert_eq!(migrations[0].name(), crate::db::store_format::BASELINE);

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
    async fn current_trajectory_catalog_survives_restart() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        let manager = SchemaManager::new(&db);

        Migrator::up(&db, None).await?;
        Migrator::up(&db, None).await?;

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
        Ok(())
    }

    #[tokio::test]
    async fn trajectory_prefix_lookup_uses_the_owner_digest_index() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        Migrator::up(&db, None).await?;

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
        Migrator::up(&db, None).await?;

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
        Migrator::up(&db, None).await?;

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
