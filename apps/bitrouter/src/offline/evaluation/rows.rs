//! Finite exact SQLite value projection for the four-table Legacy Eval graph.
use super::super::files::Fingerprint;
use anyhow::{Result, ensure};
use sea_orm::sqlx::{Row as _, SqliteConnection};

pub(crate) const MAX_ROWS: usize = 4096;
pub(crate) const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_CELL: usize = 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Cell {
    Null,
    Integer(i64),
    Text(String),
}
#[derive(Clone)]
pub(crate) struct Row {
    pub cells: Vec<Cell>,
    pub digest: [u8; 32],
}
impl Row {
    pub(crate) fn text(&self, index: usize) -> Result<&str> {
        match self.cells.get(index) {
            Some(Cell::Text(value)) => Ok(value),
            _ => anyhow::bail!("Eval text storage class"),
        }
    }
    pub(crate) fn integer(&self, index: usize) -> Result<i64> {
        match self.cells.get(index) {
            Some(Cell::Integer(value)) => Ok(*value),
            _ => anyhow::bail!("Eval integer storage class"),
        }
    }
    pub(crate) fn nullable(&self, index: usize) -> Result<Option<String>> {
        match self.cells.get(index) {
            Some(Cell::Null) => Ok(None),
            Some(Cell::Text(value)) => Ok(Some(value.clone())),
            _ => anyhow::bail!("Eval ownership storage class"),
        }
    }
}
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Null,
    Integer,
    Text,
}
#[derive(Clone, Copy)]
pub(crate) struct Field {
    pub name: &'static str,
    pub kind: Kind,
}
macro_rules! table {
    ($name:ident, $table:literal, [$($field:literal : $kind:ident),* $(,)?]) => {
        pub(crate) const $name: Table = Table { name: $table, fields: &[
            Field { name: "installation_id", kind: Kind::Null },
            Field { name: "tenant_id", kind: Kind::Null },
            Field { name: "creator_principal_id", kind: Kind::Null },
            $(Field { name: $field, kind: Kind::$kind }),*
        ] };
    }
}
#[derive(Clone, Copy)]
pub(crate) struct Table {
    pub name: &'static str,
    pub fields: &'static [Field],
}
table!(SUBJECTS, "eval_subjects", ["eval_id":Text,"owner_user_id":Text,"subject_id":Text,"scope":Text,"policy_digest":Text,"holdout":Integer,"content_digest":Text,"subject_json":Text,"created_at":Text]);
table!(RESULTS, "eval_results", ["result_id":Text,"owner_user_id":Text,"eval_id":Text,"idempotency_key":Text,"authority_id":Text,"evaluator_kind":Text,"content_digest":Text,"result_json":Text,"created_at":Text]);
table!(ADMISSIONS, "eval_admission_events", ["sequence":Integer,"result_id":Text,"status":Text,"reason":Text,"authority_id":Text,"created_at":Text]);
table!(SNAPSHOTS, "eval_snapshots", ["evidence_root":Text,"owner_user_id":Text,"manifest_json":Text,"result_count":Integer,"frozen_at":Text]);
pub(crate) const TABLES: [Table; 4] = [SUBJECTS, RESULTS, ADMISSIONS, SNAPSHOTS];

pub(crate) struct Tables {
    pub rows: [Vec<Row>; 4],
    pub row_count: usize,
    pub cell_bytes: usize,
}
fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
fn size(field: &Field) -> String {
    match field.kind {
        Kind::Null => "0".into(),
        Kind::Integer => "8".into(),
        Kind::Text => format!("length(CAST({} AS BLOB))", quoted(field.name)),
    }
}
fn valid(field: &Field) -> String {
    let col = quoted(field.name);
    match field.kind {
        Kind::Null => format!("typeof({col})='null'"),
        Kind::Integer => format!("typeof({col})='integer'"),
        Kind::Text => format!("typeof({col})='text' AND length(CAST({col} AS BLOB))<={MAX_CELL}"),
    }
}

