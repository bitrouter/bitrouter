use super::{CapacityExceeded, Charge, Workspace, phase::Phase, sort};
use crate::offline::{
    files::Fingerprint,
    format::{Column, Inventory, MAX_CELL, MAX_ROW, Table},
    inventory as shared,
};
use anyhow::{Context, Result, ensure};
use futures::TryStreamExt;
use sea_orm::sqlx::{Row, SqliteConnection};

pub(crate) struct Inspected {
    value: Inventory,
    charge: Charge,
}
impl Inspected {
    pub(crate) fn value(&self) -> &Inventory {
        &self.value
    }
    pub(crate) fn into_parts(self) -> (Inventory, Charge) {
        (self.value, self.charge)
    }
}

/// The caller owns a stable read transaction and a one-row SQLx output buffer.
/// Every connection's cache/driver allowance must already be held by its owner
/// from before open through acknowledged close (Connections::open_budgeted).
/// This function never initializes, commits, rolls back or closes that worker.
pub(super) async fn inspect_in(
    db: &mut SqliteConnection,
    key: &[u8; 32],
    workspace: &Workspace,
) -> Result<Inspected> {
    workspace.check()?;
    let mut retained = workspace.reserve(256 * (std::mem::size_of::<Table>() + 512) + 65536)?;
    let encoding: String = sea_orm::sqlx::query_scalar("PRAGMA encoding")
        .fetch_one(&mut *db)
        .await?;
    ensure!(
        matches!(encoding.as_str(), "UTF-8" | "UTF-16le" | "UTF-16be"),
        "unsupported SQLite encoding"
    );
    let schema_digest = {
        // Schema SQL is projected twice by the shared decoder. Cover queued,
        // worker and consumer rows, tokenization and transient field copies.
        let phase = Phase::new(workspace, 12 * 1024 * 1024)?;
        let result = shared::schema(db, key, &mut shared::Budget::totals()).await;
        phase.clear(db).await?;
        result?
    };
    workspace.check()?;
    let mut definitions = Vec::new();
    definitions
        .try_reserve_exact(256)
        .map_err(|_| CapacityExceeded)?;
    {
        let phase = Phase::new(workspace, 4 * 1024 * 1024)?;
        let result = async {
        let rows = sea_orm::sqlx::query("SELECT CASE WHEN length(CAST(name AS BLOB))<=256 THEN name ELSE NULL END AS name,type FROM pragma_table_list WHERE schema='main' AND name!='sqlite_schema' AND type!='view' ORDER BY name COLLATE BINARY LIMIT 257")
            .fetch_all(&mut *db).await?;
        ensure!(rows.len() <= 256, "too many tables");
        for row in rows {
            workspace.check()?;
            let name: String = row.try_get("name")?;
            shared::identifier(&name)?;
            ensure!(
                row.try_get::<String, _>("type")? == "table",
                "virtual/shadow tables are outside offline profile"
            );
            // Retained schema metadata only, not a retained-data cardinality cap.
            // Reserve before columns() materializes its schema-bounded vector.
            retained.grow(128 * (std::mem::size_of::<Column>() + 512))?;
            let columns = shared::columns(db, &name).await?;
            shared::indexes(db, &name).await?;
            definitions.push((name, columns));
        }
        Ok::<_, anyhow::Error>(())
        }.await;
        phase.clear(db).await?;
        result?;
    }
    let mut tables = Vec::new();
    tables
        .try_reserve_exact(definitions.len())
        .map_err(|_| CapacityExceeded)?;
    let mut rows = 0_u64;
    let mut value_bytes = 0_u64;
    // Every expression is qualified before reading any table or evaluating
    // integrity. Hash runs retain duplicates and exactly the v1 framing.
    for (name, columns) in definitions {
        let table = table(db, name, columns, key, workspace).await?;
        rows = rows
            .checked_add(table.rows)
            .context("inventory row count overflow")?;
        value_bytes = value_bytes
            .checked_add(table.value_bytes)
            .context("inventory byte count overflow")?;
        tables.push(table);
    }
    workspace.check()?;
    // The TVF quotes its argument, so (1) selects a table named "1" rather
    // than PRAGMA's numeric error limit. Reduce its no-argument result in SQL:
    // only one integer crosses the driver, even for large corruption messages.
    // https://sqlite.org/pragma.html#pragma_integrity_check
    let integrity: i64 = sea_orm::sqlx::query_scalar(
        "SELECT count(*)=1 AND coalesce(min(integrity_check='ok'),0) FROM pragma_integrity_check",
    )
    .fetch_one(&mut *db)
    .await?;
    ensure!(integrity == 1, "SQLite integrity failed");
    let ledger = if tables.iter().any(|table| table.name == "seaql_migrations") {
        retained.grow(1024 * (std::mem::size_of::<crate::offline::format::LedgerEntry>() + 512))?;
        let phase = Phase::new(workspace, 2 * 1024 * 1024)?;
        let result = shared::ledger(db).await;
        phase.clear(db).await?;
        Some(result?)
    } else {
        None
    };
    let installation_id = if tables
        .iter()
        .any(|table| table.name == "tenancy_installation")
    {
        shared::installation(db).await?
    } else {
        None
    };
    workspace.check()?;
    Ok(Inspected {
        value: Inventory {
            encoding,
            schema_digest,
            tables,
            rows,
            value_bytes,
            ledger,
            installation_id,
        },
        charge: retained,
    })
}

