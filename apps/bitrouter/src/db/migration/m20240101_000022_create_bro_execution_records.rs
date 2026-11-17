//! Dedicated BRO execution facts; metering trajectories are not recovery logs.

use sea_orm_migration::prelude::*;

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

/// DDL-only component of the fresh current baseline; never accepts an old dataset.
pub(super) async fn initialize_current(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
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
