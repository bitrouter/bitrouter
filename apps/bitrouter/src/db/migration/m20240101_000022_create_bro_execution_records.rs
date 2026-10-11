//! Dedicated BRO execution facts; metering trajectories are not recovery logs.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(BroExecutions::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(BroExecutions::Id)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(BroExecutions::Version)
                            .big_integer()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                Table::create()
                    .table(BroExecutionRecords::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(BroExecutionRecords::ExecutionId)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(BroExecutionRecords::Sequence)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(BroExecutionRecords::Payload)
                            .text()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(BroExecutionRecords::ExecutionId)
                            .col(BroExecutionRecords::Sequence),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(BroExecutionRecords::Table, BroExecutionRecords::ExecutionId)
                            .to(BroExecutions::Table, BroExecutions::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(BroExecutionRecords::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(BroExecutions::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum BroExecutions {
    Table,
    Id,
    Version,
}

#[derive(DeriveIden)]
enum BroExecutionRecords {
    Table,
    ExecutionId,
    Sequence,
    Payload,
}
