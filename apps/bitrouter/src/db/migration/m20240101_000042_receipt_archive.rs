//! SQLite receipt payload locations and an initially empty archive.
//!
//! This migration does not archive data, change authority format, or enable the
//! archive writer. Format-2 activation and the owned maintenance driver are
//! separate prerequisites. Every retained byte/digest/count stays unchanged;
//! existing rows acquire only an inline location marker.
use sea_orm::{ConnectionTrait, DatabaseBackend, TransactionTrait};
use sea_orm_migration::prelude::*;

use super::{
    m20240101_000028_resource_tenancy::identity, m20240101_000035_execution_receipts as receipts,
    m20240101_000040_usage_evidence as usage,
};

pub(super) mod catalog;
#[cfg(test)]
mod tests;

#[derive(DeriveMigrationName)]
pub struct Migration;

const TABLE: &str = "tenancy_receipt_archive";
const RECEIPTS: &str = "tenancy_execution_receipts";
const USAGE: &str = "tenancy_usage_evidence";
const STORAGE_INDEX: &str = "idx_tenancy_receipts_storage";

fn incompatible() -> DbErr {
    DbErr::Custom("migration042 incompatible receipt archive storage".into())
}

fn marker() -> ColumnDef {
    ColumnDef::new(Alias::new("storage_kind"))
        .integer()
        .not_null()
        .default(0)
        .check(Expr::col(Alias::new("storage_kind")).is_in([0, 1]))
        .to_owned()
}

fn definition() -> TableCreateStatement {
    let mut table = Table::create();
    table.table(Alias::new(TABLE));
    for name in ["installation_id", "tenant_id", "execution_id"] {
        table.col(identity(DatabaseBackend::Sqlite, name, 128, false));
    }
    for name in ["format_version", "codec", "receipt_bytes", "usage_bytes"] {
        table.col(ColumnDef::new(Alias::new(name)).big_integer().not_null());
    }
    table.col(ColumnDef::new(Alias::new("payload")).blob().not_null());
    table
        .primary_key(
            Index::create()
                .col(Alias::new("installation_id"))
                .col(Alias::new("tenant_id"))
                .col(Alias::new("execution_id")),
        )
        .to_owned()
}

fn receipt_definition() -> TableCreateStatement {
    receipts::definition(DatabaseBackend::Sqlite)
        .col(marker())
        .to_owned()
}

fn usage_definition() -> TableCreateStatement {
    usage::definition(DatabaseBackend::Sqlite)
        .col(marker())
        .to_owned()
}

fn storage_index() -> IndexCreateStatement {
    Index::create()
        .name(STORAGE_INDEX)
        .table(Alias::new(RECEIPTS))
        .col(Alias::new("installation_id"))
        .col(Alias::new("storage_kind"))
        .col(Alias::new("tenant_id"))
        .col(Alias::new("execution_id"))
        .to_owned()
}

/// Exact complete upgraded schema; never accepts one upgraded table in isolation.
pub(super) async fn validate(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    if db.get_database_backend() != DatabaseBackend::Sqlite {
        return Err(incompatible());
    }
    catalog::validate(db).await
}

async fn upgrade(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let db = manager.get_connection();
    catalog::no_shadows(db).await?;
    if catalog::present(db).await? {
        // A ledger-managed repeat skips up(). An unrecorded archive object,
        // even with matching DDL, must never be adopted as an applied migration.
        return Err(incompatible());
    }
    receipts::validate_legacy(db).await?;
    usage::validate_legacy(db).await?;
    // ADD COLUMN preserves all original values and indexes; no row decoding,
    // copying, canonicalization, or inferred correction of historical evidence.
    // https://www.sqlite.org/lang_altertable.html#alter_table_add_column
    for table in [RECEIPTS, USAGE] {
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new(table))
                    .add_column(marker())
                    .to_owned(),
            )
            .await?;
    }
    manager.create_table(definition()).await?;
    manager.create_index(storage_index()).await?;
    validate(db).await
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() != DatabaseBackend::Sqlite {
            return Ok(());
        }
        // Also make direct invocation atomic. Under run_migrations this is a
        // savepoint; the outer transaction owns the DDL plus migration ledger.
        let tx = manager.get_connection().begin().await?;
        let result = upgrade(&SchemaManager::new(&tx)).await;
        match result {
            Ok(()) => tx.commit().await,
            Err(error) => {
                tx.rollback().await?;
                Err(error)
            }
        }
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() != DatabaseBackend::Sqlite {
            return Ok(());
        }
        // Removing location markers or an archive table is not a schema-only
        // rollback once a future writer has populated it. Never discard history.
        Err(DbErr::Custom(
            "migration042 requires verified-backup rollback".into(),
        ))
    }
}
