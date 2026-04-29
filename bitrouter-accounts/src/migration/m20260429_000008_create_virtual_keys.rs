use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(VirtualKeys::Table)
                    .if_not_exists()
                    .col(string(VirtualKeys::KeyHash).primary_key())
                    .col(text(VirtualKeys::Jwt))
                    .col(timestamp(VirtualKeys::CreatedAt))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(VirtualKeys::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum VirtualKeys {
    Table,
    KeyHash,
    Jwt,
    CreatedAt,
}
