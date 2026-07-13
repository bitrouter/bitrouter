//! Persist the adequacy learner's positive exploration state.
//!
//! `adequacy_pins` stores negative safety state. This table stores the positive
//! side: trial cadence and learned cheap-route locks, so policy rounds can keep
//! learning across daemon restarts.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(AdequacyExploration::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(AdequacyExploration::Fingerprint)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(AdequacyExploration::Observed)
                            .integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AdequacyExploration::AdequateTrials)
                            .integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AdequacyExploration::Locked)
                            .boolean()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AdequacyExploration::UpdatedAt)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AdequacyExploration::CreatedAt)
                            .string()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(AdequacyExploration::Table).to_owned())
            .await?;
        Ok(())
    }
}

#[derive(DeriveIden)]
enum AdequacyExploration {
    Table,
    Fingerprint,
    Observed,
    AdequateTrials,
    Locked,
    UpdatedAt,
    CreatedAt,
}
