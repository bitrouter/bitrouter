//! Content-free, append-only terminal and charge evidence for evaluation attempts.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(EvaluationAttempts::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(EvaluationAttempts::AttemptId)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(EvaluationAttempts::RequestId)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(EvaluationAttempts::Selector)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(EvaluationAttempts::Provider)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(EvaluationAttempts::ProviderModelId)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(EvaluationAttempts::ReportedModel).string())
                    .col(ColumnDef::new(EvaluationAttempts::AccountLabel).string())
                    .col(
                        ColumnDef::new(EvaluationAttempts::AttemptIndex)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(EvaluationAttempts::Format)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(EvaluationAttempts::DurationMs)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(EvaluationAttempts::Terminal)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(EvaluationAttempts::ErrorCode).string())
                    .col(ColumnDef::new(EvaluationAttempts::InputTokens).string())
                    .col(ColumnDef::new(EvaluationAttempts::OutputTokens).string())
                    .col(
                        ColumnDef::new(EvaluationAttempts::ChargeStatus)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(EvaluationAttempts::ChargeMicroUsd).big_integer())
                    .col(ColumnDef::new(EvaluationAttempts::ChargeEvidenceJson).text())
                    .col(
                        ColumnDef::new(EvaluationAttempts::CreatedAt)
                            .string()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_evaluation_attempts_request")
                    .table(EvaluationAttempts::Table)
                    .col(EvaluationAttempts::RequestId)
                    .col(EvaluationAttempts::AttemptIndex)
                    .to_owned(),
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(EvaluationAttempts::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum EvaluationAttempts {
    Table,
    AttemptId,
    RequestId,
    Selector,
    Provider,
    ProviderModelId,
    ReportedModel,
    AccountLabel,
    AttemptIndex,
    Format,
    DurationMs,
    Terminal,
    ErrorCode,
    InputTokens,
    OutputTokens,
    ChargeStatus,
    ChargeMicroUsd,
    ChargeEvidenceJson,
    CreatedAt,
}
