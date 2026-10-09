//! Durable caller/operation keys share the execution transaction.
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(BroAcceptanceKeys::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(BroAcceptanceKeys::Scope)
                            .string_len(512)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(BroAcceptanceKeys::Key)
                            .string_len(128)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(BroAcceptanceKeys::Fingerprint)
                            .string_len(64)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(BroAcceptanceKeys::ThreadId)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(BroAcceptanceKeys::TurnId).string())
                    .primary_key(
                        Index::create()
                            .col(BroAcceptanceKeys::Scope)
                            .col(BroAcceptanceKeys::Key),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(BroAcceptanceKeys::Table, BroAcceptanceKeys::ThreadId)
                            .to(BroExecutions::Table, BroExecutions::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(BroAcceptanceKeys::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum BroAcceptanceKeys {
    Table,
    Scope,
    Key,
    Fingerprint,
    ThreadId,
    TurnId,
}
#[derive(DeriveIden)]
enum BroExecutions {
    Table,
    Id,
}
