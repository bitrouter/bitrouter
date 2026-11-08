//! Complete authority validation without retaining a directory or history map.
//!
//! The caller owns a fixed read transaction or exclusive stopped/startup custody.
//! No helper creates, commits, times out or abandons that custody. Returning an
//! error (including CapacityExceeded) never permits partially validated service.

mod operations;
mod retained;
mod write;

use anyhow::{Context, Result, ensure};
use sea_orm::{
    ConnectionTrait, QueryResult,
    sea_query::{Alias, Expr, Order, SelectStatement},
};

use super::directory::{
    self, CapacityExceeded, PageBudget,
    query::{self, Table},
};
use crate::tenancy::model::AuthorityHeader;

// These are properties of the historical v1 format, never v2 admission limits.
pub(in crate::tenancy) const LEGACY_ORDINARY_OPERATIONS: u64 = 99_488;
pub(in crate::tenancy) const LEGACY_SAFETY_OPERATIONS: u64 = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tenancy) enum Format {
    V1,
    V2,
    V3,
}

impl Format {
    fn number(self) -> i32 {
        match self {
            Self::V1 => 1,
            Self::V2 => 2,
            Self::V3 => 3,
        }
    }
}

/// Small publication header with the explicitly retained internal store format.
pub(in crate::tenancy) struct LoadedHeader {
    pub header: AuthorityHeader,
    pub format: Format,
}

/// Aggregate transient working reservation, independent of total directory size.
/// `rows` controls a metadata keyset page. Keep 32x encoded metadata bytes for
/// decoded facts and linked reads. Immutable bodies are decoded one at a time
/// against the remaining full working allowance, independently of page width.
/// This is conservative payload accounting, not an allocator/RSS measurement.
#[derive(Clone, Copy)]
pub(in crate::tenancy) struct Budget {
    pub rows: usize,
    pub bytes: usize,
}

impl Budget {
    fn page(self) -> Result<PageBudget> {
        let slots = self.rows.checked_add(4).ok_or(CapacityExceeded)?;
        let metadata = slots.checked_mul(4096).ok_or(CapacityExceeded)?;
        let available = self.bytes.checked_sub(metadata).ok_or(CapacityExceeded)?;
        let divisor = slots.checked_mul(32).ok_or(CapacityExceeded)?;
        let row = available.checked_div(divisor).ok_or(CapacityExceeded)?;
        if self.rows == 0 || row == 0 || self.bytes > isize::MAX as usize {
            return Err(CapacityExceeded.into());
        }
        Ok(PageBudget {
            rows: self.rows,
            bytes: row.checked_mul(self.rows).ok_or(CapacityExceeded)?,
        })
    }
}

fn decode_header(row: &QueryResult) -> Result<LoadedHeader> {
    ensure!(
        row.try_get::<i32>("", "singleton")? == 1,
        "invalid authority singleton"
    );
    let format = match row.try_get::<i32>("", "schema_version")? {
        1 => Format::V1,
        2 => Format::V2,
        3 => Format::V3,
        _ => anyhow::bail!("unsupported authority store format"),
    };
    let created_at = super::string(row, "created_at")?;
    chrono::DateTime::parse_from_rfc3339(&created_at)?;
    let loaded = LoadedHeader {
        format,
        header: AuthorityHeader {
            installation: super::string(row, "installation_id")?.try_into()?,
            revision: u64::try_from(row.try_get::<i64>("", "authority_revision")?)?.try_into()?,
            operation_ids: u64::try_from(row.try_get::<i64>("", "operation_ids")?)?,
            safety_operation_ids: u64::try_from(row.try_get::<i64>("", "safety_operation_ids")?)?,
            saturated: row.try_get("", "saturated")?,
            created_at,
        },
    };
    if format == Format::V1 {
        ensure!(
            loaded.header.operation_ids <= LEGACY_ORDINARY_OPERATIONS
                && loaded.header.safety_operation_ids <= LEGACY_SAFETY_OPERATIONS,
            "invalid legacy operation counters"
        );
        ensure!(
            loaded.header.operation_ids < LEGACY_ORDINARY_OPERATIONS || loaded.header.saturated,
            "missing legacy saturation latch"
        );
    }
    Ok(loaded)
}

/// Read only metadata; this alone is NOT a completed startup validation.
/// `bytes` bounds each encoded row, including the duplicate-singleton probe.
pub(in crate::tenancy) async fn read_header_in(
    db: &impl ConnectionTrait,
    bytes: usize,
) -> Result<Option<LoadedHeader>> {
    query::one(db, query::select(db, Table::Header, bytes)?, decode_header).await
}

/// Validate every core authority row, returning only its small header/format.
/// No format upgrade, saturation reset, history deletion or initialization occurs.
/// `None` means all core authority tables are empty; companion recovery owners
/// must still validate their own epochs, obligations and artifacts before ready.
pub(in crate::tenancy) async fn load_header_in(
    db: &impl ConnectionTrait,
    budget: Budget,
) -> Result<Option<LoadedHeader>> {
    Ok(load_header_observed_in(db, budget)
        .await?
        .map(|(loaded, _)| loaded))
}

