//! Pin the explicit tenant wrapping key without touching Legacy continuation rows.

use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use sea_orm_migration::prelude::*;

use super::m20240101_000028_resource_tenancy::identity;

#[derive(DeriveMigrationName)]
pub struct Migration;

const TABLE: &str = "tenancy_continuation_epoch";

fn definition(backend: DatabaseBackend) -> TableCreateStatement {
    let mut table = Table::create();
    if backend == DatabaseBackend::MySql {
        table.engine("InnoDB");
    }
    table
        .table(Alias::new(TABLE))
        .col(
            ColumnDef::new(Alias::new("singleton"))
                .integer()
                .not_null()
                .primary_key(),
        )
        .col(identity(backend, "installation_id", 128, false))
        .col(identity(backend, "key_id", 64, false))
        .col(identity(backend, "created_at", 20, false))
        .to_owned()
}

fn incompatible() -> DbErr {
    DbErr::Custom("migration031 incompatible native key epoch".into())
}

async fn validate(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let backend = manager.get_database_backend();
    let db = manager.get_connection();
    if backend == DatabaseBackend::Sqlite {
        let rows = db.query_all(Statement::from_string(backend,
            "SELECT type, sql FROM sqlite_master WHERE name = 'tenancy_continuation_epoch' LIMIT 2".to_owned())).await?;
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
    if backend == DatabaseBackend::MySql {
        let engine = db.query_one(Statement::from_string(backend, "SELECT ENGINE AS engine FROM information_schema.tables WHERE table_schema = DATABASE() AND table_name = 'tenancy_continuation_epoch'".to_owned())).await?.ok_or_else(incompatible)?;
        if engine.try_get::<String>("", "engine")? != "InnoDB" {
            return Err(incompatible());
        }
    }
    if backend == DatabaseBackend::Postgres {
        // Matching columns are insufficient: an UNLOGGED epoch is truncated
        // after an unclean shutdown and must never authorize key replacement.
        let rows = db.query_all(Statement::from_string(backend,
            "SELECT c.relpersistence::text AS persistence, c.relkind::text AS kind FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = current_schema() AND c.relname = 'tenancy_continuation_epoch' LIMIT 2".to_owned())).await?;
        if rows.len() != 1
            || rows[0].try_get::<String>("", "persistence")? != "p"
            || rows[0].try_get::<String>("", "kind")? != "r"
        {
            return Err(incompatible());
        }
    }
    let sql = match backend {
        DatabaseBackend::Postgres => {
            "SELECT column_name AS name, data_type AS kind, character_maximum_length::text AS width, is_nullable AS nullable, column_default AS default_value, collation_name, collation_schema, is_generated AS generation_expression_value, is_identity AS identity_column FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = 'tenancy_continuation_epoch' ORDER BY ordinal_position LIMIT 5"
        }
        DatabaseBackend::MySql => {
            "SELECT COLUMN_NAME AS name, DATA_TYPE AS kind, CAST(CHARACTER_MAXIMUM_LENGTH AS CHAR) AS width, IS_NULLABLE AS nullable, COLUMN_DEFAULT AS default_value, COLLATION_NAME AS collation_name, CHARACTER_SET_NAME AS charset_name, EXTRA AS extra, GENERATION_EXPRESSION AS generation_expression_value FROM information_schema.columns WHERE table_schema = DATABASE() AND table_name = 'tenancy_continuation_epoch' ORDER BY ordinal_position LIMIT 5"
        }
        DatabaseBackend::Sqlite => return Err(incompatible()),
    };
    let rows = db
        .query_all(Statement::from_string(backend, sql.to_owned()))
        .await?;
    if rows.len() != 4 {
        return Err(incompatible());
    }
    for (row, (name, width)) in rows.iter().zip([
        ("singleton", None),
        ("installation_id", Some("128")),
        ("key_id", Some("64")),
        ("created_at", Some("20")),
    ]) {
        let kind = match (backend, width) {
            (DatabaseBackend::Postgres, Some(_)) => "character varying",
            (DatabaseBackend::MySql, Some(_)) => "varchar",
            (DatabaseBackend::MySql, None) => "int",
            _ => "integer",
        };
        if row.try_get::<String>("", "name")? != name
            || row.try_get::<String>("", "kind")? != kind
            || row.try_get::<Option<String>>("", "width")?.as_deref() != width
            || row.try_get::<String>("", "nullable")? != "NO"
            || row
                .try_get::<Option<String>>("", "default_value")?
                .is_some()
        {
            return Err(incompatible());
        }
        if backend == DatabaseBackend::Postgres {
            if row.try_get::<String>("", "generation_expression_value")? != "NEVER"
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
        } else if !row.try_get::<String>("", "extra")?.is_empty()
            || !row
                .try_get::<String>("", "generation_expression_value")?
                .is_empty()
            || (width.is_some()
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
    let sql = if backend == DatabaseBackend::Postgres {
        "SELECT pg_get_constraintdef(oid) AS definition FROM pg_constraint WHERE conrelid = 'tenancy_continuation_epoch'::regclass AND contype <> 'n' LIMIT 2"
    } else {
        "SELECT CONCAT(CONSTRAINT_TYPE, ':', (SELECT GROUP_CONCAT(COLUMN_NAME ORDER BY ORDINAL_POSITION SEPARATOR ',') FROM information_schema.key_column_usage k WHERE k.constraint_schema = c.constraint_schema AND k.table_name = c.table_name AND k.constraint_name = c.constraint_name)) AS definition FROM information_schema.table_constraints c WHERE constraint_schema = DATABASE() AND table_name = 'tenancy_continuation_epoch' LIMIT 2"
    };
    let rows = db
        .query_all(Statement::from_string(backend, sql.to_owned()))
        .await?;
    let expected = if backend == DatabaseBackend::Postgres {
        "PRIMARY KEY (singleton)"
    } else {
        "PRIMARY KEY:singleton"
    };
    if rows.len() != 1 || rows[0].try_get::<String>("", "definition")? != expected {
        return Err(incompatible());
    }
    Ok(())
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if !manager.has_table(TABLE).await? {
            manager
                .create_table(definition(manager.get_database_backend()))
                .await?;
        }
        // Resume an acknowledged DDL whose migration-ledger write was interrupted.
        validate(manager).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if !manager.has_table(TABLE).await? {
            return Ok(());
        }
        validate(manager).await?;
        let row = manager
            .get_connection()
            .query_one(Statement::from_string(
                manager.get_database_backend(),
                "SELECT 1 FROM tenancy_continuation_epoch LIMIT 1".to_owned(),
            ))
            .await?;
        if row.is_some() {
            return Err(DbErr::Custom(
                "migration031 cannot remove a retained native artifact key pin".into(),
            ));
        }
        manager
            .drop_table(Table::drop().table(Alias::new(TABLE)).to_owned())
            .await
    }
}

#[cfg(test)]
mod tests;
