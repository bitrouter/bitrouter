use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(SpendLogs::Table)
                    .if_not_exists()
                    .col(uuid(SpendLogs::Id).primary_key())
                    .col(string_null(SpendLogs::AccountId))
                    .col(string(SpendLogs::Model))
                    .col(string(SpendLogs::Provider))
                    .col(integer(SpendLogs::InputTokens))
                    .col(integer(SpendLogs::OutputTokens))
                    .col(double(SpendLogs::Cost))
                    .col(big_integer(SpendLogs::LatencyMs))
                    .col(boolean(SpendLogs::Success))
                    .col(string_null(SpendLogs::ErrorType))
                    .col(timestamp(SpendLogs::CreatedAt))
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_spend_logs_account_created")
                    .table(SpendLogs::Table)
                    .col(SpendLogs::AccountId)
                    .col(SpendLogs::CreatedAt)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(SpendLogs::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum SpendLogs {
    Table,
    Id,
    AccountId,
    Model,
    Provider,
    InputTokens,
    OutputTokens,
    Cost,
    LatencyMs,
    Success,
    ErrorType,
    CreatedAt,
}
