//! Typed SQLite values, bounded before they cross the driver boundary.
//! SQLite storage classes: <https://www.sqlite.org/datatype3.html>
//! BLOB casts preserve text bytes: <https://www.sqlite.org/lang_expr.html#castexpr>
use super::{
    files::Fingerprint,
    format::{
        Column, Inventory, LedgerEntry, MAX_BYTES, MAX_CELL, MAX_FILE, MAX_ROW, MAX_ROWS, Table,
    },
};
use anyhow::{Result, ensure};
use futures::TryStreamExt;
use sea_orm::sqlx::{Connection, Row, SqliteConnection};

#[derive(Default)]
struct Budget {
    rows: u64,
    bytes: u64,
}
impl Budget {
    fn charge(&mut self, rows: u64, bytes: u64) -> Result<()> {
        self.rows = self
            .rows
            .checked_add(rows)
            .ok_or_else(|| anyhow::anyhow!("row count overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| anyhow::anyhow!("byte count overflow"))?;
        ensure!(
            self.rows <= MAX_ROWS && self.bytes <= MAX_BYTES,
            "offline inventory exceeds profile"
        );
        Ok(())
    }
}
fn identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
        "unsupported schema identifier"
    );
    Ok(())
}
fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

pub(super) async fn inspect(db: &mut SqliteConnection, key: &[u8; 32]) -> Result<Inventory> {
    let mut tx = db.begin().await?;
    let result = inspect_in(&mut tx, key).await;
    // Read outcome is not successful until the stable snapshot has been released.
    tx.rollback().await?;
    result
}
pub(super) async fn inspect_in(db: &mut SqliteConnection, key: &[u8; 32]) -> Result<Inventory> {
    let pages: i64 = sea_orm::sqlx::query_scalar("PRAGMA page_count")
        .fetch_one(&mut *db)
        .await?;
    let size: i64 = sea_orm::sqlx::query_scalar("PRAGMA page_size")
        .fetch_one(&mut *db)
        .await?;
    ensure!(
        pages >= 0
            && size > 0
            && pages
                .checked_mul(size)
                .is_some_and(|v| v as u64 <= MAX_FILE),
        "SQLite exceeds offline profile"
    );
    let encoding: String = sea_orm::sqlx::query_scalar("PRAGMA encoding")
        .fetch_one(&mut *db)
        .await?;
    ensure!(
        matches!(encoding.as_str(), "UTF-8" | "UTF-16le" | "UTF-16be"),
        "unsupported SQLite encoding"
    );
    let mut budget = Budget::default();
    let schema_digest = schema(db, key, &mut budget).await?;
    // Table-valued PRAGMAs allow bounded metadata projection before allocation.
    // https://www.sqlite.org/pragma.html#pragma_table_list
    let rows = sea_orm::sqlx::query("SELECT CASE WHEN length(CAST(name AS BLOB))<=256 THEN name ELSE NULL END AS name,type FROM pragma_table_list WHERE schema='main' AND name!='sqlite_schema' AND type!='view' ORDER BY name COLLATE BINARY LIMIT 257").fetch_all(&mut *db).await?;
    ensure!(rows.len() <= 256, "too many tables");
    let mut definitions = Vec::new();
    for row in rows {
        let name: String = row.try_get("name")?;
        identifier(&name)?;
        let kind: String = row.try_get("type")?;
        ensure!(
            kind == "table",
            "virtual/shadow tables are outside offline profile"
        );
        let columns = columns(db, &name).await?;
        indexes(db, &name).await?;
        definitions.push((name, columns));
    }
    // Qualify every source expression/column/index before any table read or
    // integrity evaluation. Integrity's argument bounds errors, not work.
    let mut tables = Vec::new();
    for (name, columns) in definitions {
        tables.push(table(db, &name, columns, key, &mut budget).await?);
    }
    let integrity: String = sea_orm::sqlx::query_scalar("PRAGMA integrity_check(1)")
        .fetch_one(&mut *db)
        .await?;
    ensure!(integrity == "ok", "SQLite integrity failed");
    let ledger = if tables.iter().any(|t| t.name == "seaql_migrations") {
        Some(ledger(db).await?)
    } else {
        None
    };
    let installation_id = if tables.iter().any(|t| t.name == "tenancy_installation") {
        installation(db).await?
    } else {
        None
    };
    let value_bytes = tables.iter().try_fold(0_u64, |sum, t| {
        sum.checked_add(t.value_bytes)
            .ok_or_else(|| anyhow::anyhow!("byte count overflow"))
    })?;
    Ok(Inventory {
        encoding,
        schema_digest,
        tables,
        rows: budget.rows,
        value_bytes,
        ledger,
        installation_id,
    })
}
async fn schema(db: &mut SqliteConnection, key: &[u8; 32], budget: &mut Budget) -> Result<String> {
    let count: i64 = sea_orm::sqlx::query_scalar("SELECT count(*) FROM sqlite_schema")
        .fetch_one(&mut *db)
        .await?;
    ensure!((0..=2048).contains(&count), "schema object limit exceeded");
    let mut digest = Fingerprint::new(key, b"schema")?;
    let query = "SELECT CASE WHEN length(CAST(type AS BLOB))<=32 THEN CAST(type AS BLOB) ELSE NULL END, CASE WHEN length(CAST(name AS BLOB))<=256 THEN CAST(name AS BLOB) ELSE NULL END, CASE WHEN length(CAST(tbl_name AS BLOB))<=256 THEN CAST(tbl_name AS BLOB) ELSE NULL END, CASE WHEN length(CAST(sql AS BLOB))<=1048576 THEN CAST(sql AS BLOB) ELSE NULL END, CASE WHEN sql IS NOT NULL AND length(CAST(sql AS BLOB))>1048576 THEN 1 ELSE 0 END, CASE WHEN length(CAST(type AS BLOB))<=32 THEN type ELSE NULL END, CASE WHEN length(CAST(sql AS BLOB))<=1048576 THEN sql ELSE NULL END FROM sqlite_schema ORDER BY type COLLATE BINARY,name COLLATE BINARY";
    let mut stream = sea_orm::sqlx::query(query).fetch(&mut *db);
    let mut seen = 0_i64;
    while let Some(row) = stream.try_next().await? {
        seen += 1;
        ensure!(
            seen <= 2048 && row.try_get::<i64, _>(4)? == 0,
            "schema exceeds profile"
        );
        let kind: String = row.try_get(5)?;
        let sql: Option<String> = row.try_get(6)?;
        super::schema::validate(kind.as_bytes(), sql.as_deref().map(str::as_bytes))?;
        for index in 0..4 {
            let bytes: Option<Vec<u8>> = row.try_get(index)?;
            ensure!(
                index == 3 || bytes.is_some(),
                "schema metadata exceeds profile"
            );
            digest.field(&[u8::from(bytes.is_some())]);
            if let Some(bytes) = bytes {
                budget.charge(0, bytes.len() as u64)?;
                digest.field(&bytes);
            }
        }
    }
    ensure!(seen == count, "schema changed during inspection");
    Ok(hex::encode(digest.finish()))
}
async fn columns(db: &mut SqliteConnection, name: &str) -> Result<Vec<Column>> {
    let rows=sea_orm::sqlx::query("SELECT CASE WHEN length(CAST(name AS BLOB))<=256 THEN name ELSE NULL END AS name,pk,hidden FROM pragma_table_xinfo(?) ORDER BY cid LIMIT 129").bind(name).fetch_all(&mut *db).await?;
    ensure!(
        !rows.is_empty() && rows.len() <= 128,
        "unsupported column count"
    );
    rows.into_iter()
        .map(|row| {
            ensure!(
                row.try_get::<i64, _>("hidden")? == 0,
                "generated/hidden columns are outside offline profile"
            );
            let name: String = row.try_get("name")?;
            identifier(&name)?;
            Ok(Column {
                name,
                primary_key_position: row.try_get("pk")?,
            })
        })
        .collect()
}
async fn indexes(db: &mut SqliteConnection, table: &str) -> Result<()> {
    // Index expressions and partial predicates are executable during VACUUM and
    // integrity_check. All production indexes are ordinary column indexes.
    let rows = sea_orm::sqlx::query("SELECT CASE WHEN length(CAST(name AS BLOB))<=256 THEN name ELSE NULL END AS name,partial FROM pragma_index_list(?) LIMIT 2049")
        .bind(table).fetch_all(&mut *db).await?;
    ensure!(rows.len() <= 2048, "index count exceeds profile");
    for row in rows {
        ensure!(
            row.try_get::<i64, _>("partial")? == 0,
            "partial index outside profile"
        );
        let name: String = row.try_get("name")?;
        identifier(&name)?;
        let fields = sea_orm::sqlx::query("SELECT cid FROM pragma_index_xinfo(?) LIMIT 130")
            .bind(name)
            .fetch_all(&mut *db)
            .await?;
        ensure!(fields.len() <= 129, "index width exceeds profile");
        for field in fields {
            ensure!(
                field.try_get::<i64, _>("cid")? >= -1,
                "expression index outside profile"
            );
        }
    }
    Ok(())
}
fn row_query(name: &str, columns: &[Column]) -> String {
    let sizes:Vec<_>=columns.iter().map(|c| {
        let c=quoted(&c.name);
        format!("CASE typeof({c}) WHEN 'null' THEN 0 WHEN 'integer' THEN 8 WHEN 'real' THEN 8 ELSE length(CAST({c} AS BLOB)) END")
    }).collect();
    let total = sizes.join(" + ");
    let mut projections = vec![format!("({total})")];
    for (column, size) in columns.iter().zip(&sizes) {
        let c = quoted(&column.name);
        // A single row cannot allocate N individually permitted large cells.
        let bounded = format!("({total})<={MAX_ROW} AND ({size})<={MAX_CELL}");
        projections.extend([
            format!("typeof({c})"),
            format!("({size})"),
            format!("CASE WHEN {bounded} AND typeof({c}) IN ('text','blob') THEN CAST({c} AS BLOB) ELSE NULL END"),
            format!("CASE WHEN typeof({c})='integer' THEN {c} ELSE NULL END"),
            format!("CASE WHEN typeof({c})='real' THEN {c} ELSE NULL END"),
        ]);
    }
    format!("SELECT {} FROM {}", projections.join(","), quoted(name))
}
async fn table(
    db: &mut SqliteConnection,
    name: &str,
    columns: Vec<Column>,
    key: &[u8; 32],
    budget: &mut Budget,
) -> Result<Table> {
    let query = row_query(name, &columns);
    let mut stream = sea_orm::sqlx::query(&query).fetch(&mut *db);
    let mut hashes = Vec::new();
    let mut value_bytes = 0_u64;
    while let Some(row) = stream.try_next().await? {
        let bytes: i64 = row.try_get(0)?;
        ensure!(
            bytes >= 0 && bytes as u64 <= MAX_ROW,
            "row exceeds offline profile"
        );
        budget.charge(1, bytes as u64)?;
        value_bytes = value_bytes
            .checked_add(bytes as u64)
            .ok_or_else(|| anyhow::anyhow!("byte count overflow"))?;
        let mut digest = Fingerprint::new(key, b"typed-row")?;
        digest.field(name.as_bytes());
        for (index, column) in columns.iter().enumerate() {
            let start = 1 + index * 5;
            let kind: String = row.try_get(start)?;
            let size: i64 = row.try_get(start + 1)?;
            ensure!(
                size >= 0 && size as u64 <= MAX_CELL,
                "cell exceeds offline profile"
            );
            digest.field(column.name.as_bytes());
            digest.field(kind.as_bytes());
            match kind.as_str() {
                "null" => digest.field(&[]),
                "integer" => digest.field(&row.try_get::<i64, _>(start + 3)?.to_be_bytes()),
                "real" => digest.field(&row.try_get::<f64, _>(start + 4)?.to_bits().to_be_bytes()),
                "text" | "blob" => {
                    let bytes: Vec<u8> = row.try_get(start + 2)?;
                    ensure!(bytes.len() as i64 == size, "cell projection mismatch");
                    digest.field(&bytes);
                }
                _ => anyhow::bail!("unsupported SQLite value type"),
            }
        }
        hashes.push(digest.finish());
    }
    // Preserve duplicates; sorting fixed-size keyed hashes is independent of SQL
    // collation, insertion order, and VACUUM's physical btree layout.
    hashes.sort_unstable();
    let rows = hashes.len() as u64;
    let mut digest = Fingerprint::new(key, b"table-multiset")?;
    digest.field(name.as_bytes());
    digest.field(&serde_json::to_vec(&columns)?);
    digest.field(&rows.to_be_bytes());
    for hash in hashes {
        digest.field(&hash);
    }
    Ok(Table {
        name: name.into(),
        columns,
        rows,
        value_bytes,
        digest: hex::encode(digest.finish()),
    })
}
pub(super) const LEDGER_SQL: &str = "SELECT CASE WHEN typeof(version)='text' AND length(CAST(version AS BLOB))<=256 THEN version ELSE NULL END AS version,CASE WHEN typeof(applied_at)='integer' THEN applied_at ELSE NULL END AS applied_at,typeof(applied_at) AS kind FROM seaql_migrations ORDER BY version COLLATE BINARY LIMIT 1025";
pub(super) const INSTALLATION_SQL: &str = "SELECT CASE WHEN typeof(singleton)='integer' THEN singleton ELSE NULL END AS singleton,CASE WHEN typeof(installation_id)='text' AND length(CAST(installation_id AS BLOB))<=128 THEN installation_id ELSE NULL END AS installation_id FROM tenancy_installation LIMIT 2";

