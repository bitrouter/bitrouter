use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // 1. Add master_pubkey column to accounts.
        manager
            .alter_table(
                Table::alter()
                    .table(Accounts::Table)
                    .add_column(string_null(Accounts::MasterPubkey))
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_accounts_master_pubkey")
                    .table(Accounts::Table)
                    .col(Accounts::MasterPubkey)
                    .unique()
                    .to_owned(),
            )
            .await?;

        // 2. Make accounts.name nullable (auto-created accounts have generated names).
        manager
            .alter_table(
                Table::alter()
                    .table(Accounts::Table)
                    .modify_column(ColumnDef::new(Accounts::Name).string().null())
                    .to_owned(),
            )
            .await?;

        // 3. Create rotated_pubkeys table for key rotation history.
        manager
            .create_table(
                Table::create()
                    .table(RotatedPubkeys::Table)
                    .if_not_exists()
                    .col(uuid(RotatedPubkeys::Id).primary_key())
                    .col(uuid(RotatedPubkeys::AccountId))
                    .col(string_uniq(RotatedPubkeys::Pubkey))
                    .col(timestamp(RotatedPubkeys::RotatedAt))
                    .foreign_key(
                        ForeignKey::create()
                            .from(RotatedPubkeys::Table, RotatedPubkeys::AccountId)
                            .to(Accounts::Table, Accounts::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // 4. Drop api_keys table.
        manager
            .drop_table(Table::drop().table(ApiKeys::Table).to_owned())
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Reverse: recreate api_keys, drop rotated_pubkeys, remove master_pubkey.
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
            .drop_table(Table::drop().table(RotatedPubkeys::Table).to_owned())
            .await?;

        manager
            .drop_index(
                Index::drop()
                    .name("idx_accounts_master_pubkey")
                    .table(Accounts::Table)
                    .to_owned(),
            )
            .await?;

        manager
            .alter_table(
                Table::alter()
                    .table(Accounts::Table)
                    .drop_column(Accounts::MasterPubkey)
                    .to_owned(),
            )
            .await?;

        manager
            .alter_table(
                Table::alter()
                    .table(Accounts::Table)
                    .modify_column(ColumnDef::new(Accounts::Name).string().not_null())
                    .to_owned(),
            )
            .await?;

        Ok(())
    }
}

#[derive(DeriveIden)]
enum Accounts {
    Table,
    Id,
    Name,
    MasterPubkey,
}

#[derive(DeriveIden)]
enum RotatedPubkeys {
    Table,
    Id,
    AccountId,
    Pubkey,
    RotatedAt,
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
