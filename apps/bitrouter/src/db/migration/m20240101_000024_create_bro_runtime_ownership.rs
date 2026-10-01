//! Runtime-wide write fence plus retained owner/clean-stop proofs.
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(BroRuntimeOwnership::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(BroRuntimeOwnership::Id)
                            .integer()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(BroRuntimeOwnership::Generation)
                            .big_integer()
                            .not_null(),
                    )
                    .col(ColumnDef::new(BroRuntimeOwnership::ServerInstanceId).string_len(128))
                    .col(ColumnDef::new(BroRuntimeOwnership::StoppedAtMs).big_integer())
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                Table::create()
                    .table(BroRuntimeOwners::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(BroRuntimeOwners::Generation)
                            .big_integer()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(BroRuntimeOwners::ServerInstanceId)
                            .string_len(128)
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(BroRuntimeOwners::StoppedAtMs).big_integer())
                    .to_owned(),
            )
            .await?;
        let mut insert = Query::insert();
        insert
            .into_table(BroRuntimeOwnership::Table)
            .columns([BroRuntimeOwnership::Id, BroRuntimeOwnership::Generation]);
        insert
            .values([1.into(), 0i64.into()])
            .map_err(|error| DbErr::Custom(error.to_string()))?;
        manager.exec_stmt(insert.to_owned()).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(BroRuntimeOwners::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(BroRuntimeOwnership::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum BroRuntimeOwnership {
    Table,
    Id,
    Generation,
    ServerInstanceId,
    StoppedAtMs,
}

#[derive(DeriveIden)]
enum BroRuntimeOwners {
    Table,
    Generation,
    ServerInstanceId,
    StoppedAtMs,
}
