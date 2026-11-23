//! Persist the normalized router identity attached to newly settled requests.
//!
//! All three columns are nullable and existing rows remain null. A router
//! binding cannot be reconstructed from a historical model/provider pair
//! because the config and policy may have changed since that request ran.

use sea_orm_migration::prelude::*;

#[derive(DeriveIden)]
enum Requests {
    Table,
    RouterId,
    BindingDigest,
    OriginalSelector,
}

/// DDL-only component of the fresh current baseline; never accepts an old dataset.
pub(super) async fn initialize_current(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
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
