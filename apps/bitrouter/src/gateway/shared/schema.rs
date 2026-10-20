//! Shared-only schema with an independent migration ledger.

use sea_orm::{ConnectionTrait, DatabaseBackend, IsolationLevel, Statement};
use sea_orm_migration::prelude::*;

pub(super) struct SharedMigrator;

/// Serialize schema changes only; this lock never participates in inference.
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
        vec![Box::new(Initial), Box::new(NativeContinuations)]
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
