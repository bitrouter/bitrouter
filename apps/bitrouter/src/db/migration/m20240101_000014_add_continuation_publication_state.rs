//! Make continuation publication a durable two-phase state transition.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(ProviderContinuations::Table)
                    .add_column(
                        ColumnDef::new(ProviderContinuations::PublicationState)
                            .string()
                            .not_null()
                            .default("active"),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(ProviderContinuations::Table)
                    .drop_column(ProviderContinuations::PublicationState)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum ProviderContinuations {
    Table,
    PublicationState,
}
