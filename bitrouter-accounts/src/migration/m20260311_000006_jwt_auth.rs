use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // 1. Add caip10_identity column to accounts (stores CAIP-10 identifier
        //    for JWT authentication, e.g. "solana:5eykt...:BASE58_KEY").
        manager
            .alter_table(
                Table::alter()
                    .table(Accounts::Table)
                    .add_column(string_null(Accounts::Caip10Identity))
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_accounts_caip10_identity")
                    .table(Accounts::Table)
                    .col(Accounts::Caip10Identity)
                    .unique()
                    .to_owned(),
            )
            .await?;

        // 2. Create rotated_pubkeys table for key rotation history.
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

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(RotatedPubkeys::Table).to_owned())
            .await?;

        manager
            .drop_index(
                Index::drop()
                    .name("idx_accounts_caip10_identity")
                    .table(Accounts::Table)
                    .to_owned(),
            )
            .await?;

        manager
            .alter_table(
                Table::alter()
                    .table(Accounts::Table)
                    .drop_column(Accounts::Caip10Identity)
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
    Caip10Identity,
}

#[derive(DeriveIden)]
enum RotatedPubkeys {
    Table,
    Id,
    AccountId,
    Pubkey,
    RotatedAt,
}
