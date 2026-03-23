use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Skills::Table)
                    .if_not_exists()
                    .col(uuid(Skills::Id).primary_key())
                    .col(string_uniq(Skills::Name))
                    .col(text(Skills::Description))
                    .col(string_null(Skills::License))
                    .col(string_null(Skills::Compatibility))
                    .col(json_null(Skills::Metadata))
                    .col(json_null(Skills::AllowedTools))
                    .col(string(Skills::SourceType))
                    .col(string_null(Skills::SourceUrl))
                    .col(json(Skills::RequiredApis))
                    .col(string(Skills::InstalledBy))
                    .col(string_null(Skills::SessionId))
                    .col(timestamp(Skills::CreatedAt))
                    .col(timestamp(Skills::UpdatedAt))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Skills::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum Skills {
    Table,
    Id,
    Name,
    Description,
    License,
    Compatibility,
    Metadata,
    AllowedTools,
    SourceType,
    SourceUrl,
    RequiredApis,
    InstalledBy,
    SessionId,
    CreatedAt,
    UpdatedAt,
}
