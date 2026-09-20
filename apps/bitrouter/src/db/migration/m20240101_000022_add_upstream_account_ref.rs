//! Persist the redaction-safe credential principal proven for settled requests.
//!
//! Existing rows remain null. Provider names, account labels, and inbound API
//! keys are not sufficient evidence to reconstruct this value historically.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Requests::Table)
                    .add_column(ColumnDef::new(Requests::UpstreamAccountRef).string())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Requests::Table)
                    .drop_column(Requests::UpstreamAccountRef)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum Requests {
    Table,
    UpstreamAccountRef,
}
