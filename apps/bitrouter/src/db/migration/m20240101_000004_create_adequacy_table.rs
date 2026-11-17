//! Create the adequacy ledger's `adequacy_pins` table — one row per fingerprint
//! the online learner has escalated (pinned) away from a failing downgrade.

use sea_orm_migration::prelude::*;

#[derive(DeriveIden)]
enum AdequacyPins {
    Table,
    Fingerprint,
    PinnedAtUnix,
    CreatedAt,
}

/// DDL-only component of the fresh current baseline; never accepts an old dataset.
pub(super) async fn initialize_current(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    manager
        .create_table(
            Table::create()
                .table(AdequacyPins::Table)
                .if_not_exists()
                .col(
                    ColumnDef::new(AdequacyPins::Fingerprint)
                        .string()
                        .not_null()
                        .primary_key(),
                )
                .col(
                    ColumnDef::new(AdequacyPins::PinnedAtUnix)
                        .big_integer()
                        .not_null(),
                )
                .col(ColumnDef::new(AdequacyPins::CreatedAt).string().not_null())
                .to_owned(),
        )
        .await?;
    Ok(())
}
