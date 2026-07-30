//! Bind eval exchange rows to the authenticated owning user.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
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

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for (table, index) in [
            ("eval_snapshots", "idx_eval_snapshots_owner"),
            ("eval_results", "idx_eval_results_owner_idempotency"),
            ("eval_results", "idx_eval_results_owner"),
            ("eval_subjects", "idx_eval_subjects_owner"),
        ] {
            manager
                .drop_index(
                    Index::drop()
                        .if_exists()
                        .name(index)
                        .table(Alias::new(table))
                        .to_owned(),
                )
                .await?;
        }
        for table in ["eval_snapshots", "eval_results", "eval_subjects"] {
            manager
                .alter_table(
                    Table::alter()
                        .table(Alias::new(table))
                        .drop_column(Alias::new("owner_user_id"))
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }
}
