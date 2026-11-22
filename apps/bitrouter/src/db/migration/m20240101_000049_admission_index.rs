//! Persistent witnesses and resumable authenticated admission backfill.

use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use sea_orm_migration::prelude::*;

use super::m20240101_000028_resource_tenancy::identity;

#[derive(DeriveMigrationName)]
pub struct Migration;

pub(super) fn head_index() -> IndexCreateStatement {
    Index::create()
        .name("tenancy_resource_head_lookup")
        .table(Alias::new("tenancy_resource_lifecycle"))
        .col(Alias::new("owner_kind"))
        .col(Alias::new("owner_id"))
        .col(Alias::new("resource_kind"))
        .col(Alias::new("resource_id"))
        .col(Alias::new("is_head"))
        .to_owned()
}

// MySQL commits DDL independently of the migration ledger. A restart may reuse
// only our exact EMPTY objects; no application writer runs before the fence.
async fn resume_mysql_table(manager: &SchemaManager<'_>, table: &str) -> Result<(), DbErr> {
    let db = manager.get_connection();
    let incompatible = || DbErr::Custom("incompatible partial admission index migration".into());
    let columns = db.query_all(Statement::from_sql_and_values(DatabaseBackend::MySql,
        "SELECT COLUMN_NAME AS name,DATA_TYPE AS kind,CAST(CHARACTER_MAXIMUM_LENGTH AS CHAR) AS width,IS_NULLABLE AS nullable,COLUMN_DEFAULT AS default_value,COLLATION_NAME AS collation_name,EXTRA AS extra,GENERATION_EXPRESSION AS generated_value FROM information_schema.columns WHERE TABLE_SCHEMA=DATABASE() AND TABLE_NAME=? ORDER BY ORDINAL_POSITION LIMIT 7",
        [table.into()])).await?;
    let expected = if table == "tenancy_admission_index" {
        vec![
            ("index_kind", "varchar", Some("32")),
            ("owner_kind", "varchar", Some("32")),
            ("owner_id", "varchar", Some("128")),
            ("resource_kind", "varchar", Some("32")),
            ("resource_id", "varchar", Some("128")),
            ("reference_json", "text", None),
        ]
    } else {
        vec![
            ("singleton", "int", None),
            ("installation_id", "varchar", Some("128")),
            ("authority_revision", "bigint", None),
            ("cursor_json", "text", None),
            ("complete", "tinyint", None),
        ]
    };
    if columns.len() != expected.len() {
        return Err(incompatible());
    }
    for (row, (name, kind, width)) in columns.iter().zip(expected) {
        if row.try_get::<String>("", "name")? != name
            || row.try_get::<String>("", "kind")? != kind
            || row.try_get::<String>("", "nullable")? != "NO"
            || row
                .try_get::<Option<String>>("", "default_value")?
                .is_some()
            || !row.try_get::<String>("", "extra")?.is_empty()
            || !row.try_get::<String>("", "generated_value")?.is_empty()
            || (width.is_some() && row.try_get::<Option<String>>("", "width")?.as_deref() != width)
            || (kind == "varchar"
                && row
                    .try_get::<Option<String>>("", "collation_name")?
                    .as_deref()
                    != Some("utf8mb4_0900_bin"))
        {
            return Err(incompatible());
        }
    }
    let engine = db.query_one(Statement::from_sql_and_values(DatabaseBackend::MySql,
        "SELECT ENGINE AS engine FROM information_schema.tables WHERE TABLE_SCHEMA=DATABASE() AND TABLE_NAME=?", [table.into()])).await?
        .ok_or_else(incompatible)?;
    if engine.try_get::<String>("", "engine")? != "InnoDB" {
        return Err(incompatible());
    }
    let primary: &[&str] = if table == "tenancy_admission_index" {
        &[
            "index_kind",
            "owner_kind",
            "owner_id",
            "resource_kind",
            "resource_id",
        ]
    } else {
        &["singleton"]
    };
    let indexes = db.query_all(Statement::from_sql_and_values(DatabaseBackend::MySql,
        "SELECT INDEX_NAME AS name,COLUMN_NAME AS column_name,SUB_PART AS prefix,NON_UNIQUE AS non_unique,COLLATION AS ordering FROM information_schema.statistics WHERE TABLE_SCHEMA=DATABASE() AND TABLE_NAME=? ORDER BY INDEX_NAME,SEQ_IN_INDEX LIMIT 7", [table.into()])).await?;
    if indexes.len() != primary.len() {
        return Err(incompatible());
    }
    for (row, column) in indexes.iter().zip(primary) {
        if row.try_get::<String>("", "name")? != "PRIMARY"
            || row.try_get::<String>("", "column_name")? != *column
            || row.try_get::<Option<i64>>("", "prefix")?.is_some()
            || row.try_get::<i64>("", "non_unique")? != 0
            || row.try_get::<String>("", "ordering")? != "A"
        {
            return Err(incompatible());
        }
    }
    if db
        .query_one(Statement::from_string(
            DatabaseBackend::MySql,
            format!("SELECT 1 FROM `{table}` LIMIT 1"),
        ))
        .await?
        .is_some()
    {
        return Err(incompatible());
    }
    Ok(())
}

