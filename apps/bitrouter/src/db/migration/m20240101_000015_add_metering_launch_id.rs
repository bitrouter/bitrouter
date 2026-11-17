//! Add the per-launch attribution dimension to OSS metering (#795).
//!
//! Nullable by design: only requests whose credential `bro launch`
//! minted carry a launch, and every other caller — a direct API client, a
//! spawned sub-agent, an editor plugin — must keep recording sanely with the
//! column simply unset.
//!
//! Deliberately **not** folded into `api_key_id`. Under `skip_auth` that
//! column is the synthetic `local` caller, and overloading a field named for
//! keys with an unauthenticated tag would mislead every later reader of the
//! schema.

use sea_orm_migration::prelude::*;

#[derive(DeriveIden)]
enum Requests {
    Table,
    LaunchId,
}

/// DDL-only component of the fresh current baseline; never accepts an old dataset.
pub(super) async fn initialize_current(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    manager
        .alter_table(
            Table::alter()
                .table(Requests::Table)
                .add_column(ColumnDef::new(Requests::LaunchId).string().to_owned())
                .to_owned(),
        )
        .await?;
    // Every read of this column filters by it; without the index each
    // exit summary and status-bar refresh is a full scan of the window.
    manager
        .create_index(
            Index::create()
                .if_not_exists()
                .name("idx_requests_launch_id")
                .table(Requests::Table)
                .col(Requests::LaunchId)
                .to_owned(),
        )
        .await
}
