//! Add format metadata without reserializing historical receipt or evidence bytes.
use super::m20240101_000028_resource_tenancy::{identity, payload};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;
const INTENTS: &str = "tenancy_dispatch_intents";

fn intent_table(backend: DatabaseBackend) -> TableCreateStatement {
    let mut table = Table::create();
    table.table(Alias::new(INTENTS));
    if backend == DatabaseBackend::MySql {
        table.engine("InnoDB");
    }
    for name in ["installation_id", "tenant_id", "execution_id"] {
        table.col(identity(backend, name, 128, false));
    }
    table
        .col(payload(backend, "intent_json"))
        .col(identity(backend, "content_digest", 64, false))
        .col(
            ColumnDef::new(Alias::new("byte_count"))
                .big_integer()
                .not_null(),
        )
        .primary_key(
            Index::create()
                .col(Alias::new("installation_id"))
                .col(Alias::new("tenant_id"))
                .col(Alias::new("execution_id")),
        );
    table.to_owned()
}
pub(super) fn format_column() -> ColumnDef {
    ColumnDef::new(Alias::new("receipt_format"))
        .integer()
        .not_null()
        .default(1)
        .to_owned()
}

pub(super) fn directory_index() -> IndexCreateStatement {
    Index::create()
        .name("idx_tenancy_credentials_directory")
        .table(Alias::new("tenancy_credentials"))
        .col(Alias::new("tenant_id"))
        .col(Alias::new("id"))
        .to_owned()
}

async fn sqlite_indexes(db: &impl ConnectionTrait, table: &str) -> Result<(), DbErr> {
    let backend = db.get_database_backend();
    let rows=db.query_all(Statement::from_sql_and_values(backend,"SELECT CASE WHEN length(CAST(name AS BLOB))<=256 THEN name ELSE NULL END AS name, origin, \"unique\" AS unique_value, partial FROM pragma_index_list(?) LIMIT 3",vec![table.into()])).await?;
    if rows.len() != 1 {
        return Err(incompatible());
    }
    let mut primary = false;
    for row in rows {
        let name = row.try_get::<String>("", "name")?;
        if name.len() > 256 || row.try_get::<i64>("", "partial")? != 0 {
            return Err(incompatible());
        }
        let is_primary = row.try_get::<String>("", "origin")? == "pk";
        let expected: &[&str] = if is_primary && !primary {
            primary = true;
            &["installation_id", "tenant_id", "execution_id"]
        } else {
            return Err(incompatible());
        };
        if row.try_get::<i64>("", "unique_value")? != i64::from(is_primary) {
            return Err(incompatible());
        }
        let columns=db.query_all(Statement::from_sql_and_values(backend,"SELECT name, coll, \"desc\" AS descending FROM pragma_index_xinfo(?) WHERE key=1 ORDER BY seqno LIMIT 5",vec![name.into()])).await?;
        if columns.len() != expected.len() {
            return Err(incompatible());
        }
        for (column, expected) in columns.iter().zip(expected) {
            if column.try_get::<String>("", "name")? != *expected
                || column.try_get::<String>("", "coll")? != "BINARY"
                || column.try_get::<i64>("", "descending")? != 0
            {
                return Err(incompatible());
            }
        }
    }
    if !primary {
        return Err(incompatible());
    }
    Ok(())
}
fn incompatible() -> DbErr {
    DbErr::Custom("incompatible ordered receipt storage".into())
}
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let backend = manager.get_database_backend();
        if manager.has_table(INTENTS).await?
            || manager
                .has_column("tenancy_execution_receipts", "receipt_format")
                .await?
        {
            return Err(incompatible());
        }
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("tenancy_execution_receipts"))
                    .add_column(format_column())
                    .to_owned(),
            )
            .await?;
        manager.create_table(intent_table(backend)).await?;
        manager.create_index(directory_index()).await?;
        validate(db).await
    }
    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "ordered receipt storage requires verified-backup rollback".into(),
        ))
    }
}

/// SQLite production and archive readers verify immutable storage before decoding.
pub(crate) async fn validate(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    let backend = db.get_database_backend();
    if backend != DatabaseBackend::Sqlite {
        return Ok(());
    }
    let row = db.query_one(Statement::from_string(backend,
        "SELECT CASE WHEN length(CAST(sql AS BLOB))<=65536 THEN sql ELSE NULL END AS sql FROM sqlite_master WHERE name='tenancy_dispatch_intents' AND type='table'".to_owned())).await?.ok_or_else(incompatible)?;
    if row.try_get::<String>("", "sql")? != intent_table(backend).to_string(SqliteQueryBuilder) {
        return Err(incompatible());
    }
    if db.query_one(Statement::from_string(backend,
        "SELECT 1 FROM sqlite_master WHERE type='trigger' AND tbl_name IN ('tenancy_dispatch_intents','tenancy_execution_receipts') UNION ALL SELECT 1 FROM temp.sqlite_schema WHERE lower(name) IN ('tenancy_dispatch_intents','tenancy_execution_receipts') LIMIT 1".to_owned())).await?.is_some() { return Err(incompatible()); }
    super::m20240101_000042_receipt_archive::validate(db).await?;
    sqlite_indexes(db, INTENTS).await?;
    Ok(())
}
