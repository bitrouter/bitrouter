//! Typed SQLite values, bounded before they cross the driver boundary.
//! SQLite storage classes: <https://www.sqlite.org/datatype3.html>
//! BLOB casts preserve text bytes: <https://www.sqlite.org/lang_expr.html#castexpr>
use super::{
    files::Fingerprint,
    format::{Column, LedgerEntry, MAX_CELL, MAX_ROW},
};
use anyhow::{Context, Result, ensure};
use futures::TryStreamExt;
use sea_orm::sqlx::{Row, SqliteConnection};
#[cfg(all(test, unix))]
use {
    super::format::{Inventory, MAX_BYTES, MAX_FILE, MAX_ROWS, Table},
    sea_orm::sqlx::Connection,
};

pub(super) struct Budget {
    #[cfg(all(test, unix))]
    bounded_reference: bool,
    rows: u64,
    bytes: u64,
}
#[cfg(all(test, unix))]
impl Default for Budget {
    fn default() -> Self {
        Self {
            bounded_reference: true,
            rows: 0,
            bytes: 0,
        }
    }
}
impl Budget {
    pub(super) fn totals() -> Self {
        Self {
            #[cfg(all(test, unix))]
            bounded_reference: false,
            rows: 0,
            bytes: 0,
        }
    }
    pub(super) fn charge(&mut self, rows: u64, bytes: u64) -> Result<()> {
        self.rows = self
            .rows
            .checked_add(rows)
            .ok_or_else(|| anyhow::anyhow!("row count overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| anyhow::anyhow!("byte count overflow"))?;
        #[cfg(all(test, unix))]
        ensure!(
            !self.bounded_reference || (self.rows <= MAX_ROWS && self.bytes <= MAX_BYTES),
            "offline inventory exceeds profile"
        );
        Ok(())
    }
}
pub(super) fn identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
        "unsupported schema identifier"
    );
    Ok(())
}
pub(super) fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

#[cfg(all(test, unix))]
pub(super) async fn inspect(db: &mut SqliteConnection, key: &[u8; 32]) -> Result<Inventory> {
    let mut tx = db.begin().await?;
    let result = inspect_in(&mut tx, key).await;
    // Read outcome is not successful until the stable snapshot has been released.
    tx.rollback().await?;
    result
}
#[cfg(all(test, unix))]
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
pub(super) async fn schema(
    db: &mut SqliteConnection,
    key: &[u8; 32],
    budget: &mut Budget,
) -> Result<String> {
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
pub(super) async fn columns(db: &mut SqliteConnection, name: &str) -> Result<Vec<Column>> {
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
pub(super) async fn indexes(db: &mut SqliteConnection, table: &str) -> Result<()> {
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
pub(super) fn row_query(name: &str, columns: &[Column]) -> String {
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

/// The caller has qualified every schema object and bounded this table's cells
/// in the same snapshot. Keep operand and statement allowances until clear ACK.
pub(super) async fn validate_checks_in(db: &mut SqliteConnection, name: &str) -> Result<()> {
    let sql: Option<String> = sea_orm::sqlx::query_scalar(
        "SELECT CASE WHEN length(CAST(sql AS BLOB))<=1048576 THEN sql ELSE NULL END FROM main.sqlite_schema WHERE type='table' AND name=?",
    )
    .bind(name)
    .fetch_one(&mut *db)
    .await?;
    let sql = sql.context("table definition missing or exceeds profile")?;
    let mut checks = super::schema::CheckCursor::new(&sql)?;
    while let Some(predicate) = checks.next_predicate()? {
        // Readonly SQLite can omit CHECKs from integrity_check. Evaluate only
        // accepted original spans, with no aliases or normalization: affinity,
        // collation, TRUE/FALSE name resolution and CHECK-NULL remain SQLite's.
        // https://sqlite.org/lang_createtable.html#check_constraints
        // https://sqlite.org/lang_expr.html#booleanexpr
        let query = format!(
            "SELECT EXISTS(SELECT 1 FROM main.{} NOT INDEXED WHERE NOT{predicate})",
            quoted(name)
        );
        let violated: i64 = sea_orm::sqlx::query_scalar(&query)
            .fetch_one(&mut *db)
            .await?;
        ensure!(violated == 0, "SQLite integrity failed");
    }
    Ok(())
}

#[cfg(all(test, unix))]
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
        hashes.push(row_digest(&row, name, &columns, key)?);
    }
    drop(stream);
    // Every row/cell has passed the bounded projection before CHECK evaluation.
    validate_checks_in(db, name).await?;
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
pub(super) fn row_digest(
    row: &sea_orm::sqlx::sqlite::SqliteRow,
    name: &str,
    columns: &[Column],
    key: &[u8; 32],
) -> Result<[u8; 32]> {
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
    Ok(digest.finish())
}
pub(super) const LEDGER_SQL: &str = "SELECT CASE WHEN typeof(version)='text' AND length(CAST(version AS BLOB))<=256 THEN version ELSE NULL END AS version,CASE WHEN typeof(applied_at)='integer' THEN applied_at ELSE NULL END AS applied_at,typeof(applied_at) AS kind FROM seaql_migrations ORDER BY version COLLATE BINARY LIMIT 1025";
pub(super) const INSTALLATION_SQL: &str = "SELECT CASE WHEN typeof(singleton)='integer' THEN singleton ELSE NULL END AS singleton,CASE WHEN typeof(installation_id)='text' AND length(CAST(installation_id AS BLOB))<=128 THEN installation_id ELSE NULL END AS installation_id FROM tenancy_installation LIMIT 2";

pub(super) async fn ledger(db: &mut SqliteConnection) -> Result<Vec<LedgerEntry>> {
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
pub(super) async fn installation(db: &mut SqliteConnection) -> Result<Option<String>> {
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
