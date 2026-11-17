//! Add request-scoped authoritative reconciliation state to OSS metering.

use sea_orm_migration::prelude::*;

#[derive(DeriveIden)]
enum Requests {
    Table,
    ReconciliationStatus,
    ReconciliationAttempts,
    ReconciliationLastError,
    ReconciliationLastAttemptAt,
    AuthoritativeSettledAt,
    AuthoritativeReceiptJson,
}

/// DDL-only component of the fresh current baseline; never accepts an old dataset.
pub(super) async fn initialize_current(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let columns = [
        ColumnDef::new(Requests::ReconciliationStatus)
            .string()
            .not_null()
            .default("not_applicable")
            .to_owned(),
        ColumnDef::new(Requests::ReconciliationAttempts)
            .integer()
            .not_null()
            .default(0)
            .to_owned(),
        ColumnDef::new(Requests::ReconciliationLastError)
            .text()
            .to_owned(),
        ColumnDef::new(Requests::ReconciliationLastAttemptAt)
            .string()
            .to_owned(),
        ColumnDef::new(Requests::AuthoritativeSettledAt)
            .string()
            .to_owned(),
        ColumnDef::new(Requests::AuthoritativeReceiptJson)
            .text()
            .to_owned(),
    ];
    for column in columns {
        manager
            .alter_table(
                Table::alter()
                    .table(Requests::Table)
                    .add_column(column)
                    .to_owned(),
            )
            .await?;
    }
    Ok(())
}
