//! Persist canonical route and authenticated caller identity for new
//! evaluation attempts. Historical rows remain null rather than guessed.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for column in [
            ColumnDef::new(EvaluationAttempts::CanonicalModel)
                .string()
                .to_owned(),
            ColumnDef::new(EvaluationAttempts::CallerApiKeyId)
                .string()
                .to_owned(),
            ColumnDef::new(EvaluationAttempts::CallerUserId)
                .string()
                .to_owned(),
        ] {
            manager
                .alter_table(
                    Table::alter()
                        .table(EvaluationAttempts::Table)
                        .add_column(column)
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for column in [
            EvaluationAttempts::CallerUserId,
            EvaluationAttempts::CallerApiKeyId,
            EvaluationAttempts::CanonicalModel,
        ] {
            manager
                .alter_table(
                    Table::alter()
                        .table(EvaluationAttempts::Table)
                        .drop_column(column)
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }
}

#[derive(DeriveIden)]
enum EvaluationAttempts {
    Table,
    CanonicalModel,
    CallerApiKeyId,
    CallerUserId,
}
