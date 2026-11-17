//! Current SQLite inline/archive receipt storage and catalog validation.
//!
//! Current writers atomically record payload locations and receipt formats.
//! Validation never changes payload bytes, digests, counters or authority.
use sea_orm::{ConnectionTrait, DatabaseBackend};
use sea_orm_migration::prelude::*;

use super::{
    m20240101_000028_resource_tenancy::identity, m20240101_000035_execution_receipts as receipts,
    m20240101_000040_usage_evidence as usage,
};

pub(super) mod catalog;
#[cfg(test)]
mod tests;

const TABLE: &str = "tenancy_receipt_archive";
const RECEIPTS: &str = "tenancy_execution_receipts";
const USAGE: &str = "tenancy_usage_evidence";
const STORAGE_INDEX: &str = "idx_tenancy_receipts_storage";

fn incompatible() -> DbErr {
    DbErr::Custom("migration042 incompatible receipt archive storage".into())
}

pub(super) fn marker() -> ColumnDef {
    ColumnDef::new(Alias::new("storage_kind"))
        .integer()
        .not_null()
        .default(0)
        .check(Expr::col(Alias::new("storage_kind")).is_in([0, 1]))
        .to_owned()
}

pub(super) fn definition() -> TableCreateStatement {
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

pub(super) fn storage_index() -> IndexCreateStatement {
    Index::create()
        .name(STORAGE_INDEX)
        .table(Alias::new(RECEIPTS))
        .col(Alias::new("installation_id"))
        .col(Alias::new("storage_kind"))
        .col(Alias::new("tenant_id"))
        .col(Alias::new("execution_id"))
        .to_owned()
}

/// Require the complete current receipt, evidence and archive catalog.
pub(super) async fn validate(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    if db.get_database_backend() != DatabaseBackend::Sqlite {
        return Err(incompatible());
    }
    catalog::validate(db).await
}
