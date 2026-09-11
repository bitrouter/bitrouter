//! Durable evaluation jobs and versioned evolution records.

use sea_orm::DatabaseBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
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

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(Alias::new("checkpoint_evolution"))
                    .to_owned(),
            )
            .await
    }
}
