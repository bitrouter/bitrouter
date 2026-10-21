//! Shared-only schema with an independent migration ledger.

use sea_orm::{ConnectionTrait, DatabaseBackend, IsolationLevel, Statement};
use sea_orm_migration::prelude::*;

pub(super) struct SharedMigrator;

/// Serialize schema changes only; this lock never participates in inference.
#[cfg(test)]
pub(super) async fn initialize(store: &super::store::Store) -> anyhow::Result<()> {
    initialize_selected(store, false).await
}

/// Explicit stock bootstrap also needs the ordinary auth/observation tables.
/// Both ledgers share the schema lock; replica startup calls neither initializer.
pub(super) async fn initialize_all(store: &super::store::Store) -> anyhow::Result<()> {
    initialize_selected(store, true).await
}

async fn initialize_selected(store: &super::store::Store, ordinary: bool) -> anyhow::Result<()> {
    store
        .bounded(async {
            // A waiter must see the preceding initializer's committed ledger
            // after acquiring the advisory lock, rather than an earlier snapshot.
            let tx = store
                .begin_with_isolation(IsolationLevel::ReadCommitted)
                .await?;
            // The fixed transaction advisory lock also protects first creation of
            // the migration ledger by two independent initializing processes.
            // https://www.postgresql.org/docs/current/explicit-locking.html#ADVISORY-LOCKS
            tx.execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT pg_advisory_xact_lock($1)",
                [0x4252_4757_5343_4831_i64.into()],
            ))
            .await?;
            if ordinary {
                crate::db::migration::Migrator::up(&tx, None).await?;
            }
            SharedMigrator::up(&tx, None).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
}

#[async_trait::async_trait]
impl MigratorTrait for SharedMigrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(Initial),
            Box::new(NativeContinuations),
            Box::new(SharedMetering),
            Box::new(SharedTrajectory),
            Box::new(SharedEvaluation),
        ]
    }

    fn migration_table_name() -> DynIden {
        Alias::new("gateway_shared_migrations").into_iden()
    }
}

struct Initial;

