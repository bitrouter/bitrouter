//! Immutable private import evidence. No historical rows are adopted by DDL.
use super::m20240101_000028_resource_tenancy::{identity, payload};
use sea_orm_migration::prelude::*;

const TABLE: &str = "eval_import_provenance";

/// DDL-only component of the fresh current baseline; never accepts an old dataset.
pub(super) async fn initialize_current(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    // There is no legacy form of this table. Refuse a pre-existing object;
    // do not bless a same-name table or its contents as import evidence.
    if manager.has_table(TABLE).await? {
        return Err(DbErr::Custom(
            "migration038 provenance object already exists".into(),
        ));
    }
    let backend = manager.get_database_backend();
    let mut table = Table::create();
    table.table(Alias::new(TABLE));
    if backend == sea_orm::DatabaseBackend::MySql {
        table.engine("InnoDB");
    }
    for (name, width) in [
        ("installation_id", 128),
        ("tenant_id", 128),
        ("creator_principal_id", 128),
        ("snapshot_key", 64),
        ("manifest_sha256", 64),
        ("provenance_digest", 64),
    ] {
        table.col(identity(backend, name, width, false));
    }
    table.col(payload(backend, "provenance_json"));
    table.primary_key(Index::create().col(Alias::new("snapshot_key")));
    manager.create_table(table.to_owned()).await
}
