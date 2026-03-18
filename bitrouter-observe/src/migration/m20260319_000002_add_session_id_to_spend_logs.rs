use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(SpendLogs::Table)
                    .add_column(uuid_null(SpendLogs::SessionId))
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_spend_logs_session_id")
                    .table(SpendLogs::Table)
                    .col(SpendLogs::SessionId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                Index::drop()
                    .name("idx_spend_logs_session_id")
                    .table(SpendLogs::Table)
                    .to_owned(),
            )
            .await?;

        manager
            .alter_table(
                Table::alter()
                    .table(SpendLogs::Table)
                    .drop_column(SpendLogs::SessionId)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum SpendLogs {
    Table,
    SessionId,
}
