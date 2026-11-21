//! Add cache-aware normalized usage and auditable charge evidence to metering.

use sea_orm_migration::prelude::*;

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

/// DDL-only component of the fresh current baseline; never accepts an old dataset.
pub(super) async fn initialize_current(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
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
