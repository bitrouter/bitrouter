//! Add request-scoped authoritative reconciliation state to OSS metering.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
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

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for column in [
            Requests::AuthoritativeReceiptJson,
            Requests::AuthoritativeSettledAt,
            Requests::ReconciliationLastAttemptAt,
            Requests::ReconciliationLastError,
            Requests::ReconciliationAttempts,
            Requests::ReconciliationStatus,
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
    ReconciliationStatus,
    ReconciliationAttempts,
    ReconciliationLastError,
    ReconciliationLastAttemptAt,
    AuthoritativeSettledAt,
    AuthoritativeReceiptJson,
}