async fn ledger(db: &mut SqliteConnection) -> Result<Vec<LedgerEntry>> {
    let rows = sea_orm::sqlx::query(LEDGER_SQL).fetch_all(&mut *db).await?;
    ensure!(rows.len() <= 1024, "ledger exceeds profile");
    rows.into_iter()
        .map(|row| {
            let version: String = row.try_get("version")?;
            // A ledger field is metadata, never a place to print arbitrary payload.
            let mut fields = version.splitn(3, '_');
            let date = fields
                .next()
                .and_then(|v| v.strip_prefix('m'))
                .ok_or_else(|| anyhow::anyhow!("unsupported ledger version"))?;
            let number = fields
                .next()
                .ok_or_else(|| anyhow::anyhow!("unsupported ledger version"))?;
            let label = fields
                .next()
                .ok_or_else(|| anyhow::anyhow!("unsupported ledger version"))?;
            ensure!(
                date.len() == 8
                    && date.bytes().all(|b| b.is_ascii_digit())
                    && number.len() == 6
                    && number.bytes().all(|b| b.is_ascii_digit())
                    && !label.is_empty()
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "unsupported ledger version"
            );
            ensure!(
                row.try_get::<String, _>("kind")? == "integer",
                "unsupported ledger timestamp"
            );
            Ok(LedgerEntry {
                version,
                applied_at: row.try_get("applied_at")?,
            })
        })
        .collect()
}
async fn installation(db: &mut SqliteConnection) -> Result<Option<String>> {
    let rows = sea_orm::sqlx::query(INSTALLATION_SQL)
        .fetch_all(&mut *db)
        .await?;
    ensure!(rows.len() <= 1, "invalid installation singleton");
    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };
    ensure!(
        row.try_get::<i64, _>("singleton")? == 1,
        "invalid installation singleton"
    );
    let id: String = row.try_get("installation_id")?;
    let parsed = bitrouter_ai::tenancy::identity::InstallationId::try_from(id)?;
    Ok(Some(parsed.as_str().into()))
}

