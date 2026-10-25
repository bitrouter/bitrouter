//! Retained, explicitly owned resources and configurations; no legacy assignment.

use sea_orm::DatabaseBackend;
use sea_orm_migration::prelude::*;

use super::m20240101_000028_resource_tenancy::{identity, payload};

#[derive(DeriveMigrationName)]
pub struct Migration;

const TABLES: [&str; 3] = [
    "tenancy_resource_revisions",
    "tenancy_configurations",
    "tenancy_active_configurations",
];

fn table(name: &str) -> TableCreateStatement {
    Table::create()
        .table(Alias::new(name))
        .if_not_exists()
        .to_owned()
}

fn primary(columns: &[&str]) -> IndexCreateStatement {
    let mut key = Index::create();
    for column in columns {
        key.col(Alias::new(*column));
    }
    key.to_owned()
}

fn authority_revision() -> ColumnDef {
    ColumnDef::new(Alias::new("authority_revision"))
        .big_integer()
        .not_null()
        .to_owned()
}

fn statements(backend: DatabaseBackend) -> Vec<TableCreateStatement> {
    let id = |name| identity(backend, name, 128, false);
    let text = |name| payload(backend, name);
    vec![
        table(TABLES[0])
            .col(identity(backend, "owner_kind", 32, false))
            .col(id("owner_id"))
            .col(identity(backend, "resource_kind", 32, false))
            .col(id("resource_id"))
            .col(id("revision_id"))
            .col(identity(backend, "compiler_version", 64, false))
            // Opaque encrypted artifact; dependency_json contains only nonsecret refs.
            .col(text("payload_json"))
            .col(text("dependency_json"))
            .col(identity(backend, "content_digest", 64, false))
            .col(identity(backend, "created_at", 64, false))
            // utf8mb4: (32 + 128 + 32 + 128 + 128) * 4 = 1792 bytes,
            // below MySQL 8.4's 3072-byte InnoDB key bound.
            .primary_key(&mut primary(&[
                "owner_kind",
                "owner_id",
                "resource_kind",
                "resource_id",
                "revision_id",
            ]))
            .to_owned(),
        table(TABLES[1])
            .col(id("tenant_id"))
            .col(id("revision_id"))
            .col(
                ColumnDef::new(Alias::new("schema_version"))
                    .integer()
                    .not_null(),
            )
            .col(text("document_json"))
            .col(identity(backend, "compiler_version", 64, false))
            .col(identity(backend, "content_digest", 64, false))
            // Opaque encrypted retained envelope, never serialized RoutingTarget secrets.
            .col(text("compiled_json"))
            .col(text("dependencies_json"))
            // Installation creators need not be rows in tenancy_principals.
            .col(id("creator_principal_id"))
            .col(authority_revision())
            .col(identity(backend, "created_at", 64, false))
            .primary_key(&mut primary(&["tenant_id", "revision_id"]))
            .foreign_key(
                ForeignKey::create()
                    .name("fk_tenancy_configuration_tenant")
                    .from(Alias::new(TABLES[1]), Alias::new("tenant_id"))
                    .to(Alias::new("tenancy_tenants"), Alias::new("id")),
            )
            .to_owned(),
        table(TABLES[2])
            .col(id("tenant_id").primary_key())
            .col(id("revision_id"))
            .col(authority_revision())
            .foreign_key(
                ForeignKey::create()
                    .name("fk_tenancy_active_configuration")
                    .from(
                        Alias::new(TABLES[2]),
                        (Alias::new("tenant_id"), Alias::new("revision_id")),
                    )
                    .to(
                        Alias::new(TABLES[1]),
                        (Alias::new("tenant_id"), Alias::new("revision_id")),
                    ),
            )
            .to_owned(),
    ]
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Every lookup uses a table's primary key. No separate CREATE INDEX is
        // needed, so resuming MySQL's committed DDL cannot duplicate an index.
        for statement in statements(manager.get_database_backend()) {
            manager.create_table(statement).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for name in TABLES.into_iter().rev() {
            manager
                .drop_table(Table::drop().table(Alias::new(name)).to_owned())
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
