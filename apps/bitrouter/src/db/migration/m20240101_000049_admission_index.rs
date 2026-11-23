//! Current persistent admission witnesses and bounded head lookup.

use sea_orm::{ConnectionTrait, DatabaseBackend};
use sea_orm_migration::prelude::*;

use super::m20240101_000028_resource_tenancy::{identity, payload};

pub(super) fn head_index() -> IndexCreateStatement {
    Index::create()
        .name("tenancy_resource_head_lookup")
        .table(Alias::new("tenancy_resource_lifecycle"))
        .col(Alias::new("owner_kind"))
        .col(Alias::new("owner_id"))
        .col(Alias::new("resource_kind"))
        .col(Alias::new("resource_id"))
        .col(Alias::new("is_head"))
        .to_owned()
}

pub(super) fn definition(backend: DatabaseBackend) -> TableCreateStatement {
    let mut table = Table::create();
    table.table(Alias::new("tenancy_admission_index"));
    if backend == DatabaseBackend::MySql {
        table.engine("InnoDB");
    }
    let mut primary = Index::create();
    for (name, width) in [
        ("index_kind", 32),
        ("owner_kind", 32),
        ("owner_id", 128),
        ("resource_kind", 32),
        ("resource_id", 128),
    ] {
        table.col(identity(backend, name, width, false));
        primary.col(Alias::new(name));
    }
    table
        .col(payload(backend, "reference_json"))
        .primary_key(&mut primary)
        .to_owned()
}

pub(super) async fn initialize_current(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    manager
        .create_table(definition(manager.get_database_backend()))
        .await?;
    manager.create_index(head_index()).await
}

pub(super) async fn validate(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    if db.get_database_backend() == DatabaseBackend::Sqlite {
        super::m20240101_000044_capacity_upgrade::validate_authority(db).await
    } else {
        crate::db::current_schema::validate(db).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, ensure};

    #[test]
    fn admission_index_ddl_keeps_qualified_keys_byte_exact_on_every_backend() -> Result<()> {
        for backend in [
            DatabaseBackend::Sqlite,
            DatabaseBackend::Postgres,
            DatabaseBackend::MySql,
        ] {
            let index = backend.build(&definition(backend)).to_string();
            let (collation, count) = match backend {
                DatabaseBackend::Sqlite => ("COLLATE BINARY", 5),
                DatabaseBackend::Postgres => ("COLLATE \"C\"", 5),
                DatabaseBackend::MySql => ("COLLATE utf8mb4_0900_bin", 6),
            };
            ensure!(index.matches(collation).count() == count);
            ensure!(index.contains("PRIMARY KEY") && index.contains("reference_json"));
        }
        Ok(())
    }
}