// SQL text construction repeats the row-size expression once per column.
// Count the actual identifier lengths before creating that quadratic string.
fn query_bytes(columns: &[Column]) -> Result<usize> {
    let sizes = columns.iter().try_fold(0_usize, |total, column| {
        let size = column
            .name
            .len()
            .checked_mul(4)
            .and_then(|bytes| bytes.checked_add(160));
        total
            .checked_add(size.ok_or(CapacityExceeded)?)
            .ok_or(CapacityExceeded)
    })?;
    sizes
        .checked_mul(columns.len().checked_add(8).ok_or(CapacityExceeded)?)
        .and_then(|bytes| bytes.checked_mul(5))
        .and_then(|bytes| bytes.checked_add(65536))
        .ok_or_else(|| CapacityExceeded.into())
}

async fn table(
    db: &mut SqliteConnection,
    name: String,
    columns: Vec<Column>,
    key: &[u8; 32],
    workspace: &Workspace,
) -> Result<Table> {
    workspace.check()?;
    // Includes simultaneous SQL builder intermediates and column JSON framing.
    let query_memory = query_bytes(&columns)?;
    let mut phase = Phase::new(workspace, query_memory)?;
    let result = async {
    let sizes = columns.iter().map(|column| {
        let column = shared::quoted(&column.name);
        format!("CASE typeof({column}) WHEN 'null' THEN 0 WHEN 'integer' THEN 8 WHEN 'real' THEN 8 ELSE length(CAST({column} AS BLOB)) END")
    }).collect::<Vec<_>>();
        let cells = sizes.iter().map(|size| format!("coalesce(max({size}),0)")).collect::<Vec<_>>().join(",");
        let maximum = format!(
            "SELECT coalesce(max({}),0),{} FROM {}",
        sizes.join(" + "),
            cells,
        shared::quoted(&name)
    );
    // Scalar lengths only: a legal body that cannot fit is a capacity refusal,
    // before any body crosses the driver boundary. Snapshot stability is required.
        let maximum = sea_orm::sqlx::query(&maximum)
        .fetch_one(&mut *db)
        .await?;
        for index in 1..=columns.len() {
            let cell: i64 = maximum.try_get(index)?;
            ensure!(cell >= 0 && cell as u64 <= MAX_CELL, "cell exceeds offline profile");
        }
        let maximum: i64 = maximum.try_get(0)?;
    ensure!(
        maximum >= 0 && maximum as u64 <= MAX_ROW,
        "row exceeds offline profile"
    );
    drop(sizes);
    let row_memory = usize::try_from(maximum)?
        .checked_add(columns.len().checked_mul(1024).ok_or(CapacityExceeded)?)
        .and_then(|bytes| bytes.checked_add(4096))
        .and_then(|bytes| bytes.checked_mul(5))
        .ok_or(CapacityExceeded)?;
    // Consumer, queued and worker rows, SQLite result values, and copied cell.
    phase.grow(row_memory)?;
    {
        // Retain the actual operand/VM envelope above. As in schema(), 12 MiB
        // covers bounded DDL/driver copies, token strings/spans and one original
        // predicate/SQL statement, including UTF-16 to UTF-8 expansion. Clear
        // both success and error before allocating the hash-sort working set.
        let checks = Phase::new(workspace, 12 * 1024 * 1024)?;
        let checked = shared::validate_checks_in(db, &name).await;
        checks.clear(db).await?;
        checked?;
    }
    let mut hashes = sort::Records::<32>::new(workspace, workspace.available() / 2)?;
    let mut value_bytes = 0_u64;
    {
        let query = shared::row_query(&name, &columns);
        let mut stream = sea_orm::sqlx::query(&query).fetch(&mut *db);
        while let Some(row) = stream.try_next().await? {
            workspace.check()?;
            let bytes: i64 = row.try_get(0)?;
            ensure!(bytes >= 0 && bytes <= maximum, "inventory snapshot changed");
            value_bytes = value_bytes
                .checked_add(bytes as u64)
                .context("inventory byte count overflow")?;
            hashes.push(shared::row_digest(&row, &name, &columns, key)?)?;
        }
    }
    Ok::<_, anyhow::Error>((hashes, value_bytes))
    }.await;
    phase.clear(db).await?;
    let (hashes, value_bytes) = result?;
    // Column framing is application memory after the SQL statement barrier.
    let _columns_json = workspace.reserve(query_memory)?;
    let mut hashes = hashes.finish()?;
    let rows = hashes.len();
    let mut digest = Fingerprint::new(key, b"table-multiset")?;
    digest.field(name.as_bytes());
    digest.field(&serde_json::to_vec(&columns)?);
    digest.field(&rows.to_be_bytes());
    while let Some(hash) = hashes.next()? {
        digest.field(&hash);
    }
    Ok(Table {
        name,
        columns,
        rows,
        value_bytes,
        digest: hex::encode(digest.finish()),
    })
}
