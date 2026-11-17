//! Existing roots retain format zero and require explicit recovery handling.
use sea_orm_migration::prelude::*;
#[derive(DeriveIden)]
enum BroExecutions {
    Table,
    FormatVersion,
}

/// DDL-only component of the fresh current baseline; never accepts an old dataset.
pub(super) async fn initialize_current(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
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
