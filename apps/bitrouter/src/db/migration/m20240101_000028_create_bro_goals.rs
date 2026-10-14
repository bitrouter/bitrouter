//! Goal heads and immutable input/control receipts have one transaction boundary.
use sea_orm::DatabaseBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(BroGoals::Table)
                    .col(
                        ColumnDef::new(BroGoals::Position)
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(BroGoals::Id)
                            .string_len(128)
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(BroGoals::Owner).string_len(128).not_null())
                    .col(ColumnDef::new(BroGoals::Revision).big_integer().not_null())
                    .col(payload(BroGoals::Payload, manager.get_database_backend()))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("bro_goals_owner_position")
                    .table(BroGoals::Table)
                    .col(BroGoals::Owner)
                    .col(BroGoals::Position)
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                Table::create()
                    .table(BroGoalOperations::Table)
                    .col(
                        ColumnDef::new(BroGoalOperations::Owner)
                            .string_len(128)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(BroGoalOperations::Id)
                            .string_len(128)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(BroGoalOperations::GoalId)
                            .string_len(128)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(BroGoalOperations::Revision)
                            .big_integer()
                            .not_null(),
                    )
                    .col(payload(
                        BroGoalOperations::Payload,
                        manager.get_database_backend(),
                    ))
                    .primary_key(
                        Index::create()
                            .col(BroGoalOperations::Owner)
                            .col(BroGoalOperations::Id),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(BroGoalOperations::Table, BroGoalOperations::GoalId)
                            .to(BroGoals::Table, BroGoals::Id)
                            .on_delete(ForeignKeyAction::Restrict),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("bro_goal_history_order")
                    .unique()
                    .table(BroGoalOperations::Table)
                    .col(BroGoalOperations::GoalId)
                    .col(BroGoalOperations::Revision)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(BroGoalOperations::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(BroGoals::Table).to_owned())
            .await
    }
}

fn payload(column: impl IntoIden, backend: DatabaseBackend) -> ColumnDef {
    let mut definition = ColumnDef::new(column);
    if backend == DatabaseBackend::MySql {
        definition.custom(Alias::new("longblob"));
    } else {
        definition.blob();
    }
    definition.not_null();
    definition
}

#[derive(DeriveIden)]
enum BroGoals {
    Table,
    Position,
    Id,
    Owner,
    Revision,
    Payload,
}

#[derive(DeriveIden)]
enum BroGoalOperations {
    Table,
    Owner,
    Id,
    GoalId,
    Revision,
    Payload,
}
