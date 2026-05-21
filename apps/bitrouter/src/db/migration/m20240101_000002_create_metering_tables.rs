//! Create the metering module's `requests` table.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Requests::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Requests::RequestId)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Requests::UserId).string().not_null())
                    .col(ColumnDef::new(Requests::ApiKeyId).string().not_null())
                    .col(ColumnDef::new(Requests::ModelId).string().not_null())
                    .col(ColumnDef::new(Requests::ProviderId).string().not_null())
                    .col(
                        ColumnDef::new(Requests::PromptTokens)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Requests::CompletionTokens)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Requests::ReasoningTokens)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Requests::CacheReadTokens)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(Requests::CacheWriteTokens)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(Requests::EstimatedChargeMicroUsd)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(Requests::Streamed)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(Requests::LatencyMs)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(Requests::GenerationTimeMs)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(ColumnDef::new(Requests::Error).string())
                    .col(ColumnDef::new(Requests::CreatedAt).string().not_null())
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_requests_api_key_created")
                    .table(Requests::Table)
                    .col(Requests::ApiKeyId)
                    .col(Requests::CreatedAt)
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_requests_user_created")
                    .table(Requests::Table)
                    .col(Requests::UserId)
                    .col(Requests::CreatedAt)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Requests::Table).to_owned())
            .await?;
        Ok(())
    }
}

#[derive(DeriveIden)]
enum Requests {
    Table,
    RequestId,
    UserId,
    ApiKeyId,
    ModelId,
    ProviderId,
    PromptTokens,
    CompletionTokens,
    ReasoningTokens,
    CacheReadTokens,
    CacheWriteTokens,
    EstimatedChargeMicroUsd,
    Streamed,
    LatencyMs,
    GenerationTimeMs,
    Error,
    CreatedAt,
}
