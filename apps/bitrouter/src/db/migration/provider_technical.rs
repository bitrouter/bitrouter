//! Optional diagnostic schema; never a required migration or serving gate.
use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseBackend, QueryResult, Statement, sea_query::Query};
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

async fn postgres_objects(db: &impl ConnectionTrait) -> Result<Vec<QueryResult>> {
    Ok(db
        .query_all(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT c.relname AS name,c.relkind::text AS kind,CASE WHEN n.oid=pg_my_temp_schema() THEN 'temporary' ELSE 'current' END AS scope FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE c.relname IN ('tenancy_provider_technical_facts','tenancy_provider_technical_schema') AND (n.nspname=current_schema() OR n.oid=pg_my_temp_schema()) ORDER BY scope,name",
        ))
        .await?)
}

async fn validate_postgres(db: &impl ConnectionTrait, objects: Vec<QueryResult>) -> Result<()> {
    ensure!(objects.len() == 2, "optional technical schema unavailable");
    for (row, name) in objects.iter().zip([TABLE, VERSION]) {
        ensure!(
            row.try_get::<String>("", "name")? == name
                && row.try_get::<String>("", "kind")? == "r"
                && row.try_get::<String>("", "scope")? == "current",
            "unknown optional technical object"
        );
    }
    let columns = db
        .query_all(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT c.relname AS table_name,a.attname AS column_name,pg_catalog.format_type(a.atttypid,a.atttypmod) AS kind,a.attnotnull AS required,COALESCE(coll.collname,'') AS collation,pg_catalog.pg_get_expr(d.adbin,d.adrelid) AS default_value FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace JOIN pg_catalog.pg_attribute a ON a.attrelid=c.oid AND a.attnum>0 AND NOT a.attisdropped LEFT JOIN pg_catalog.pg_collation coll ON coll.oid=a.attcollation LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid=c.oid AND d.adnum=a.attnum WHERE n.nspname=current_schema() AND c.relname IN ('tenancy_provider_technical_facts','tenancy_provider_technical_schema') ORDER BY c.relname,a.attnum",
        ))
        .await?;
    let expected = [
        (TABLE, "installation_id", "character varying(128)", "C"),
        (TABLE, "tenant_id", "character varying(128)", "C"),
        (TABLE, "execution_id", "character varying(128)", "C"),
        (TABLE, "schema_version", "integer", ""),
        (TABLE, "receipt_digest", "character varying(64)", "C"),
        (TABLE, "technical_digest", "character varying(64)", "C"),
        (TABLE, "technical_json", "text", "default"),
        (TABLE, "byte_count", "bigint", ""),
        (VERSION, "version", "integer", ""),
        (
            VERSION,
            "definition_sha256",
            "character varying(64)",
            "default",
        ),
    ];
    ensure!(
        columns.len() == expected.len(),
        "optional technical columns differ"
    );
    for (row, (table, name, kind, collation)) in columns.iter().zip(expected) {
        ensure!(
            row.try_get::<String>("", "table_name")? == table
                && row.try_get::<String>("", "column_name")? == name
                && row.try_get::<String>("", "kind")? == kind
                && row.try_get::<bool>("", "required")?
                && row.try_get::<String>("", "collation")? == collation
                && row
                    .try_get::<Option<String>>("", "default_value")?
                    .is_none(),
            "optional technical column differs"
        );
    }
    let constraints = db
        .query_all(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT c.relname AS table_name,k.contype::text AS kind,k.conname AS name,pg_catalog.pg_get_constraintdef(k.oid,false) AS definition FROM pg_catalog.pg_constraint k JOIN pg_catalog.pg_class c ON c.oid=k.conrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=current_schema() AND c.relname IN ('tenancy_provider_technical_facts','tenancy_provider_technical_schema') AND k.contype<>'n' ORDER BY c.relname,k.contype,k.conname",
        ))
        .await?;
    ensure!(
        constraints.len() == 3,
        "optional technical constraints differ"
    );
    let mut facts_primary = false;
    let mut version_primary = false;
    let mut receipt_foreign = false;
    for row in constraints {
        let table: String = row.try_get("", "table_name")?;
        let kind: String = row.try_get("", "kind")?;
        let definition: String = row.try_get("", "definition")?;
        match (table.as_str(), kind.as_str()) {
            (TABLE, "p") => {
                facts_primary |=
                    definition == "PRIMARY KEY (installation_id, tenant_id, execution_id)";
            }
            (VERSION, "p") => version_primary |= definition == "PRIMARY KEY (version)",
            (TABLE, "f") => {
                receipt_foreign |= row.try_get::<String>("", "name")?
                    == "fk_provider_technical_receipt"
                    && definition
                        == "FOREIGN KEY (installation_id, tenant_id, execution_id) REFERENCES tenancy_execution_receipts(installation_id, tenant_id, execution_id) ON DELETE CASCADE";
            }
            _ => {}
        }
    }
    ensure!(
        facts_primary && version_primary && receipt_foreign,
        "optional technical constraints differ"
    );
    let indexes = db
        .query_all(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT i.indisprimary AS primary_index,i.indisunique AS unique_index,i.indisvalid AS valid_index,i.indpred IS NULL AS full_index,i.indexprs IS NULL AS plain_index FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class c ON c.oid=i.indrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=current_schema() AND c.relname IN ('tenancy_provider_technical_facts','tenancy_provider_technical_schema')",
        ))
        .await?;
    ensure!(indexes.len() == 2, "optional technical indexes differ");
    for row in indexes {
        ensure!(
            row.try_get::<bool>("", "primary_index")?
                && row.try_get::<bool>("", "unique_index")?
                && row.try_get::<bool>("", "valid_index")?
                && row.try_get::<bool>("", "full_index")?
                && row.try_get::<bool>("", "plain_index")?,
            "optional technical indexes differ"
        );
    }
    ensure!(
        db.query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT 1 FROM pg_catalog.pg_trigger t JOIN pg_catalog.pg_class c ON c.oid=t.tgrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=current_schema() AND c.relname IN ('tenancy_provider_technical_facts','tenancy_provider_technical_schema') AND NOT t.tgisinternal LIMIT 1",
        ))
        .await?
        .is_none(),
        "optional technical trigger"
    );
    let rows = db
        .query_all(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT CAST(version AS BIGINT) AS version,CASE WHEN octet_length(definition_sha256)=64 THEN definition_sha256 ELSE NULL END AS digest FROM tenancy_provider_technical_schema LIMIT 2",
        ))
        .await?;
    ensure!(
        rows.len() == 1
            && rows[0].try_get::<i64>("", "version")? == 1
            && rows[0].try_get::<String>("", "digest")? == definition_digest(),
        "optional technical installation stamp mismatch"
    );
    Ok(())
}

