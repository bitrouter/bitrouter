//! Indexed lifecycle facts, with immutable references to their raw evidence.

use sea_orm_migration::{prelude::*, sea_orm::DbBackend};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let mut table = Table::create();
        table.table(NativeExecutionFacts::Table).if_not_exists();
        for column in [
            NativeExecutionFacts::Id,
            NativeExecutionFacts::Owner,
            NativeExecutionFacts::Namespace,
            NativeExecutionFacts::ParserVersion,
            NativeExecutionFacts::RecordId,
            NativeExecutionFacts::Digest,
        ] {
            let mut definition = ColumnDef::new(column);
            definition.string_len(96).not_null();
            if matches!(column, NativeExecutionFacts::Id) {
                definition.primary_key();
            }
            table.col(&mut definition);
        }
        for column in [
            NativeExecutionFacts::NodeId,
            NativeExecutionFacts::RelatedNodeId,
        ] {
            table.col(ColumnDef::new(column).string_len(96));
        }
        let mut json = ColumnDef::new(NativeExecutionFacts::FactJson);
        if manager.get_database_backend() == DbBackend::MySql {
            json.custom(Alias::new("LONGTEXT"));
        } else {
            json.text();
        }
        table.col(json.not_null());
        manager.create_table(table.to_owned()).await?;
        for (name, column) in [
            ("idx_native_facts_node", NativeExecutionFacts::NodeId),
            (
                "idx_native_facts_related",
                NativeExecutionFacts::RelatedNodeId,
            ),
        ] {
            manager
                .create_index(
                    Index::create()
                        .if_not_exists()
                        .name(name)
                        .table(NativeExecutionFacts::Table)
                        .col(NativeExecutionFacts::Owner)
                        .col(NativeExecutionFacts::Namespace)
                        .col(NativeExecutionFacts::ParserVersion)
                        .col(column)
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(NativeExecutionFacts::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await
    }
}

#[derive(Clone, Copy, DeriveIden)]
enum NativeExecutionFacts {
    Table,
    Id,
    Owner,
    Namespace,
    ParserVersion,
    RecordId,
    Digest,
    NodeId,
    RelatedNodeId,
    FactJson,
}
