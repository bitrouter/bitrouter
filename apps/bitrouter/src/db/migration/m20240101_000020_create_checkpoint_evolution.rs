//! Durable evaluation jobs and versioned evolution records.

use sea_orm::DatabaseBackend;
use sea_orm_migration::prelude::*;

/// DDL-only component of the fresh current baseline; never accepts an old dataset.
pub(super) async fn initialize_current(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let mut body = ColumnDef::new(Alias::new("body"));
    // https://dev.mysql.com/doc/refman/8.4/en/blob.html
    if manager.get_database_backend() == DatabaseBackend::MySql {
        body.custom(Alias::new("LONGTEXT"));
    } else {
        body.text();
    }
    body.not_null();
    manager
        .create_table(
            Table::create()
                .table(Alias::new("checkpoint_evolution"))
                .col(
                    ColumnDef::new(Alias::new("record_id"))
                        .string_len(64)
                        .primary_key(),
                )
                .col(
                    ColumnDef::new(Alias::new("scope_id"))
                        .string_len(64)
                        .not_null(),
                )
                .col(ColumnDef::new(Alias::new("kind")).string_len(32).not_null())
                .col(ColumnDef::new(Alias::new("session_key")).string_len(64))
                .col(
                    ColumnDef::new(Alias::new("revision"))
                        .big_integer()
                        .not_null(),
                )
                .col(&mut body)
                .to_owned(),
        )
        .await?;
    manager
        .create_index(
            Index::create()
                .name("idx_checkpoint_evolution_scope_kind")
                .table(Alias::new("checkpoint_evolution"))
                .col(Alias::new("scope_id"))
                .col(Alias::new("kind"))
                .to_owned(),
        )
        .await?;
    manager
        .create_index(
            Index::create()
                .name("idx_checkpoint_evolution_session")
                .table(Alias::new("checkpoint_evolution"))
                .col(Alias::new("session_key"))
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// Original tenant scope columns and index on the gateway's retained records.
pub(super) async fn scope(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    for name in ["installation_id", "tenant_id", "creator_principal_id"] {
        let column = super::m20240101_000028_resource_tenancy::identity(
            manager.get_database_backend(),
            name,
            128,
            true,
        );
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("checkpoint_evolution"))
                    .add_column(column)
                    .to_owned(),
            )
            .await?;
    }
    manager
        .create_index(
            Index::create()
                .name("idx_legacy33_3_scope")
                .table(Alias::new("checkpoint_evolution"))
                .col(Alias::new("installation_id"))
                .col(Alias::new("tenant_id"))
                .to_owned(),
        )
        .await
}