/// Narrow Eval profile. Preflight every table in one stable transaction before
/// any payload enters the generic inventory decoder. Other offline actions keep
/// their existing limits and exact evidence encoding.
#[cfg(feature = "core")]
pub(super) async fn inspect_eval(db: &mut SqliteConnection, key: &[u8; 32]) -> Result<Inventory> {
    let mut tx = db.begin().await?;
    let result = inspect_eval_in(&mut tx, key).await;
    tx.rollback().await?;
    result
}
#[cfg(feature = "core")]
pub(super) async fn inspect_eval_in(
    db: &mut SqliteConnection,
    key: &[u8; 32],
) -> Result<Inventory> {
    schema(db, key, &mut Budget::default()).await?;
    let names=sea_orm::sqlx::query("SELECT CASE WHEN length(CAST(name AS BLOB))<=256 THEN name ELSE NULL END AS name,type FROM pragma_table_list WHERE schema='main' AND name!='sqlite_schema' LIMIT 257").fetch_all(&mut *db).await?;
    ensure!(names.len() <= 256, "Eval inventory table limit");
    let mut definitions = Vec::new();
    for row in names {
        let name: String = row.try_get("name")?;
        identifier(&name)?;
        ensure!(
            row.try_get::<String, _>("type")? == "table",
            "unsupported table"
        );
        let cols = columns(db, &name).await?;
        indexes(db, &name).await?;
        definitions.push((name, cols));
    }
    let mut rows = 0_i64;
    let mut bytes = 0_i64;
    for (name, columns) in definitions {
        let sizes=columns.iter().map(|c|{
            let c=quoted(&c.name);
            format!("CASE typeof({c}) WHEN 'null' THEN 0 WHEN 'integer' THEN 8 WHEN 'real' THEN 8 ELSE length(CAST({c} AS BLOB)) END")
        }).collect::<Vec<_>>();
        let total = sizes.join(" + ");
        let valid = sizes
            .iter()
            .map(|s| format!("({s})<=1048576"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let sql = format!(
            "SELECT count(*) AS n,coalesce(sum({total}),0) AS bytes,coalesce(sum(CASE WHEN {valid} THEN 0 ELSE 1 END),0) AS bad FROM (SELECT * FROM {} LIMIT 4097)",
            quoted(&name)
        );
        let row = sea_orm::sqlx::query(&sql).fetch_one(&mut *db).await?;
        ensure!(
            row.try_get::<i64, _>("bad")? == 0,
            "Eval inventory cell bound"
        );
        rows = rows
            .checked_add(row.try_get("n")?)
            .ok_or_else(|| anyhow::anyhow!("row overflow"))?;
        bytes = bytes
            .checked_add(row.try_get("bytes")?)
            .ok_or_else(|| anyhow::anyhow!("byte overflow"))?;
        ensure!(
            (0..=4096).contains(&rows) && (0..=33554432).contains(&bytes),
            "Eval inventory profile exceeded"
        );
    }
    inspect_in(db, key).await
}
