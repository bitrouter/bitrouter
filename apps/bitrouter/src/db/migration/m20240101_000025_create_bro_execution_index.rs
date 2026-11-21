//! Stable monotonic discovery positions, committed with each execution root.
use sea_orm_migration::prelude::*;

#[derive(DeriveIden)]
enum BroExecutionIndex {
    Table,
    Position,
    ExecutionId,
}
#[derive(DeriveIden)]
enum BroExecutions {
    Table,
    Id,
}

/// DDL-only component of the fresh current baseline; never accepts an old dataset.
pub(super) async fn initialize_current(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    manager
        .create_table(
            Table::create()
                .table(BroExecutionIndex::Table)
                .col(
                    ColumnDef::new(BroExecutionIndex::Position)
                        .big_integer()
                        .not_null()
                        .auto_increment()
                        .primary_key(),
                )
                .col(
                    ColumnDef::new(BroExecutionIndex::ExecutionId)
                        .string_len(128)
                        .not_null()
                        .unique_key(),
                )
                .foreign_key(
                    ForeignKey::create()
                        .from(BroExecutionIndex::Table, BroExecutionIndex::ExecutionId)
                        .to(BroExecutions::Table, BroExecutions::Id)
                        .on_delete(ForeignKeyAction::Restrict),
                )
                .to_owned(),
        )
        .await?;
    // Backfill in the database, without collecting an unbounded root list.
    let select = Query::select()
        .column(BroExecutions::Id)
        .from(BroExecutions::Table)
        .order_by(BroExecutions::Id, Order::Asc)
        .to_owned();
    let mut insert = Query::insert();
    insert
        .into_table(BroExecutionIndex::Table)
        .columns([BroExecutionIndex::ExecutionId]);
    insert
        .select_from(select)
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    manager.exec_stmt(insert.to_owned()).await
}
