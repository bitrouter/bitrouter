//! Durable caller/operation keys share the execution transaction.
use sea_orm_migration::prelude::*;

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

/// DDL-only component of the fresh current baseline; never accepts an old dataset.
pub(super) async fn initialize_current(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
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
