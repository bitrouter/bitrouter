//! Portable persistence for immutable ACP prefixes and replaceable evaluations.
use sea_orm::DatabaseBackend;
use sea_orm_migration::prelude::*;

fn content_column(name: &str, backend: DatabaseBackend) -> ColumnDef {
    let mut column = ColumnDef::new(Alias::new(name));
    // Long sessions and individual tool results can exceed MySQL TEXT capacity.
    // https://dev.mysql.com/doc/refman/8.4/en/blob.html
    if backend == DatabaseBackend::MySql {
        column.custom(Alias::new("LONGTEXT"));
    } else {
        column.text();
    }
    column.not_null().to_owned()
}

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        if backend == DatabaseBackend::MySql {
            manager
                .alter_table(
                    Table::alter()
                        .table(Alias::new("acp_capture_events"))
                        .modify_column(content_column("event_json", backend))
                        .to_owned(),
                )
                .await?;
        }
        let definitions = [
            (
                "acp_checkpoints",
                vec!["checkpoint_id"],
                vec!["checkpoint_id", "session_key"],
                vec!["manifest_json"],
            ),
            (
                "acp_checkpoint_members",
                vec!["checkpoint_id", "session_key"],
                vec!["checkpoint_id", "session_key"],
                vec![],
            ),
            (
                "acp_resource_observations",
                vec!["observation_id"],
                vec!["observation_id", "checkpoint_id"],
                vec!["observed_at", "observation_json"],
            ),
            (
                "acp_assessment_revisions",
                vec!["revision_id"],
                vec!["revision_id", "session_key", "checkpoint_id"],
                vec!["created_at", "revision_json"],
            ),
            (
                "acp_effective_assessments",
                vec!["session_key"],
                vec!["session_key"],
                vec![],
            ),
        ];
        for (table_name, keys, ids, texts) in definitions {
            let mut table = Table::create();
            table.table(Alias::new(table_name));
            for id in ids {
                table.col(ColumnDef::new(Alias::new(id)).string_len(64).not_null());
            }
            for field in texts {
                if field.ends_with("_json") {
                    table.col(content_column(field, backend));
                } else {
                    table.col(ColumnDef::new(Alias::new(field)).text().not_null());
                }
            }
            if table_name == "acp_checkpoints" {
                table.col(
                    ColumnDef::new(Alias::new("watermark"))
                        .big_integer()
                        .not_null(),
                );
                table.col(ColumnDef::new(Alias::new("deleted")).boolean().not_null());
            }
            if table_name == "acp_effective_assessments" {
                table.col(ColumnDef::new(Alias::new("revision_id")).string_len(64));
            }
            if table_name == "acp_resource_observations" {
                table.col(
                    ColumnDef::new(Alias::new("revision"))
                        .big_integer()
                        .not_null(),
                );
            }
            let mut primary = Index::create();
            for key in keys {
                primary.col(Alias::new(key));
            }
            table.primary_key(&mut primary);
            manager.create_table(table.to_owned()).await?;
        }
        for (name, table, column) in [
            (
                "idx_acp_checkpoints_session",
                "acp_checkpoints",
                "session_key",
            ),
            (
                "idx_acp_members_session",
                "acp_checkpoint_members",
                "session_key",
            ),
            (
                "idx_acp_resources_checkpoint",
                "acp_resource_observations",
                "checkpoint_id",
            ),
            (
                "idx_acp_revisions_session",
                "acp_assessment_revisions",
                "session_key",
            ),
            (
                "idx_acp_revisions_checkpoint",
                "acp_assessment_revisions",
                "checkpoint_id",
            ),
        ] {
            manager
                .create_index(
                    Index::create()
                        .name(name)
                        .table(Alias::new(table))
                        .col(Alias::new(column))
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Keep the widened capture column: shrinking it could truncate content.
        for table in [
            "acp_effective_assessments",
            "acp_assessment_revisions",
            "acp_resource_observations",
            "acp_checkpoint_members",
            "acp_checkpoints",
        ] {
            manager
                .drop_table(Table::drop().table(Alias::new(table)).to_owned())
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_columns_support_large_manifests_on_every_backend() {
        for (backend, expected) in [
            (DatabaseBackend::MySql, "LONGTEXT"),
            (DatabaseBackend::Postgres, "text"),
            (DatabaseBackend::Sqlite, "text"),
        ] {
            let table = Table::create()
                .table(Alias::new("large_content"))
                .col(content_column("manifest_json", backend))
                .to_owned();
            let sql = match backend {
                DatabaseBackend::MySql => table.to_string(MysqlQueryBuilder),
                DatabaseBackend::Postgres => table.to_string(PostgresQueryBuilder),
                DatabaseBackend::Sqlite => table.to_string(SqliteQueryBuilder),
            };
            assert!(sql.contains(expected), "{sql}");
        }
    }
}
