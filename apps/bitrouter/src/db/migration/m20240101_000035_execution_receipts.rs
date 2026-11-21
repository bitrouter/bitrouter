//! Immutable tenant execution receipts. This table is never a dispatch log.
use super::m20240101_000028_resource_tenancy::{identity, payload};
use sea_orm::DatabaseBackend;
use sea_orm_migration::prelude::*;

const TABLE: &str = "tenancy_execution_receipts";
const INDEX: &str = "idx_tenancy_receipts_correlation";
const IDS: [&str; 5] = [
    "installation_id",
    "tenant_id",
    "execution_id",
    "principal_id",
    "correlation_id",
];

pub(super) fn definition(backend: DatabaseBackend) -> TableCreateStatement {
    let mut table = Table::create();
    table.table(Alias::new(TABLE));
    if backend == DatabaseBackend::MySql {
        table.engine("InnoDB");
    }
    for id in IDS {
        table.col(identity(backend, id, 128, false));
    }
    table
        .col(payload(backend, "receipt_json"))
        .col(identity(backend, "content_digest", 64, false))
        .col(payload(backend, "disposition_json"))
        .col(
            ColumnDef::new(Alias::new("byte_count"))
                .big_integer()
                .not_null(),
        )
        .col(
            ColumnDef::new(Alias::new("unknown_outcome"))
                .boolean()
                .not_null(),
        )
        .col(identity(backend, "recorded_at", 20, false))
        .primary_key(
            Index::create()
                .col(Alias::new("installation_id"))
                .col(Alias::new("tenant_id"))
                .col(Alias::new("execution_id")),
        )
        .to_owned()
}

pub(super) fn correlation() -> IndexCreateStatement {
    Index::create()
        .name(INDEX)
        .table(Alias::new(TABLE))
        .col(Alias::new("installation_id"))
        .col(Alias::new("tenant_id"))
        .col(Alias::new("correlation_id"))
        .col(Alias::new("execution_id"))
        .to_owned()
}
