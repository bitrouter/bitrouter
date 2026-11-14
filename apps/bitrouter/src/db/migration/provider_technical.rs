//! Optional diagnostic schema; never a required migration or serving gate.
use super::m20240101_000028_resource_tenancy::{identity, payload};
use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use sea_orm_migration::prelude::*;
use sha2::{Digest, Sha256};

pub(crate) const TABLE: &str = "tenancy_provider_technical_facts";
const VERSION: &str = "tenancy_provider_technical_schema";
pub(crate) fn definition(backend: DatabaseBackend) -> TableCreateStatement {
    let mut table = Table::create();
    table.table(Alias::new(TABLE));
    if backend == DatabaseBackend::MySql {
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
    for name in ["receipt_digest", "technical_digest"] {
        table.col(identity(backend, name, 64, false));
    }
    table.col(payload(backend, "technical_json"));
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
    table.foreign_key(
        ForeignKey::create()
            .name("fk_provider_technical_receipt")
            .from_tbl(Alias::new(TABLE))
            .from_col(Alias::new("installation_id"))
            .from_col(Alias::new("tenant_id"))
            .from_col(Alias::new("execution_id"))
            .to_tbl(Alias::new("tenancy_execution_receipts"))
            .to_col(Alias::new("installation_id"))
            .to_col(Alias::new("tenant_id"))
            .to_col(Alias::new("execution_id"))
            .on_delete(ForeignKeyAction::Cascade),
    );
    table.to_owned()
}

fn version_definition() -> TableCreateStatement {
    Table::create()
        .table(Alias::new(VERSION))
        .col(
            ColumnDef::new(Alias::new("version"))
                .integer()
                .not_null()
                .primary_key(),
        )
        .col(
            ColumnDef::new(Alias::new("definition_sha256"))
                .string_len(64)
                .not_null(),
        )
        .to_owned()
}
fn definition_digest() -> String {
    let mut hash = Sha256::new();
    hash.update(b"provider-technical-schema-v1\0");
    hash.update(definition(DatabaseBackend::Sqlite).to_string(SqliteQueryBuilder));
    hash.update(version_definition().to_string(SqliteQueryBuilder));
    hex::encode(hash.finalize())
}

/// Return false only when both main and temporary namespaces have no optional
/// objects. Partial, shadowed or conflicting layouts retain strict refusal.
pub(crate) async fn available(db: &impl ConnectionTrait) -> Result<bool> {
    ensure!(
        db.get_database_backend() == DatabaseBackend::Sqlite,
        "optional technical backend unavailable"
    );
    let main = db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT 1 FROM main.sqlite_schema WHERE lower(name) IN ('tenancy_provider_technical_facts','tenancy_provider_technical_schema') LIMIT 1")).await?;
    let temporary = db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT 1 FROM temp.sqlite_schema WHERE lower(name) IN ('tenancy_provider_technical_facts','tenancy_provider_technical_schema') LIMIT 1")).await?;
    if main.is_none() && temporary.is_none() {
        return Ok(false);
    }
    validate(db).await?;
    Ok(true)
}

/// Recognize only the exact registered optional layout and its committed
/// installation stamp. An orphan/conflicting name is never adopted or repaired.
pub(crate) async fn validate(db: &impl ConnectionTrait) -> Result<()> {
    ensure!(
        db.get_database_backend() == DatabaseBackend::Sqlite,
        "optional technical backend unavailable"
    );
    let rows=db.query_all(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT name,type,CASE WHEN length(CAST(sql AS BLOB))<=65536 THEN sql ELSE NULL END AS sql FROM main.sqlite_schema WHERE lower(name) IN ('tenancy_provider_technical_facts','tenancy_provider_technical_schema') ORDER BY name LIMIT 3")).await?;
    ensure!(rows.len() == 2, "optional technical schema unavailable");
    for (row, (name, expected)) in rows.iter().zip([
        (
            TABLE,
            definition(DatabaseBackend::Sqlite).to_string(SqliteQueryBuilder),
        ),
        (VERSION, version_definition().to_string(SqliteQueryBuilder)),
    ]) {
        ensure!(
            row.try_get::<String>("", "name")? == name
                && row.try_get::<String>("", "type")? == "table"
                && row.try_get::<String>("", "sql")? == expected,
            "unknown optional technical object"
        );
    }
    ensure!(db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT 1 FROM temp.sqlite_schema WHERE lower(name) IN ('tenancy_provider_technical_facts','tenancy_provider_technical_schema') LIMIT 1")).await?.is_none(),"shadow optional technical object");
    let rows=db.query_all(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT CASE WHEN typeof(version)='integer' THEN version END AS version,CASE WHEN length(CAST(definition_sha256 AS BLOB))=64 THEN definition_sha256 END AS digest FROM main.tenancy_provider_technical_schema LIMIT 2")).await?;
    ensure!(
        rows.len() == 1
            && rows[0].try_get::<i32>("", "version")? == 1
            && rows[0].try_get::<String>("", "digest")? == definition_digest(),
        "optional technical installation stamp mismatch"
    );
    Ok(())
}

/// Called only by disposable, original-owner optional work after the required
/// 48 migrations have succeeded. Both new objects and the stamp commit together;
/// an error/unknown ACK is not recorded as a mandatory migration success.
pub(crate) async fn install(db: &impl ConnectionTrait) -> Result<()> {
    ensure!(
        db.get_database_backend() == DatabaseBackend::Sqlite,
        "optional technical backend unavailable"
    );
    db.execute_unprepared("BEGIN IMMEDIATE").await?;
    let tx = db;
    let result:Result<()>=async {
        let exists=tx.query_one(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT 1 FROM main.sqlite_schema WHERE lower(name) IN ('tenancy_provider_technical_facts','tenancy_provider_technical_schema') LIMIT 1")).await?.is_some();
        if !exists {
            tx.execute_unprepared(&definition(DatabaseBackend::Sqlite).to_string(SqliteQueryBuilder)).await?;
            tx.execute_unprepared(&version_definition().to_string(SqliteQueryBuilder)).await?;
            tx.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "INSERT INTO tenancy_provider_technical_schema(version,definition_sha256) VALUES(1,?)",[definition_digest().into()])).await?;
        }
        validate(tx).await
    }.await;
    if let Err(error) = result {
        tx.execute_unprepared("ROLLBACK")
            .await
            .context("close refused optional installation")?;
        return Err(error);
    }
    tx.execute_unprepared("COMMIT")
        .await
        .context("optional installation acknowledgement")?;
    Ok(())
}