/// Counts describe this complete validation snapshot, never subsequent live mutations.
pub(in crate::tenancy) async fn load_header_observed_in(
    db: &impl ConnectionTrait,
    budget: Budget,
) -> Result<Option<(LoadedHeader, directory::DirectoryTotals)>> {
    let page = budget.page()?;
    let bytes = page.bytes / page.rows;
    let Some(loaded) = read_header_in(db, bytes).await? else {
        for table in [
            Table::Tenant,
            Table::Principal,
            Table::Binding,
            Table::Credential,
            Table::Operation,
            Table::Lifecycle,
            Table::Resource,
            Table::ConfigurationBody,
            Table::Active,
        ] {
            // Only a scalar crosses the driver even for corrupt, oversized cells.
            let query = sea_orm::sea_query::Query::select()
                .expr(Expr::val(1_i32))
                .from(Alias::new(table.name()))
                .limit(1)
                .to_owned();
            ensure!(
                db.query_one(db.get_database_backend().build(&query))
                    .await?
                    .is_none(),
                "authority has no installation identity"
            );
        }
        return Ok(None);
    };
    let started = std::time::Instant::now();
    tracing::info!(
        phase = "directory",
        outcome = "started",
        "tenant authority validation phase"
    );
    let totals = directory::validate_directory_in(db, page).await?;
    tracing::info!(
        phase = "directory",
        outcome = "validated",
        elapsed_ms = started.elapsed().as_millis() as u64,
        "tenant authority validation phase"
    );
    let started = std::time::Instant::now();
    tracing::info!(
        phase = "operations",
        outcome = "started",
        "tenant authority validation phase"
    );
    operations::validate_in(db, &loaded.header, budget).await?;
    tracing::info!(
        phase = "operations",
        outcome = "validated",
        elapsed_ms = started.elapsed().as_millis() as u64,
        "tenant authority validation phase"
    );
    let started = std::time::Instant::now();
    tracing::info!(
        phase = "retained",
        outcome = "started",
        "tenant authority validation phase"
    );
    retained::validate_in(db, &loaded.header, budget).await?;
    tracing::info!(
        phase = "retained",
        outcome = "validated",
        elapsed_ms = started.elapsed().as_millis() as u64,
        "tenant authority validation phase"
    );
    Ok(Some((loaded, totals)))
}

/// Keyset traversal always addresses qualified source columns, never CASE aliases.
/// Composite tuple seeks follow the existing primary key without OFFSET scans.
/// https://sqlite.org/rowvalue.html#scrolling_window_queries
fn page_query(
    db: &impl ConnectionTrait,
    table: Table,
    keys: &[&str],
    after: Option<&[String]>,
    budget: PageBudget,
) -> Result<SelectStatement> {
    order_page(
        query::select(db, table, budget.bytes / budget.rows)?,
        table,
        keys,
        after,
        budget.rows,
    )
}

fn order_page(
    mut query: SelectStatement,
    table: Table,
    keys: &[&str],
    after: Option<&[String]>,
    rows: usize,
) -> Result<SelectStatement> {
    let columns = || {
        keys.iter()
            .map(|key| Expr::col((Alias::new(table.name()), Alias::new(*key))))
    };
    if let Some(after) = after {
        ensure!(after.len() == keys.len(), "invalid authority page cursor");
        if keys.len() == 1 {
            query.and_where(
                columns()
                    .next()
                    .context("missing cursor column")?
                    .gt(after[0].as_str()),
            );
        } else {
            query.and_where(Expr::tuple(columns().map(Into::into)).gt(Expr::tuple(
                after.iter().map(|value| Expr::val(value.as_str()).into()),
            )));
        }
    }
    for key in keys {
        query.order_by((Alias::new(table.name()), Alias::new(*key)), Order::Asc);
    }
    query.limit(u64::try_from(rows)?);
    Ok(query)
}

/// Write metadata in caller-owned transaction custody, preserving format and stop.
pub(in crate::tenancy) async fn write_header_in(
    db: &impl ConnectionTrait,
    next: &LoadedHeader,
) -> Result<()> {
    write::write_header_in(db, next).await
}

/// Format-only transition after complete validation in the SAME exclusive
/// transaction. Healthy startup may commit it automatically. A stopped legacy
/// latch needs the separate acknowledged offline action; this never clears it.
pub(in crate::tenancy) async fn upgrade_format_in(
    db: &impl ConnectionTrait,
    validated: &LoadedHeader,
) -> Result<()> {
    write::upgrade_format_in(db, validated).await
}

/// Header-based equivalent of the legacy writer, under the original transaction.
pub(in crate::tenancy) async fn write_in(
    tx: &sea_orm::DatabaseTransaction,
    next: &LoadedHeader,
    changes: &[super::Change],
    operation: &super::Operation,
) -> Result<()> {
    for change in changes {
        super::write_change(tx, change).await?;
    }
    write_header_in(tx, next).await?;
    super::insert_operation_in(tx, &next.header.installation, operation).await
}
