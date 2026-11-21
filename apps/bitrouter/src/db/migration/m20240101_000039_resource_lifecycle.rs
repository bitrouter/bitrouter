//! Local managed heads and exact-revision revocations; no resource adoption.

use sea_orm::DatabaseBackend;
use sea_orm_migration::prelude::*;

use super::m20240101_000028_resource_tenancy::identity;

const TABLE: &str = "tenancy_resource_lifecycle";

pub(super) fn definition(backend: DatabaseBackend) -> TableCreateStatement {
    let mut table = Table::create();
    table.table(Alias::new(TABLE));
    if backend == DatabaseBackend::MySql {
        table.engine("InnoDB");
    }
    for (name, width) in [
        ("owner_kind", 32),
        ("owner_id", 128),
        ("resource_kind", 32),
        ("resource_id", 128),
        ("revision_id", 128),
        ("installation_id", 128),
    ] {
        table.col(identity(backend, name, width, false));
    }
    for name in ["is_head", "revoked"] {
        table.col(ColumnDef::new(Alias::new(name)).boolean().not_null());
    }
    table.col(
        ColumnDef::new(Alias::new("authority_revision"))
            .big_integer()
            .not_null(),
    );
    // Same byte-exact qualified identity and MySQL key width as immutable revisions.
    // A single installation owns the table; load validates every row's installation.
    let mut primary = Index::create();
    for name in [
        "owner_kind",
        "owner_id",
        "resource_kind",
        "resource_id",
        "revision_id",
    ] {
        primary.col(Alias::new(name));
    }
    table.primary_key(&mut primary).to_owned()
}

/// DDL-only component of the fresh current baseline; never accepts an old dataset.
pub(super) async fn initialize_current(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    if manager.has_table(TABLE).await? {
        return Err(DbErr::Custom(
            "migration039 resource lifecycle object already exists".into(),
        ));
    }
    manager
        .create_table(definition(manager.get_database_backend()))
        .await
}