/// Caller owns one stable read transaction after complete M4a schema checks.
/// Preflight ALL four tables before any payload crosses the driver boundary.
pub(crate) async fn load(db: &mut SqliteConnection, key: &[u8; 32]) -> Result<Tables> {
    let mut rows = 0_usize;
    let mut bytes = 0_usize;
    for table in TABLES {
        // Exact selected column set; an added payload column cannot silently be
        // dropped by an older converter. The SQL identifiers below are constants.
        let columns: Vec<String> = sea_orm::sqlx::query_scalar(
            "SELECT name FROM pragma_table_xinfo(?) ORDER BY name COLLATE BINARY LIMIT 33",
        )
        .bind(table.name)
        .fetch_all(&mut *db)
        .await?;
        let mut expected: Vec<&str> = table.fields.iter().map(|f| f.name).collect();
        expected.sort_unstable();
        ensure!(
            columns
                .iter()
                .map(String::as_str)
                .eq(expected.iter().copied()),
            "unsupported Eval schema"
        );
        let valid = table
            .fields
            .iter()
            .map(valid)
            .collect::<Vec<_>>()
            .join(" AND ");
        let size = table
            .fields
            .iter()
            .map(size)
            .collect::<Vec<_>>()
            .join(" + ");
        let sql = format!(
            "SELECT count(*) AS n,coalesce(sum(CASE WHEN {valid} THEN ({size}) ELSE 0 END),0) AS bytes,coalesce(sum(CASE WHEN {valid} THEN 0 ELSE 1 END),0) AS invalid FROM (SELECT * FROM {} LIMIT {})",
            quoted(table.name),
            MAX_ROWS + 1
        );
        let row = sea_orm::sqlx::query(&sql).fetch_one(&mut *db).await?;
        ensure!(
            row.try_get::<i64, _>("invalid")? == 0,
            "mixed ownership or unsupported Eval cell"
        );
        rows = rows
            .checked_add(usize::try_from(row.try_get::<i64, _>("n")?)?)
            .ok_or_else(|| anyhow::anyhow!("Eval count overflow"))?;
        bytes = bytes
            .checked_add(usize::try_from(row.try_get::<i64, _>("bytes")?)?)
            .ok_or_else(|| anyhow::anyhow!("Eval byte overflow"))?;
        ensure!(
            rows <= MAX_ROWS && bytes <= MAX_BYTES,
            "Eval graph bound exceeded"
        );
    }
    let mut output: [Vec<Row>; 4] = std::array::from_fn(|_| Vec::new());
    let mut seen_bytes = 0_usize;
    let mut seen_rows = 0_usize;
    for (index, table) in TABLES.into_iter().enumerate() {
        let select = table
            .fields
            .iter()
            .map(|f| quoted(f.name))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT {select} FROM {} LIMIT {}",
            quoted(table.name),
            MAX_ROWS + 1
        );
        use futures::TryStreamExt;
        let mut stream = sea_orm::sqlx::query(&sql).fetch(&mut *db);
        while let Some(row) = stream.try_next().await? {
            seen_rows += 1;
            ensure!(seen_rows <= rows, "Eval source changed");
            let mut cells = Vec::new();
            for (i, field) in table.fields.iter().enumerate() {
                let cell = match field.kind {
                    Kind::Null => {
                        ensure!(
                            row.try_get::<Option<String>, _>(i)?.is_none(),
                            "ownership changed"
                        );
                        Cell::Null
                    }
                    Kind::Integer => {
                        seen_bytes = seen_bytes
                            .checked_add(8)
                            .ok_or_else(|| anyhow::anyhow!("byte overflow"))?;
                        Cell::Integer(row.try_get(i)?)
                    }
                    Kind::Text => {
                        let value: String = row.try_get(i)?;
                        ensure!(value.len() <= MAX_CELL, "cell grew");
                        seen_bytes = seen_bytes
                            .checked_add(value.len())
                            .ok_or_else(|| anyhow::anyhow!("byte overflow"))?;
                        Cell::Text(value)
                    }
                };
                cells.push(cell);
            }
            ensure!(seen_bytes <= bytes, "Eval source changed");
            let digest = digest(table, &cells, key)?;
            output[index].push(Row { cells, digest });
        }
    }
    ensure!(
        seen_rows == rows && seen_bytes == bytes,
        "Eval source changed"
    );
    Ok(Tables {
        rows: output,
        row_count: rows,
        cell_bytes: bytes,
    })
}
pub(crate) fn digest(table: Table, cells: &[Cell], key: &[u8; 32]) -> Result<[u8; 32]> {
    ensure!(cells.len() == table.fields.len(), "Eval row width");
    let mut digest = Fingerprint::new(key, b"eval-partition.row.v1")?;
    digest.field(table.name.as_bytes());
    for (field, cell) in table.fields.iter().zip(cells) {
        digest.field(field.name.as_bytes());
        match cell {
            Cell::Null => {
                digest.field(b"null");
                digest.field(&[])
            }
            Cell::Integer(value) => {
                digest.field(b"integer");
                digest.field(&value.to_be_bytes())
            }
            Cell::Text(value) => {
                digest.field(b"text");
                digest.field(value.as_bytes())
            }
        }
    }
    Ok(digest.finish())
}