impl MigrationName for Initial {
    fn name(&self) -> &str {
        "m20261019_000001_gateway_shared_authority"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Initial {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() != DatabaseBackend::Postgres {
            return Err(DbErr::Custom(
                "shared gateway authority requires PostgreSQL".into(),
            ));
        }
        manager
            .create_table(
                Table::create()
                    .table(Records::Table)
                    .col(
                        ColumnDef::new(Records::Id)
                            .string_len(64)
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Records::Namespace).string_len(64).not_null())
                    .col(ColumnDef::new(Records::Kind).string_len(32).not_null())
                    .col(ColumnDef::new(Records::Key).string_len(256).not_null())
                    .col(ColumnDef::new(Records::Revision).big_integer().not_null())
                    .col(ColumnDef::new(Records::Status).string_len(32).not_null())
                    .col(ColumnDef::new(Records::DueAtMs).big_integer())
                    .col(
                        ColumnDef::new(Records::UpdatedAtMs)
                            .big_integer()
                            .not_null(),
                    )
                    .col(ColumnDef::new(Records::Body).text().not_null())
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("gateway_shared_due")
                    .table(Records::Table)
                    .col(Records::Namespace)
                    .col(Records::Kind)
                    .col(Records::Status)
                    .col(Records::DueAtMs)
                    .col(Records::Id)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Records::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum Records {
    #[sea_orm(iden = "gateway_shared_records")]
    Table,
    Id,
    Namespace,
    Kind,
    Key,
    Revision,
    Status,
    DueAtMs,
    UpdatedAtMs,
    Body,
}

struct NativeContinuations;

impl MigrationName for NativeContinuations {
    fn name(&self) -> &str {
        "m20261021_000002_shared_native_continuations"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for NativeContinuations {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Keep local native rows and their singleton key epoch untouched.
        // Times duplicate authenticated body fields only for bounded scans;
        // readers validate those copies before using a row.
        // https://www.postgresql.org/docs/current/sql-createtable.html
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE gateway_shared_native_continuations (
                    namespace varchar(64) NOT NULL,
                    identity varchar(256) NOT NULL,
                    revision bigint NOT NULL CHECK (revision >= 0),
                    body text NOT NULL,
                    publication_state varchar(32) NOT NULL,
                    lease_until varchar(32) NOT NULL,
                    expires_at varchar(32) NOT NULL,
                    purge_after varchar(32) NOT NULL,
                    PRIMARY KEY (namespace, identity)
                );
                CREATE INDEX gateway_shared_native_lease
                    ON gateway_shared_native_continuations
                    (namespace, publication_state, lease_until, identity);
                CREATE INDEX gateway_shared_native_expiry
                    ON gateway_shared_native_continuations
                    (namespace, expires_at, identity);
                CREATE INDEX gateway_shared_native_purge
                    ON gateway_shared_native_continuations
                    (namespace, purge_after, identity);",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE gateway_shared_native_continuations")
            .await?;
        Ok(())
    }
}

struct SharedMetering;

impl MigrationName for SharedMetering {
    fn name(&self) -> &str {
        "m20261021_000003_shared_metering"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for SharedMetering {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // One terminal execution is the immutable replay unit. Scoped unique
        // keys also reject reusing an event or canonical request across owners.
        // https://www.postgresql.org/docs/current/ddl-constraints.html
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE gateway_shared_metering_requests (
                namespace varchar(64) NOT NULL,
                execution_id varchar(256) NOT NULL,
                tenant_id varchar(256) NOT NULL,
                principal_id varchar(256) NOT NULL,
                request_id varchar(256),
                event_id varchar(256) NOT NULL,
                source_revision bigint NOT NULL CHECK (source_revision > 0),
                source_digest varchar(64) NOT NULL,
                accepted_at_ms bigint NOT NULL CHECK (accepted_at_ms >= 0),
                terminal_at_ms bigint NOT NULL CHECK (terminal_at_ms >= accepted_at_ms),
                known_subtotal_micro_usd numeric(20,0) NOT NULL
                    CHECK (known_subtotal_micro_usd BETWEEN 0 AND 18446744073709551615),
                complete_total_micro_usd numeric(20,0)
                    CHECK (complete_total_micro_usd BETWEEN 0 AND 18446744073709551615),
                known_attempts integer NOT NULL CHECK (known_attempts >= 0),
                unknown_attempts integer NOT NULL CHECK (unknown_attempts >= 0),
                not_dispatched_attempts integer NOT NULL CHECK (not_dispatched_attempts >= 0),
                payload text NOT NULL CHECK (octet_length(payload) <= 6701056),
                PRIMARY KEY (namespace, execution_id),
                UNIQUE (namespace, event_id),
                UNIQUE (namespace, request_id),
                CHECK (known_attempts + unknown_attempts + not_dispatched_attempts <= 32),
                CHECK ((known_attempts = 0 AND known_subtotal_micro_usd = 0) OR known_attempts > 0),
                CHECK ((unknown_attempts = 0 AND known_attempts > 0
                        AND complete_total_micro_usd IS NOT NULL
                        AND complete_total_micro_usd = known_subtotal_micro_usd)
                    OR ((unknown_attempts > 0 OR known_attempts = 0)
                        AND complete_total_micro_usd IS NULL))
             );
             CREATE INDEX gateway_shared_metering_tenant_time
                 ON gateway_shared_metering_requests
                 (namespace, tenant_id, accepted_at_ms, execution_id);
             CREATE INDEX gateway_shared_metering_principal_time
                 ON gateway_shared_metering_requests
                 (namespace, tenant_id, principal_id, accepted_at_ms, execution_id);",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE gateway_shared_metering_requests")
            .await?;
        Ok(())
    }
}

struct SharedTrajectory;

impl MigrationName for SharedTrajectory {
    fn name(&self) -> &str {
        "m20261021_000004_shared_trajectory"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for SharedTrajectory {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // All natural identities and secondary indexes include the deployment
        // namespace. Local trajectory tables are not altered or copied.
        // https://www.postgresql.org/docs/current/ddl-constraints.html
        manager.get_connection().execute_unprepared(
            "CREATE TABLE gateway_shared_trajectory_episodes (
                namespace varchar(64) NOT NULL,
                episode_id varchar(512) NOT NULL,
                owner_user_id varchar(512) NOT NULL,
                correlation_digest varchar(512) NOT NULL,
                correlation_key_id varchar(128) NOT NULL,
                correlation_source varchar(512) NOT NULL,
                history_completeness varchar(32) NOT NULL,
                next_sequence bigint NOT NULL CHECK (next_sequence > 0),
                first_captured_at varchar(64) NOT NULL,
                last_captured_at varchar(64) NOT NULL,
                closed_at varchar(64),
                latest_request_id varchar(512),
                PRIMARY KEY (namespace, episode_id)
             );
             CREATE INDEX gateway_shared_trajectory_episode_owner
                 ON gateway_shared_trajectory_episodes (namespace, owner_user_id, episode_id);
             CREATE TABLE gateway_shared_trajectory_events (
                namespace varchar(64) NOT NULL,
                event_id varchar(512) NOT NULL,
                owner_user_id varchar(512) NOT NULL,
                episode_id varchar(512) NOT NULL,
                request_id varchar(512),
                sequence bigint NOT NULL CHECK (sequence > 0),
                kind varchar(32) NOT NULL,
                event_json text NOT NULL,
                content_digest varchar(128) NOT NULL,
                captured_at varchar(64) NOT NULL,
                PRIMARY KEY (namespace, event_id),
                UNIQUE (namespace, episode_id, sequence)
             );
             CREATE INDEX gateway_shared_trajectory_history
                 ON gateway_shared_trajectory_events (namespace, owner_user_id, episode_id, sequence);
             CREATE TABLE gateway_shared_trajectory_requests (
                namespace varchar(64) NOT NULL,
                request_id varchar(512) NOT NULL,
                owner_user_id varchar(512) NOT NULL,
                episode_id varchar(512) NOT NULL,
                start_event_id varchar(512) NOT NULL,
                settlement_event_id varchar(512),
                settlement_outbox_id varchar(512),
                full_input_digest varchar(512) NOT NULL,
                native_parent_id varchar(512),
                protocol varchar(512) NOT NULL,
                status varchar(32) NOT NULL,
                PRIMARY KEY (namespace, request_id)
             );
             CREATE INDEX gateway_shared_trajectory_request_episode
                 ON gateway_shared_trajectory_requests (namespace, owner_user_id, episode_id, request_id);
             CREATE TABLE gateway_shared_trajectory_prefix_index (
                namespace varchar(64) NOT NULL,
                owner_user_id varchar(512) NOT NULL,
                full_input_digest varchar(512) NOT NULL,
                episode_id varchar(512) NOT NULL,
                ambiguous boolean NOT NULL,
                PRIMARY KEY (namespace, owner_user_id, full_input_digest)
             );
             CREATE INDEX gateway_shared_trajectory_prefix_episode
                 ON gateway_shared_trajectory_prefix_index (namespace, owner_user_id, episode_id);
             CREATE TABLE gateway_shared_trajectory_outbox (
                namespace varchar(64) NOT NULL,
                outbox_id varchar(512) NOT NULL,
                owner_user_id varchar(512) NOT NULL,
                topic varchar(512) NOT NULL,
                payload_json text NOT NULL,
                payload_digest varchar(128) NOT NULL,
                attempts bigint NOT NULL CHECK (attempts >= 0),
                created_at varchar(64) NOT NULL,
                delivered_at varchar(64),
                PRIMARY KEY (namespace, outbox_id)
             );
             CREATE INDEX gateway_shared_trajectory_outbox_pending
                 ON gateway_shared_trajectory_outbox (namespace, delivered_at, attempts, created_at, outbox_id);"
        ).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP TABLE gateway_shared_trajectory_outbox;
             DROP TABLE gateway_shared_trajectory_prefix_index;
             DROP TABLE gateway_shared_trajectory_requests;
             DROP TABLE gateway_shared_trajectory_events;
             DROP TABLE gateway_shared_trajectory_episodes;",
            )
            .await?;
        Ok(())
    }
}

struct SharedEvaluation;

impl MigrationName for SharedEvaluation {
    fn name(&self) -> &str {
        "m20261021_000005_shared_evaluation"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for SharedEvaluation {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Namespace is part of every identity and relational reference. A global
        // generated sequence orders admissions; gaps across namespaces are safe.
        // https://www.postgresql.org/docs/current/ddl-identity-columns.html
        // https://www.postgresql.org/docs/current/ddl-constraints.html
        manager.get_connection().execute_unprepared(
            "CREATE TABLE gateway_shared_eval_subjects (
                namespace varchar(64) NOT NULL,
                eval_id varchar(512) NOT NULL,
                owner_user_id varchar(512) NOT NULL,
                subject_id varchar(512) NOT NULL,
                scope varchar(32) NOT NULL,
                policy_digest varchar(128) NOT NULL,
                holdout boolean NOT NULL,
                content_digest varchar(128) NOT NULL,
                subject_json text NOT NULL CHECK (octet_length(subject_json) <= 1048576),
                created_at varchar(64) NOT NULL,
                PRIMARY KEY (namespace,eval_id)
             );
             CREATE INDEX gateway_shared_eval_subject_owner
                ON gateway_shared_eval_subjects (namespace,owner_user_id,eval_id);
             CREATE TABLE gateway_shared_eval_results (
                namespace varchar(64) NOT NULL,
                result_id varchar(128) NOT NULL,
                owner_user_id varchar(512) NOT NULL,
                eval_id varchar(512) NOT NULL,
                idempotency_key varchar(512) NOT NULL,
                authority_id varchar(512) NOT NULL,
                evaluator_kind varchar(32) NOT NULL,
                content_digest varchar(128) NOT NULL,
                result_json text NOT NULL CHECK (octet_length(result_json) <= 1048576),
                created_at varchar(64) NOT NULL,
                PRIMARY KEY (namespace,result_id),
                UNIQUE (namespace,owner_user_id,idempotency_key),
                FOREIGN KEY (namespace,eval_id) REFERENCES gateway_shared_eval_subjects(namespace,eval_id)
             );
             CREATE INDEX gateway_shared_eval_result_subject
                ON gateway_shared_eval_results (namespace,owner_user_id,eval_id,result_id);
             CREATE INDEX gateway_shared_eval_result_owner
                ON gateway_shared_eval_results (namespace,owner_user_id,result_id);
             CREATE TABLE gateway_shared_eval_admission_events (
                namespace varchar(64) NOT NULL,
                sequence bigint GENERATED BY DEFAULT AS IDENTITY,
                result_id varchar(128) NOT NULL,
                status varchar(32) NOT NULL CHECK (status IN ('admitted','rejected','held_out','disputed')),
                reason varchar(512) NOT NULL,
                authority_id varchar(512) NOT NULL,
                created_at varchar(64) NOT NULL,
                PRIMARY KEY (namespace,sequence),
                FOREIGN KEY (namespace,result_id) REFERENCES gateway_shared_eval_results(namespace,result_id)
             );
             CREATE INDEX gateway_shared_eval_admission_latest
                ON gateway_shared_eval_admission_events (namespace,result_id,sequence DESC);
             CREATE TABLE gateway_shared_eval_snapshots (
                namespace varchar(64) NOT NULL,
                evidence_root varchar(128) NOT NULL,
                owner_user_id varchar(512) NOT NULL,
                manifest_json text NOT NULL CHECK (octet_length(manifest_json) <= 1048576),
                result_count integer NOT NULL CHECK (result_count >= 0),
                frozen_at varchar(64) NOT NULL,
                PRIMARY KEY (namespace,evidence_root)
             );"
        ).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP TABLE gateway_shared_eval_snapshots;
             DROP TABLE gateway_shared_eval_admission_events;
             DROP TABLE gateway_shared_eval_results;
             DROP TABLE gateway_shared_eval_subjects;",
            )
            .await?;
        Ok(())
    }
}
