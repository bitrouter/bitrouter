use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(ApiKeys::Table)
                    .if_not_exists()
                    .col(uuid(ApiKeys::Id).primary_key())
                    .col(uuid(ApiKeys::AccountId))
                    .col(string(ApiKeys::Name))
                    .col(string(ApiKeys::Prefix))
                    .col(string_uniq(ApiKeys::KeyHash))
                    .col(timestamp(ApiKeys::CreatedAt))
                    .col(timestamp_null(ApiKeys::ExpiresAt))
                    .col(timestamp_null(ApiKeys::RevokedAt))
                    .foreign_key(
                        ForeignKey::create()
                            .from(ApiKeys::Table, ApiKeys::AccountId)
                            .to(Accounts::Table, Accounts::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_api_keys_account_id")
                    .table(ApiKeys::Table)
                    .col(ApiKeys::AccountId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(ApiKeys::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum ApiKeys {
    Table,
    Id,
    AccountId,
    Name,
    Prefix,
    KeyHash,
    CreatedAt,
    ExpiresAt,
    RevokedAt,
}

#[derive(DeriveIden)]
enum Accounts {
    Table,
    Id,
}
