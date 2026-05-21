//! Create the auth module's `users` and `api_keys` tables.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Users::Table)
                    .if_not_exists()
                    .col(ColumnDef::new(Users::Id).string().not_null().primary_key())
                    .col(ColumnDef::new(Users::CreatedAt).string().not_null())
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(ApiKeys::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(ApiKeys::Id)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(ApiKeys::KeyHash)
                            .string()
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(ApiKeys::UserId).string().not_null())
                    .col(ColumnDef::new(ApiKeys::SpendLimitMicroUsd).big_integer())
                    .col(ColumnDef::new(ApiKeys::RpmLimit).big_integer())
                    .col(ColumnDef::new(ApiKeys::PolicyId).string())
                    .col(ColumnDef::new(ApiKeys::ExpiresAt).string())
                    .col(
                        ColumnDef::new(ApiKeys::Active)
                            .integer()
                            .not_null()
                            .default(1),
                    )
                    .col(ColumnDef::new(ApiKeys::CreatedAt).string().not_null())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_api_keys_user")
                            .from(ApiKeys::Table, ApiKeys::UserId)
                            .to(Users::Table, Users::Id),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_api_keys_hash")
                    .table(ApiKeys::Table)
                    .col(ApiKeys::KeyHash)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(ApiKeys::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Users::Table).to_owned())
            .await?;
        Ok(())
    }
}

#[derive(DeriveIden)]
enum Users {
    Table,
    Id,
    CreatedAt,
}

#[derive(DeriveIden)]
enum ApiKeys {
    Table,
    Id,
    KeyHash,
    UserId,
    SpendLimitMicroUsd,
    RpmLimit,
    PolicyId,
    ExpiresAt,
    Active,
    CreatedAt,
}
