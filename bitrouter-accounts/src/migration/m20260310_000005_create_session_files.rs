use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(SessionFiles::Table)
                    .if_not_exists()
                    .col(uuid(SessionFiles::Id).primary_key())
                    .col(uuid(SessionFiles::SessionId))
                    .col(uuid_null(SessionFiles::MessageId))
                    .col(string_uniq(SessionFiles::BlobKey))
                    .col(string_null(SessionFiles::Filename))
                    .col(string(SessionFiles::MediaType))
                    .col(big_integer(SessionFiles::SizeBytes))
                    .col(timestamp(SessionFiles::CreatedAt))
                    .foreign_key(
                        ForeignKey::create()
                            .from(SessionFiles::Table, SessionFiles::SessionId)
                            .to(Sessions::Table, Sessions::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(SessionFiles::Table, SessionFiles::MessageId)
                            .to(Messages::Table, Messages::Id)
                            .on_delete(ForeignKeyAction::SetNull),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_session_files_session_id")
                    .table(SessionFiles::Table)
                    .col(SessionFiles::SessionId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(SessionFiles::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum SessionFiles {
    Table,
    Id,
    SessionId,
    MessageId,
    BlobKey,
    Filename,
    MediaType,
    SizeBytes,
    CreatedAt,
}

#[derive(DeriveIden)]
enum Sessions {
    Table,
    Id,
}

#[derive(DeriveIden)]
enum Messages {
    Table,
    Id,
}
