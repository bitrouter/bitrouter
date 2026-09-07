//! Owner-scoped raw evidence, transactional source cursors and frozen objects.

use sea_orm_migration::{prelude::*, sea_orm::DbBackend};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(NativeEvidenceSources::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(NativeEvidenceSources::Id)
                            .string_len(96)
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(NativeEvidenceSources::Owner)
                            .string_len(96)
                            .not_null(),
                    )
                    .col(large_json(
                        NativeEvidenceSources::DescriptorJson,
                        manager.get_database_backend(),
                    ))
                    .col(large_json(
                        NativeEvidenceSources::CursorJson,
                        manager.get_database_backend(),
                    ))
                    .col(
                        ColumnDef::new(NativeEvidenceSources::CursorDigest)
                            .string_len(96)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(NativeEvidenceSources::Revision)
                            .big_integer()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_native_sources_owner")
                    .table(NativeEvidenceSources::Table)
                    .col(NativeEvidenceSources::Owner)
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                Table::create()
                    .table(NativeEvidenceRecords::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(NativeEvidenceRecords::Id)
                            .string_len(96)
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(NativeEvidenceRecords::Owner)
                            .string_len(96)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(NativeEvidenceRecords::SourceId)
                            .string_len(96)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(NativeEvidenceRecords::Generation)
                            .string_len(96)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(NativeEvidenceRecords::SourceSequence)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(NativeEvidenceRecords::Digest)
                            .string_len(96)
                            .not_null(),
                    )
                    .col(large_json(
                        NativeEvidenceRecords::RecordJson,
                        manager.get_database_backend(),
                    ))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_native_records_source_order")
                    .table(NativeEvidenceRecords::Table)
                    .col(NativeEvidenceRecords::SourceId)
                    .col(NativeEvidenceRecords::Generation)
                    .col(NativeEvidenceRecords::SourceSequence)
                    .unique()
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                Table::create()
                    .table(NativeEvidenceObjects::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(NativeEvidenceObjects::Id)
                            .string_len(96)
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(NativeEvidenceObjects::Owner)
                            .string_len(96)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(NativeEvidenceObjects::Kind)
                            .string_len(32)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(NativeEvidenceObjects::ObjectKey)
                            .string_len(512)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(NativeEvidenceObjects::Revision)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(NativeEvidenceObjects::Digest)
                            .string_len(96)
                            .not_null(),
                    )
                    .col(large_json(
                        NativeEvidenceObjects::ObjectJson,
                        manager.get_database_backend(),
                    ))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_native_objects_owner_kind")
                    .table(NativeEvidenceObjects::Table)
                    .col(NativeEvidenceObjects::Owner)
                    .col(NativeEvidenceObjects::Kind)
                    .to_owned(),
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(NativeEvidenceObjects::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(NativeEvidenceRecords::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(NativeEvidenceSources::Table).to_owned())
            .await?;
        Ok(())
    }
}

#[derive(DeriveIden)]
enum NativeEvidenceSources {
    Table,
    Id,
    Owner,
    DescriptorJson,
    CursorJson,
    CursorDigest,
    Revision,
}
#[derive(DeriveIden)]
enum NativeEvidenceRecords {
    Table,
    Id,
    Owner,
    SourceId,
    Generation,
    SourceSequence,
    Digest,
    RecordJson,
}
#[derive(DeriveIden)]
enum NativeEvidenceObjects {
    Table,
    Id,
    Owner,
    Kind,
    ObjectKey,
    Revision,
    Digest,
    ObjectJson,
}

// MySQL TEXT is limited to 64 KiB. Native records and manifests can be larger;
// SQLite and PostgreSQL TEXT already support the application bounds.
fn large_json(column: impl IntoIden, backend: DbBackend) -> ColumnDef {
    let mut definition = ColumnDef::new(column);
    match backend {
        DbBackend::MySql => definition.custom(Alias::new("longtext")),
        DbBackend::Postgres | DbBackend::Sqlite => definition.text(),
    };
    definition.not_null().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_json_uses_backend_appropriate_large_text() {
        for backend in [DbBackend::MySql, DbBackend::Postgres, DbBackend::Sqlite] {
            let table = Table::create()
                .table(NativeEvidenceRecords::Table)
                .col(&mut large_json(NativeEvidenceRecords::RecordJson, backend))
                .col(
                    ColumnDef::new(NativeEvidenceRecords::Generation)
                        .string_len(512)
                        .not_null(),
                )
                .to_owned();
            let ddl = backend.build(&table).to_string().to_lowercase();
            assert!(ddl.contains(if backend == DbBackend::MySql {
                "longtext"
            } else {
                "text"
            }));
            if backend != DbBackend::Sqlite {
                assert!(ddl.contains("varchar(512)"));
            }
        }
    }
}
