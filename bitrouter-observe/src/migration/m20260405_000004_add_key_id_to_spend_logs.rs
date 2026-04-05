//! Adds `key_id` column to `spend_logs` for per-API-key spend tracking.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(SpendLogs::Table)
                    .add_column(ColumnDef::new(SpendLogs::KeyId).string().null())
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_spend_logs_key_id_created")
                    .table(SpendLogs::Table)
                    .col(SpendLogs::KeyId)
                    .col(SpendLogs::CreatedAt)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                Index::drop()
                    .name("idx_spend_logs_key_id_created")
                    .table(SpendLogs::Table)
                    .to_owned(),
            )
            .await?;

        manager
            .alter_table(
                Table::alter()
                    .table(SpendLogs::Table)
                    .drop_column(SpendLogs::KeyId)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum SpendLogs {
    #[sea_orm(iden = "spend_logs")]
    Table,
    KeyId,
    CreatedAt,
}
