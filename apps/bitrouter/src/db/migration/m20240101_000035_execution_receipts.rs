//! Immutable tenant execution receipts. This table is never a dispatch log.
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, Value};
mod catalog;
#[cfg(test)]
mod tests;
use super::m20240101_000028_resource_tenancy::{identity, payload};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;
const TABLE: &str = "tenancy_execution_receipts";
const INDEX: &str = "idx_tenancy_receipts_correlation";
const IDS: [&str; 5] = [
    "installation_id",
    "tenant_id",
    "execution_id",
    "principal_id",
    "correlation_id",
];

fn definition(backend: DatabaseBackend) -> TableCreateStatement {
    let mut table = Table::create();
    table.table(Alias::new(TABLE));
    if backend == DatabaseBackend::MySql {
        table.engine("InnoDB");
    }
    for id in IDS {
        table.col(identity(backend, id, 128, false));
    }
    table
        .col(payload(backend, "receipt_json"))
        .col(identity(backend, "content_digest", 64, false))
        .col(payload(backend, "disposition_json"))
        .col(
            ColumnDef::new(Alias::new("byte_count"))
                .big_integer()
                .not_null(),
        )
        .col(
            ColumnDef::new(Alias::new("unknown_outcome"))
                .boolean()
                .not_null(),
        )
        .col(identity(backend, "recorded_at", 20, false))
        .primary_key(
            Index::create()
                .col(Alias::new("installation_id"))
                .col(Alias::new("tenant_id"))
                .col(Alias::new("execution_id")),
        )
        .to_owned()
}

fn correlation() -> IndexCreateStatement {
    Index::create()
        .name(INDEX)
        .table(Alias::new(TABLE))
        .col(Alias::new("installation_id"))
        .col(Alias::new("tenant_id"))
        .col(Alias::new("correlation_id"))
        .col(Alias::new("execution_id"))
        .to_owned()
}

fn incompatible() -> DbErr {
    DbErr::Custom("migration035 incompatible receipt storage".into())
}