/// Return false only when both main and temporary namespaces have no optional
/// objects. Partial, shadowed or conflicting layouts retain strict refusal.
pub(crate) async fn available(db: &impl ConnectionTrait) -> Result<bool> {
    let backend = db.get_database_backend();
    ensure!(
        matches!(backend, DatabaseBackend::Sqlite | DatabaseBackend::Postgres),
        "optional technical backend unavailable"
    );
    if backend == DatabaseBackend::Postgres {
        let rows = postgres_objects(db).await?;
        if rows.is_empty() {
            return Ok(false);
        }
        validate_postgres(db, rows).await?;
        return Ok(true);
    }
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
    let backend = db.get_database_backend();
    ensure!(
        matches!(backend, DatabaseBackend::Sqlite | DatabaseBackend::Postgres),
        "optional technical backend unavailable"
    );
    if backend == DatabaseBackend::Postgres {
        let rows = postgres_objects(db).await?;
        return validate_postgres(db, rows).await;
    }
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
    let backend = db.get_database_backend();
    ensure!(
        matches!(backend, DatabaseBackend::Sqlite | DatabaseBackend::Postgres),
        "optional technical backend unavailable"
    );
    db.execute_unprepared(if backend == DatabaseBackend::Sqlite {
        "BEGIN IMMEDIATE"
    } else {
        "BEGIN"
    })
    .await?;
    let tx = db;
    let result:Result<()>=async {
        let exists = if backend == DatabaseBackend::Sqlite {
            tx.query_one(Statement::from_string(DatabaseBackend::Sqlite,
                "SELECT 1 FROM main.sqlite_schema WHERE lower(name) IN ('tenancy_provider_technical_facts','tenancy_provider_technical_schema') LIMIT 1")).await?.is_some()
        } else {
            !postgres_objects(tx).await?.is_empty()
        };
        if !exists {
            let facts = if backend == DatabaseBackend::Sqlite {
                definition(backend).to_string(SqliteQueryBuilder)
            } else {
                definition(backend).to_string(PostgresQueryBuilder)
            };
            let version = if backend == DatabaseBackend::Sqlite {
                version_definition().to_string(SqliteQueryBuilder)
            } else {
                version_definition().to_string(PostgresQueryBuilder)
            };
            tx.execute_unprepared(&facts).await?;
            tx.execute_unprepared(&version).await?;
            let insert = Query::insert()
                .into_table(Alias::new(VERSION))
                .columns([Alias::new("version"), Alias::new("definition_sha256")])
                .values([1_i32.into(), definition_digest().into()])?
                .to_owned();
            tx.execute(backend.build(&insert)).await?;
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

fn identity(backend: DatabaseBackend, name: &str, length: u32, nullable: bool) -> ColumnDef {
    let mut column = ColumnDef::new(Alias::new(name));
    // IDs and issuer/subject pairs are byte-exact, including case and trailing
    // spaces. MySQL's NO PAD binary Unicode collation avoids both default
    // case-folding and the older *_bin collations' PAD SPACE equality.
    // https://dev.mysql.com/doc/refman/8.4/en/charset-binary-collations.html
    match backend {
        DatabaseBackend::MySql => {
            column.custom(Alias::new(format!(
                "VARCHAR({length}) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin"
            )));
        }
        DatabaseBackend::Postgres => {
            column.custom(Alias::new(format!("VARCHAR({length}) COLLATE \"C\"")));
        }
        DatabaseBackend::Sqlite => {
            column.string_len(length).extra("COLLATE BINARY");
        }
    }
    if !nullable {
        column.not_null();
    }
    column
}

fn payload(backend: DatabaseBackend, name: &str) -> ColumnDef {
    let mut column = ColumnDef::new(Alias::new(name));
    // Valid 256-key scopes exceed MySQL TEXT's 64 KiB ceiling. Application
    // validation supplies the tighter semantic/byte bounds on these payloads.
    // https://dev.mysql.com/doc/refman/8.4/en/blob.html
    if backend == DatabaseBackend::MySql {
        column.custom(Alias::new(
            "LONGTEXT CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin",
        ));
    } else {
        column.text();
    }
    column.not_null().to_owned()
}
