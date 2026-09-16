//! Persist the normalized router identity attached to newly settled requests.
//!
//! All three columns are nullable and existing rows remain null. A router
//! binding cannot be reconstructed from a historical model/provider pair
//! because the config and policy may have changed since that request ran.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for column in [
            ColumnDef::new(Requests::RouterId).string().to_owned(),
            ColumnDef::new(Requests::BindingDigest).string().to_owned(),
            ColumnDef::new(Requests::OriginalSelector)
                .string()
                .to_owned(),
        ] {
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
            Requests::OriginalSelector,
            Requests::BindingDigest,
            Requests::RouterId,
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
    RouterId,
    BindingDigest,
    OriginalSelector,
}
