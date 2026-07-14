//! Add cache-aware normalized usage and auditable charge evidence to metering.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Requests::Table)
                    .add_column(
                        ColumnDef::new(Requests::UncachedInputTokens)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(Requests::Table)
                    .add_column(
                        ColumnDef::new(Requests::OutputTokens)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(Requests::Table)
                    .add_column(
                        ColumnDef::new(Requests::UsageOrigin)
                            .string()
                            .not_null()
                            .default("unknown"),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(Requests::Table)
                    .add_column(ColumnDef::new(Requests::RawUsageJson).text())
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(Requests::Table)
                    .add_column(
                        ColumnDef::new(Requests::ChargeStatus)
                            .string()
                            .not_null()
                            .default("legacy_unknown"),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(Requests::Table)
                    .add_column(ColumnDef::new(Requests::ChargeEvidenceJson).text())
                    .to_owned(),
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Requests::Table)
                    .drop_column(Requests::ChargeEvidenceJson)
                    .to_owned(),
            )
            .await?;
        for column in [
            Requests::ChargeStatus,
            Requests::RawUsageJson,
            Requests::UsageOrigin,
            Requests::OutputTokens,
            Requests::UncachedInputTokens,
        ] {
            manager
                .alter_table(
                    Table::alter()
                        .table(Requests::Table)
                        .drop_column(column)
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }
}

#[derive(DeriveIden)]
enum Requests {
    Table,
    UncachedInputTokens,
    OutputTokens,
    UsageOrigin,
    RawUsageJson,
    ChargeStatus,
    ChargeEvidenceJson,
}