pub(super) fn definitions(backend: DatabaseBackend) -> [TableCreateStatement; 2] {
    let mut index = Table::create();
    index.table(Alias::new("tenancy_admission_index"));
    let mut primary = Index::create();
    for (name, width) in [
        ("index_kind", 32),
        ("owner_kind", 32),
        ("owner_id", 128),
        ("resource_kind", 32),
        ("resource_id", 128),
    ] {
        index.col(identity(backend, name, width, false));
        primary.col(Alias::new(name));
    }
    index.primary_key(&mut primary).col(
        ColumnDef::new(Alias::new("reference_json"))
            .text()
            .not_null(),
    );
    let mut progress = Table::create();
    progress
        .table(Alias::new("tenancy_admission_backfill"))
        .col(
            ColumnDef::new(Alias::new("singleton"))
                .integer()
                .not_null()
                .primary_key(),
        )
        .col(identity(backend, "installation_id", 128, false))
        .col(
            ColumnDef::new(Alias::new("authority_revision"))
                .big_integer()
                .not_null(),
        )
        .col(ColumnDef::new(Alias::new("cursor_json")).text().not_null())
        .col(ColumnDef::new(Alias::new("complete")).boolean().not_null());
    if backend == DatabaseBackend::MySql {
        index.engine("InnoDB");
        progress.engine("InnoDB");
    }
    [index.to_owned(), progress.to_owned()]
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for (table, definition) in ["tenancy_admission_index", "tenancy_admission_backfill"]
            .into_iter()
            .zip(definitions(manager.get_database_backend()))
        {
            if manager.get_database_backend() == DatabaseBackend::MySql
                && manager.has_table(table).await?
            {
                resume_mysql_table(manager, table).await?;
            } else {
                manager.create_table(definition).await?;
            }
        }
        // An equality lookup for the head must not scan a key's entire history.
        if manager.get_database_backend() == DatabaseBackend::MySql
            && manager
                .has_index("tenancy_resource_lifecycle", "tenancy_resource_head_lookup")
                .await?
        {
            let rows = manager.get_connection().query_all(Statement::from_string(DatabaseBackend::MySql,
                "SELECT COLUMN_NAME AS name,SUB_PART AS prefix,NON_UNIQUE AS non_unique,COLLATION AS ordering FROM information_schema.statistics WHERE TABLE_SCHEMA=DATABASE() AND TABLE_NAME='tenancy_resource_lifecycle' AND INDEX_NAME='tenancy_resource_head_lookup' ORDER BY SEQ_IN_INDEX LIMIT 6")).await?;
            let columns = [
                "owner_kind",
                "owner_id",
                "resource_kind",
                "resource_id",
                "is_head",
            ];
            if rows.len() != columns.len() {
                return Err(DbErr::Custom("incompatible admission head index".into()));
            }
            for (row, name) in rows.iter().zip(columns) {
                if row.try_get::<String>("", "name")? != name
                    || row.try_get::<Option<i64>>("", "prefix")?.is_some()
                    || row.try_get::<i64>("", "non_unique")? != 1
                    || row.try_get::<String>("", "ordering")? != "A"
                {
                    return Err(DbErr::Custom("incompatible admission head index".into()));
                }
            }
            return Ok(());
        }
        manager.create_index(head_index()).await?;
        Ok(())
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "admission index requires verified-backup rollback".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, ensure};

    #[test]
    fn admission_index_ddl_keeps_qualified_keys_byte_exact_on_every_backend() -> Result<()> {
        for backend in [
            DatabaseBackend::Sqlite,
            DatabaseBackend::Postgres,
            DatabaseBackend::MySql,
        ] {
            let [index, progress] = definitions(backend);
            let index = backend.build(&index).to_string();
            let progress = backend.build(&progress).to_string();
            let collation = match backend {
                DatabaseBackend::Sqlite => "COLLATE BINARY",
                DatabaseBackend::Postgres => "COLLATE \"C\"",
                DatabaseBackend::MySql => "COLLATE utf8mb4_0900_bin",
            };
            ensure!(index.matches(collation).count() == 5);
            ensure!(progress.matches(collation).count() == 1);
            ensure!(index.contains("PRIMARY KEY") && index.contains("reference_json"));
            ensure!(progress.contains("cursor_json") && progress.contains("authority_revision"));
        }
        Ok(())
    }
}