async fn validate_table(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let backend = manager.get_database_backend();
    let db = manager.get_connection();
    if backend == DatabaseBackend::Sqlite {
        let rows = db.query_all(Statement::from_string(backend,
            "SELECT type, sql FROM sqlite_master WHERE name='tenancy_execution_receipts' LIMIT 2".to_owned())).await?;
        let row = rows.first().ok_or_else(incompatible)?;
        if rows.len() != 1
            || row.try_get::<String>("", "type")? != "table"
            || row.try_get::<String>("", "sql")?
                != definition(backend).to_string(SqliteQueryBuilder)
        {
            return Err(incompatible());
        }
        return Ok(());
    }
    let sql = match backend {
        DatabaseBackend::Postgres => {
            "SELECT column_name AS name, data_type AS kind, character_maximum_length::text AS width, is_nullable AS nullable, column_default AS default_value, collation_name, collation_schema, is_generated AS generated_value, is_identity AS identity_column FROM information_schema.columns WHERE table_schema=current_schema() AND table_name='tenancy_execution_receipts' ORDER BY ordinal_position LIMIT 12"
        }
        DatabaseBackend::MySql => {
            "SELECT COLUMN_NAME AS name, DATA_TYPE AS kind, COLUMN_TYPE AS full_kind, CAST(CHARACTER_MAXIMUM_LENGTH AS CHAR) AS width, IS_NULLABLE AS nullable, COLUMN_DEFAULT AS default_value, COLLATION_NAME AS collation_name, CHARACTER_SET_NAME AS charset_name, EXTRA AS extra, GENERATION_EXPRESSION AS generated_value FROM information_schema.columns WHERE table_schema=DATABASE() AND table_name='tenancy_execution_receipts' ORDER BY ordinal_position LIMIT 12"
        }
        DatabaseBackend::Sqlite => return Err(incompatible()),
    };
    let rows = db
        .query_all(Statement::from_string(backend, sql.to_owned()))
        .await?;
    let expected = [
        ("installation_id", Some("128")),
        ("tenant_id", Some("128")),
        ("execution_id", Some("128")),
        ("principal_id", Some("128")),
        ("correlation_id", Some("128")),
        ("receipt_json", None),
        ("content_digest", Some("64")),
        ("disposition_json", None),
        ("byte_count", None),
        ("unknown_outcome", None),
        ("recorded_at", Some("20")),
    ];
    if rows.len() != expected.len() {
        return Err(incompatible());
    }
    for (row, (name, width)) in rows.iter().zip(expected) {
        let kind = match (backend, name, width) {
            (DatabaseBackend::Postgres, _, Some(_)) => "character varying",
            (DatabaseBackend::MySql, _, Some(_)) => "varchar",
            (_, "byte_count", _) => "bigint",
            (DatabaseBackend::MySql, "unknown_outcome", _) => "tinyint",
            (_, "unknown_outcome", _) => "boolean",
            (DatabaseBackend::MySql, _, _) => "longtext",
            _ => "text",
        };
        if row.try_get::<String>("", "name")? != name
            || row.try_get::<String>("", "kind")? != kind
            || row.try_get::<String>("", "nullable")? != "NO"
            || row
                .try_get::<Option<String>>("", "default_value")?
                .is_some()
            || (width.is_some() && row.try_get::<Option<String>>("", "width")?.as_deref() != width)
        {
            return Err(incompatible());
        }
        if backend == DatabaseBackend::Postgres {
            if row.try_get::<String>("", "generated_value")? != "NEVER"
                || row.try_get::<String>("", "identity_column")? != "NO"
                || (width.is_some()
                    && (row
                        .try_get::<Option<String>>("", "collation_name")?
                        .as_deref()
                        != Some("C")
                        || row
                            .try_get::<Option<String>>("", "collation_schema")?
                            .as_deref()
                            != Some("pg_catalog")))
            {
                return Err(incompatible());
            }
        } else if (name == "byte_count" && row.try_get::<String>("", "full_kind")? != "bigint")
            || (name == "unknown_outcome"
                && row.try_get::<String>("", "full_kind")? != "tinyint(1)")
            || !row.try_get::<String>("", "extra")?.is_empty()
            || !row.try_get::<String>("", "generated_value")?.is_empty()
            || ((width.is_some() || kind == "longtext")
                && (row
                    .try_get::<Option<String>>("", "collation_name")?
                    .as_deref()
                    != Some("utf8mb4_0900_bin")
                    || row
                        .try_get::<Option<String>>("", "charset_name")?
                        .as_deref()
                        != Some("utf8mb4")))
        {
            return Err(incompatible());
        }
    }
    let storage = if backend == DatabaseBackend::Postgres {
        "SELECT (c.relpersistence='p' AND c.relkind='r' AND NOT c.relispartition AND NOT EXISTS (SELECT 1 FROM pg_inherits p WHERE p.inhrelid=c.oid OR p.inhparent=c.oid)) AS valid FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=current_schema() AND c.relname='tenancy_execution_receipts'"
    } else {
        "SELECT (ENGINE='InnoDB' AND NOT EXISTS (SELECT 1 FROM information_schema.partitions p WHERE p.TABLE_SCHEMA=DATABASE() AND p.TABLE_NAME='tenancy_execution_receipts' AND p.PARTITION_NAME IS NOT NULL)) AS valid FROM information_schema.tables WHERE table_schema=DATABASE() AND table_name='tenancy_execution_receipts'"
    };
    let row = db
        .query_one(Statement::from_string(backend, storage.to_owned()))
        .await?
        .ok_or_else(incompatible)?;
    let valid = if backend == DatabaseBackend::Postgres {
        row.try_get::<bool>("", "valid")?
    } else {
        row.try_get::<i64>("", "valid")? == 1
    };
    if !valid {
        return Err(incompatible());
    }
    Ok(())
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        catalog::names(manager).await?;
        if manager.has_table(TABLE).await? {
            validate_table(manager).await?;
            catalog::indexes(manager, false).await?;
        } else {
            manager
                .create_table(definition(manager.get_database_backend()))
                .await?;
        }
        if !manager.has_index(TABLE, INDEX).await? {
            manager.create_index(correlation()).await?;
        }
        validate_table(manager).await?;
        catalog::indexes(manager, true).await
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if !manager.has_table(TABLE).await? {
            return Ok(());
        }
        validate_table(manager).await?;
        catalog::indexes(manager, false).await?;
        catalog::dependencies(manager).await?;
        if manager
            .get_connection()
            .query_one(Statement::from_string(
                manager.get_database_backend(),
                "SELECT 1 FROM tenancy_execution_receipts LIMIT 1".to_owned(),
            ))
            .await?
            .is_some()
        {
            return Err(DbErr::Custom(
                "migration035 cannot remove retained execution receipts".into(),
            ));
        }
        manager
            .drop_table(Table::drop().table(Alias::new(TABLE)).to_owned())
            .await
    }
}
