use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Messages::Table)
                    .if_not_exists()
                    .col(uuid(Messages::Id).primary_key())
                    .col(uuid(Messages::SessionId))
                    .col(integer(Messages::Position))
                    .col(string(Messages::Role))
                    .col(text(Messages::Payload))
                    .col(timestamp(Messages::CreatedAt))
                    .foreign_key(
                        ForeignKey::create()
                            .from(Messages::Table, Messages::SessionId)
                            .to(Sessions::Table, Sessions::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_messages_session_position")
                    .table(Messages::Table)
                    .col(Messages::SessionId)
                    .col(Messages::Position)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Messages::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum Messages {
    Table,
    Id,
    SessionId,
    Position,
    Role,
    Payload,
    CreatedAt,
}

#[derive(DeriveIden)]
enum Sessions {
    Table,
    Id,
}
