//! Additive immutable original usage evidence. Existing receipts are not rewritten.
use super::m20240101_000028_resource_tenancy::{identity, payload};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, Value};
use sea_orm_migration::prelude::*;
mod catalog;

#[derive(DeriveMigrationName)]
pub struct Migration;
const TABLE: &str = "tenancy_usage_evidence";
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.has_table(TABLE).await? {
            return Err(DbErr::Custom(
                "migration040 refuses an existing evidence object".into(),
            ));
        }
        manager
            .create_table(definition(manager.get_database_backend()))
            .await?;
        validate(manager.get_connection()).await
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.has_table(TABLE).await? {
            return Err(DbErr::Custom(
                "migration040 requires verified-backup rollback".into(),
            ));
        }
        Ok(())
    }
}

fn definition(backend: DatabaseBackend) -> TableCreateStatement {
    let mut table = Table::create();
    table.table(Alias::new(TABLE));
    if backend == sea_orm::DatabaseBackend::MySql {
        table.engine("InnoDB");
    }
    for name in ["installation_id", "tenant_id", "execution_id"] {
        table.col(identity(backend, name, 128, false));
    }
    table.col(
        ColumnDef::new(Alias::new("schema_version"))
            .integer()
            .not_null(),
    );
    for name in ["receipt_digest", "evidence_digest"] {
        table.col(identity(backend, name, 64, false));
    }
    table.col(payload(backend, "evidence_json"));
    table.col(
        ColumnDef::new(Alias::new("byte_count"))
            .big_integer()
            .not_null(),
    );
    table.primary_key(
        Index::create()
            .col(Alias::new("installation_id"))
            .col(Alias::new("tenant_id"))
            .col(Alias::new("execution_id")),
    );
    table.to_owned()
}

fn incompatible() -> DbErr {
    DbErr::Custom("migration040 incompatible usage evidence storage".into())
}
async fn validate_table_on(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    let backend = db.get_database_backend();
    if backend == DatabaseBackend::Sqlite {
        let rows = db.query_all(Statement::from_string(backend,
            "SELECT type, CASE WHEN length(CAST(sql AS BLOB))<=65536 THEN sql ELSE NULL END AS sql FROM sqlite_master WHERE name='tenancy_usage_evidence' LIMIT 2".to_owned())).await?;
        let row = rows.first().ok_or_else(incompatible)?;
        if rows.len() != 1
            || row.try_get::<String>("", "type")? != "table"
            || row.try_get::<String>("", "sql")?
                != definition(backend).to_string(SqliteQueryBuilder)
        {
            return Err(incompatible());
        }
        if db.query_one(Statement::from_string(backend,
            "SELECT 1 FROM temp.sqlite_schema WHERE lower(name)='tenancy_usage_evidence' LIMIT 1".to_owned())).await?.is_some() {
            return Err(incompatible());
        }
        return Ok(());
    }
    let sql = match backend {
        DatabaseBackend::Postgres => {
            "SELECT column_name AS name, data_type AS kind, character_maximum_length::text AS width, is_nullable AS nullable, column_default AS default_value, collation_name, collation_schema, is_generated AS generated_value, is_identity AS identity_column FROM information_schema.columns WHERE table_schema=current_schema() AND table_name='tenancy_usage_evidence' ORDER BY ordinal_position LIMIT 9"
        }
        DatabaseBackend::MySql => {
            "SELECT COLUMN_NAME AS name, DATA_TYPE AS kind, COLUMN_TYPE AS full_kind, CAST(CHARACTER_MAXIMUM_LENGTH AS CHAR) AS width, IS_NULLABLE AS nullable, COLUMN_DEFAULT AS default_value, COLLATION_NAME AS collation_name, CHARACTER_SET_NAME AS charset_name, EXTRA AS extra, GENERATION_EXPRESSION AS generated_value FROM information_schema.columns WHERE table_schema=DATABASE() AND table_name='tenancy_usage_evidence' ORDER BY ordinal_position LIMIT 9"
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
        ("schema_version", None),
        ("receipt_digest", Some("64")),
        ("evidence_digest", Some("64")),
        ("evidence_json", None),
        ("byte_count", None),
    ];

    if rows.len() != expected.len() {
        return Err(incompatible());
    }
    for (row, (name, width)) in rows.iter().zip(expected) {
        let kind = match (backend, name, width) {
            (DatabaseBackend::Postgres, _, Some(_)) => "character varying",
            (DatabaseBackend::MySql, _, Some(_)) => "varchar",
            (_, "byte_count", _) => "bigint",
            (DatabaseBackend::MySql, "schema_version", _) => "int",
            (_, "schema_version", _) => "integer",
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
            || (name == "schema_version" && row.try_get::<String>("", "full_kind")? != "int")
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
        "SELECT (c.relpersistence='p' AND c.relkind='r' AND NOT c.relispartition AND NOT EXISTS (SELECT 1 FROM pg_inherits p WHERE p.inhrelid=c.oid OR p.inhparent=c.oid)) AS valid FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=current_schema() AND c.relname='tenancy_usage_evidence'"
    } else {
        "SELECT (ENGINE='InnoDB' AND NOT EXISTS (SELECT 1 FROM information_schema.partitions p WHERE p.TABLE_SCHEMA=DATABASE() AND p.TABLE_NAME='tenancy_usage_evidence' AND p.PARTITION_NAME IS NOT NULL)) AS valid FROM information_schema.tables WHERE table_schema=DATABASE() AND table_name='tenancy_usage_evidence'"
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

/// Read-only schema verification for startup, archives and stopped recovery.
pub(crate) async fn validate(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    validate_table_on(db).await?;
    catalog::indexes_on(db).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, ensure};
    #[tokio::test]
    async fn exact_schema_validates_and_existing_objects_are_never_adopted() -> Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        let manager = SchemaManager::new(&db);
        Migration.up(&manager).await?;
        validate(&db).await?;
        ensure!(Migration.up(&manager).await.is_err());
        ensure!(Migration.down(&manager).await.is_err());
        db.execute_unprepared("CREATE TEMP VIEW tenancy_usage_evidence AS SELECT 1")
            .await?;
        ensure!(validate(&db).await.is_err());
        db.close().await?;
        Ok(())
    }
    #[tokio::test]
    async fn trigger_or_nonbinary_shape_cannot_be_served_as_immutable_evidence() -> Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        Migration.up(&SchemaManager::new(&db)).await?;
        db.execute_unprepared("CREATE TRIGGER rewrite_usage AFTER INSERT ON tenancy_usage_evidence BEGIN DELETE FROM tenancy_usage_evidence; END").await?;
        ensure!(validate(&db).await.is_err());
        db.execute_unprepared("DROP TRIGGER rewrite_usage").await?;
        db.execute_unprepared("DROP TABLE tenancy_usage_evidence")
            .await?;
        db.execute_unprepared("CREATE TABLE tenancy_usage_evidence(installation_id TEXT COLLATE NOCASE, tenant_id TEXT, execution_id TEXT)").await?;
        ensure!(validate(&db).await.is_err());
        db.close().await?;
        Ok(())
    }
}
