//! Bind eval exchange rows to the authenticated owning user.

use sea_orm_migration::prelude::*;

/// DDL-only component of the fresh current baseline; never accepts an old dataset.
pub(super) async fn initialize_current(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    for table in ["eval_subjects", "eval_results", "eval_snapshots"] {
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new(table))
                    .add_column(
                        ColumnDef::new(Alias::new("owner_user_id"))
                            .string()
                            .not_null()
                            .default("local"),
                    )
                    .to_owned(),
            )
            .await?;
    }
    manager
        .create_index(
            Index::create()
                .if_not_exists()
                .name("idx_eval_subjects_owner")
                .table(Alias::new("eval_subjects"))
                .col(Alias::new("owner_user_id"))
                .to_owned(),
        )
        .await?;
    manager
        .create_index(
            Index::create()
                .if_not_exists()
                .name("idx_eval_results_owner")
                .table(Alias::new("eval_results"))
                .col(Alias::new("owner_user_id"))
                .to_owned(),
        )
        .await?;
    manager
        .create_index(
            Index::create()
                .if_not_exists()
                .name("idx_eval_results_owner_idempotency")
                .table(Alias::new("eval_results"))
                .col(Alias::new("owner_user_id"))
                .col(Alias::new("idempotency_key"))
                .unique()
                .to_owned(),
        )
        .await?;
    manager
        .create_index(
            Index::create()
                .if_not_exists()
                .name("idx_eval_snapshots_owner")
                .table(Alias::new("eval_snapshots"))
                .col(Alias::new("owner_user_id"))
                .to_owned(),
        )
        .await?;
    Ok(())
}