/// Compare exact transformed values and totals, including storage classes, while
/// the writer still owns the same transaction. SQL preflight precedes payloads.
pub(crate) async fn verify(
    db: &mut SqliteConnection,
    table: Table,
    expected: &[Vec<Cell>],
    key: &[u8; 32],
) -> Result<()> {
    let fields = table
        .fields
        .iter()
        .map(|f| Field {
            name: f.name,
            kind: match f.kind {
                Kind::Null => Kind::Text,
                other => other,
            },
        })
        .collect::<Vec<_>>();
    let check = fields.iter().map(valid).collect::<Vec<_>>().join(" AND ");
    let sql = format!(
        "SELECT count(*) AS n,coalesce(sum(CASE WHEN {check} THEN 0 ELSE 1 END),0) AS bad FROM (SELECT * FROM {} LIMIT {})",
        quoted(table.name),
        MAX_ROWS + 1
    );
    let n = sea_orm::sqlx::query(&sql).fetch_one(&mut *db).await?;
    ensure!(
        n.try_get::<i64, _>("n")? == expected.len() as i64 && n.try_get::<i64, _>("bad")? == 0,
        "imported count/type mismatch"
    );
    // The total allocation must equal the precomputed expected transfer size;
    // a changed 1MiB-per-cell collection cannot multiply the 32MiB allowance.
    let size = fields.iter().map(size).collect::<Vec<_>>().join(" + ");
    let bytes: i64 = sea_orm::sqlx::query_scalar(&format!(
        "SELECT coalesce(sum({size}),0) FROM {}",
        quoted(table.name)
    ))
    .fetch_one(&mut *db)
    .await?;
    let expected_bytes: usize = expected
        .iter()
        .flatten()
        .map(|c| match c {
            Cell::Null => 0,
            Cell::Integer(_) => 8,
            Cell::Text(s) => s.len(),
        })
        .sum();
    ensure!(
        usize::try_from(bytes)? == expected_bytes && expected_bytes <= MAX_BYTES,
        "imported byte mismatch"
    );
    let sql = format!(
        "SELECT {} FROM {} LIMIT {}",
        fields
            .iter()
            .map(|f| quoted(f.name))
            .collect::<Vec<_>>()
            .join(","),
        quoted(table.name),
        MAX_ROWS + 1
    );
    let mut expected = expected
        .iter()
        .map(|r| digest(table, r, key))
        .collect::<Result<Vec<_>>>()?;
    expected.sort_unstable();
    let mut actual = Vec::new();
    use futures::TryStreamExt;
    let mut stream = sea_orm::sqlx::query(&sql).fetch(&mut *db);
    while let Some(row) = stream.try_next().await? {
        ensure!(actual.len() < expected.len(), "extra imported row");
        let cells = fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                Ok(match f.kind {
                    Kind::Integer => Cell::Integer(row.try_get(i)?),
                    Kind::Text => Cell::Text(row.try_get(i)?),
                    Kind::Null => anyhow::bail!("invalid target projection"),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        actual.push(digest(table, &cells, key)?);
    }
    actual.sort_unstable();
    ensure!(actual == expected, "imported full content digest mismatch");
    Ok(())
}

pub(crate) async fn insert(db: &mut SqliteConnection, table: Table, row: &[Cell]) -> Result<()> {
    ensure!(row.len() == table.fields.len(), "import row width");
    // All identifiers come from the fixed schema. Values are always bound.
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        quoted(table.name),
        table
            .fields
            .iter()
            .map(|f| quoted(f.name))
            .collect::<Vec<_>>()
            .join(","),
        vec!["?"; row.len()].join(",")
    );
    let mut query = sea_orm::sqlx::query(&sql);
    for cell in row {
        query = match cell {
            Cell::Null => query.bind(Option::<String>::None),
            Cell::Integer(n) => query.bind(*n),
            Cell::Text(s) => query.bind(s),
        };
    }
    ensure!(
        query.execute(db).await?.rows_affected() == 1,
        "import insert count"
    );
    Ok(())
}
