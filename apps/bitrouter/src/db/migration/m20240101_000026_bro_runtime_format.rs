//! Existing roots retain format zero and require explicit recovery handling.
use sea_orm_migration::prelude::*;
#[derive(DeriveMigrationName)]
pub struct Migration;
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(BroExecutions::Table)
                    .add_column(
                        ColumnDef::new(BroExecutions::FormatVersion)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .to_owned(),
            )
            .await
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(BroExecutions::Table)
                    .drop_column(BroExecutions::FormatVersion)
                    .to_owned(),
            )
            .await
    }
}
#[derive(DeriveIden)]
enum BroExecutions {
    Table,
    FormatVersion,
}
