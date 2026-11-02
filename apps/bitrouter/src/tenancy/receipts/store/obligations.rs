//! Derived unknown obligations; no activation, commit or admission publication.
//! The exclusive writer owns source transitions and the caller transaction.
use super::*;
use archive::Charge;
use chrono::{DateTime, Utc};
use sea_orm::{Statement, Value};
mod bootstrap;
mod io;
pub(in crate::tenancy) mod sources;
#[cfg(test)]
pub(crate) mod tests;
mod validation;

const TABLE: &str = "tenancy_unknown_obligations";
const RECEIPT: u8 = 1;
const AUXILIARY: u8 = 2;
const PENDING: u8 = 4;
const PAGE: usize = 32;
/// Maximum small keys/records in one validation page, not total scan work.
pub(in crate::tenancy) const VALIDATION_PAGE_ROWS: usize = PAGE;

/// Migration043 only: backfill canonical obligations before its schema/ledger
/// transaction commits. Never call from normal startup or an empty-table probe.
/// Requires the existing exclusive database/migration owner; no format flip.
pub(crate) async fn backfill_migration(tx: &DatabaseTransaction) -> Result<()> {
    bootstrap::run(tx).await
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::tenancy) struct Key {
    pub tenant: TenantId,
    pub execution: ExecutionId,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tenancy) struct Record {
    pub key: Key,
    pub source_mask: u8,
    pub first: DateTime<Utc>,
    pub due: DateTime<Utc>,
    pub escalated: bool,
}
impl Record {
    fn validate(&self) -> Result<()> {
        ensure!((1..=7).contains(&self.source_mask), "unknown source mask");
        ensure!(
            self.first.timestamp_subsec_nanos() < 1_000_000_000
                && self.due.timestamp_subsec_nanos() < 1_000_000_000
                && due(self.first)? == self.due,
            "unknown scheduling projection"
        );
        Ok(())
    }
}

/// Retain through the caller's transaction proof. Coalesce source writes per
/// key before reconciliation; delta zero never means owner release can be skipped.
#[derive(Debug)]
#[must_use]
pub(in crate::tenancy) struct Change {
    pub key: Key,
    pub before: Option<Record>,
    pub after: Option<Record>,
}
impl Change {
    #[cfg(test)]
    pub fn delta(&self) -> i64 {
        i64::from(self.after.is_some()) - i64::from(self.before.is_some())
    }
}
fn sql(query: impl Into<String>, values: impl IntoIterator<Item = Value>) -> Statement {
    Statement::from_sql_and_values(DatabaseBackend::Sqlite, query, values)
}
fn sqlite(db: &impl ConnectionTrait) -> Result<()> {
    ensure!(
        db.get_database_backend() == DatabaseBackend::Sqlite,
        "unknown projection requires SQLite"
    );
    Ok(())
}

#[cfg(test)]
async fn empty_server_sources(db: &impl ConnectionTrait, charge: &Charge<'_>) -> Result<()> {
    // Isolated server transaction fixtures have no receipt/OAuth workload. Prove
    // that restriction in the caller's transaction; never invent an empty view
    // of retained server history or enable the SQLite projection on that backend.
    ensure!(
        matches!(
            db.get_database_backend(),
            DatabaseBackend::Postgres | DatabaseBackend::MySql
        ),
        "empty-source compatibility requires a server test database"
    );
    for table in [
        "tenancy_execution_receipts",
        "tenancy_receipt_auxiliary",
        "tenancy_oauth_slots",
    ] {
        charge(1, 128)?;
        let query = Query::select()
            .expr(Expr::val(1_i32))
            .from(Alias::new(table))
            .limit(1)
            .to_owned();
        let row = db
            .query_one(db.get_database_backend().build(&query))
            .await?;
        charge(0, 0)?;
        ensure!(row.is_none(), "server test source must be empty: {table}");
    }
    Ok(())
}

fn due(first: DateTime<Utc>) -> Result<DateTime<Utc>> {
    first
        .checked_add_signed(chrono::Duration::minutes(15))
        .context("unknown due overflow")
}

/// Recompute once per key after all source writes in the caller transaction;
/// an unchanged-final-state replay is harmless. Do not reconcile an intermediate
/// disappearance and then recreate a source: that is not a committed resolution.
/// The caller must
/// positively validate OAuth retirement before changing/removing its old work;
/// this is not an operator resolve API. Immutable-source disappearance fails.
/// No counts, generation, commit, publication or activation are performed here.
pub(in crate::tenancy) async fn reconcile(
    tx: &DatabaseTransaction,
    installation: &InstallationId,
    key: &Key,
    charge: &Charge<'_>,
) -> Result<Change> {
    sqlite(tx)?;
    let before = lookup(tx, installation, key, charge).await?;
    let source = sources::read(tx, installation, key, charge).await?;
    if let Some(old) = &before {
        ensure!(
            old.source_mask & (RECEIPT | AUXILIARY) & !source.mask == 0,
            "immutable unknown source disappeared"
        );
    }
    let after = match source.first {
        Some(first) => {
            let first = before.as_ref().map_or(first, |old| old.first.min(first));
            let value = Record {
                key: key.clone(),
                source_mask: source.mask,
                first,
                due: due(first)?,
                escalated: before.as_ref().is_some_and(|old| old.escalated),
            };
            value.validate()?;
            Some(value)
        }
        None => {
            ensure!(source.mask == 0, "unknown source time missing");
            None
        }
    };
    if before != after {
        io::write(
            tx,
            installation,
            key,
            before.is_some(),
            after.as_ref(),
            charge,
        )
        .await?;
    }
    charge(0, 0)?;
    Ok(Change {
        key: key.clone(),
        before,
        after,
    })
}

/// Exact point read of derived state; canonical coverage is established before
/// readiness and maintained by the exclusive writer's source transactions.
pub(in crate::tenancy) async fn lookup(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    key: &Key,
    charge: &Charge<'_>,
) -> Result<Option<Record>> {
    sqlite(db)?;
    io::lookup(db, installation, key, charge).await
}

/// Complete keyset page. Caller retains the final key and a stable read/exclusive
/// owner; empty page completes the scan. PAGE is a working buffer, not a quota.
pub(in crate::tenancy) async fn page(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    after: Option<&Key>,
    charge: &Charge<'_>,
) -> Result<Vec<Record>> {
    sqlite(db)?;
    io::page(db, installation, after, charge).await
}

/// Covering-index COUNT for a single tenant, with no truncation or admission
/// threshold. The actor performs its global eligibility check before this call.
/// Deadline/cancellation ownership belongs to the caller; no partial count is returned.
pub(in crate::tenancy) async fn tenant_count(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    tenant: &TenantId,
    charge: &Charge<'_>,
) -> Result<u64> {
    #[cfg(test)]
    if db.get_database_backend() != DatabaseBackend::Sqlite {
        empty_server_sources(db, charge).await?;
        return Ok(0);
    }
    sqlite(db)?;
    charge(1, 128)?;
    let row = db.query_one(sql("SELECT COUNT(*) AS count FROM tenancy_unknown_obligations WHERE installation_id=? AND tenant_id=?", [installation.as_str().into(), tenant.as_str().into()])).await?.context("unknown tenant count")?;
    let count = u64::try_from(row.try_get::<i64>("", "count")?)?;
    charge(0, 0)?;
    Ok(count)
}

/// Read-only due page; caller owns any subsequent escalation transaction.
/// Repeating this call after marking the page advances without a global scan.
pub(in crate::tenancy) async fn due_page(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    now: DateTime<Utc>,
    charge: &Charge<'_>,
) -> Result<Vec<Record>> {
    sqlite(db)?;
    io::due_page(db, installation, now, charge).await
}

/// Indexed next-due scalar for the existing actor timer; no resident row map.
pub(in crate::tenancy) async fn next_due(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    charge: &Charge<'_>,
) -> Result<Option<DateTime<Utc>>> {
    #[cfg(test)]
    if db.get_database_backend() != DatabaseBackend::Sqlite {
        empty_server_sources(db, charge).await?;
        return Ok(None);
    }
    sqlite(db)?;
    io::next_due(db, installation, charge).await
}

/// Mark only the exact inspected scheduling version in the caller transaction.
/// A replay is harmless; a different/missing obligation is a conflict, not success.
pub(in crate::tenancy) async fn mark_escalated(
    tx: &DatabaseTransaction,
    installation: &InstallationId,
    expected: &Record,
    charge: &Charge<'_>,
) -> Result<()> {
    let current = lookup(tx, installation, &expected.key, charge)
        .await?
        .context("escalation obligation missing")?;
    ensure!(
        current.source_mask == expected.source_mask
            && current.first == expected.first
            && current.due == expected.due,
        "escalation obligation changed"
    );
    if current.escalated {
        return Ok(());
    }
    let mut next = current;
    next.escalated = true;
    io::write(tx, installation, &next.key, true, Some(&next), charge).await
}

/// Complete bidirectional validation on a caller-owned stable transaction or
/// exclusive startup connection. The caller prevents intervening source writes.
/// Callback governs per-page progress/resources, never a fixed lifetime quota.
/// Failure returns no successful inventory and must leave startup unready.
pub(in crate::tenancy) async fn validate(
    tx: &impl ConnectionTrait,
    installation: &InstallationId,
    charge: &Charge<'_>,
) -> Result<Inventory> {
    #[cfg(test)]
    if tx.get_database_backend() != DatabaseBackend::Sqlite {
        empty_server_sources(tx, charge).await?;
        return Ok(Fold::new().finish());
    }
    sqlite(tx)?;
    validation::run(tx, installation, charge).await
}

/// Complete validation with one fixed working-set reservation, suitable for
/// offline/startup callers whose charge accumulates bytes. Call with a fresh
/// phase budget: the reservation lives until this future returns. The envelope
/// includes retained key/record pages, candidate/archive decode and pending-slot
/// pages together. Inner checkpoints still call charge(0, 0), never reset a
/// caller's budget or hide its cancellation/error. No total-history quota.
/// Requires the same stable transaction/exclusive connection as `validate`.
pub(in crate::tenancy) async fn validate_with_budget(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    charge: &Charge<'_>,
) -> Result<Inventory> {
    sqlite(db)?;
    charge(validation::WORKING_ROWS, validation::WORKING_BYTES)?;
    validation::run(db, installation, &|rows, bytes| {
        // Bounds changes in subordinate readers must update the documented
        // overlap envelope, not silently borrow another allowance.
        ensure!(
            rows <= validation::WORKING_ROWS && bytes <= validation::WORKING_BYTES,
            "unknown validation working envelope exceeded"
        );
        charge(0, 0)
    })
    .await
}

/// Shared ordered encoding for startup and the bounded repair-overlay merge.
/// Escalation delivery is deliberately excluded; scheduling/source state is not.
pub(in crate::tenancy) struct Fold {
    count: u64,
    hash: Sha256,
    previous: Option<Key>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tenancy) struct Inventory {
    pub unknown_count: u64,
    pub digest: [u8; 32],
}
impl Fold {
    pub fn new() -> Self {
        let mut hash = Sha256::new();
        hash.update(b"bitrouter.unknown-obligations.v1");
        Self {
            count: 0,
            hash,
            previous: None,
        }
    }
    pub fn push(&mut self, installation: &InstallationId, row: &Record) -> Result<()> {
        row.validate()?;
        ensure!(
            self.previous
                .as_ref()
                .is_none_or(|previous| previous < &row.key),
            "unknown digest key order"
        );
        let bytes = serde_json::to_vec(&(
            installation.as_str(),
            row.key.tenant.as_str(),
            row.key.execution.as_str(),
            row.source_mask,
            row.first.timestamp(),
            row.first.timestamp_subsec_nanos(),
            row.due.timestamp(),
            row.due.timestamp_subsec_nanos(),
        ))?;
        let count = self
            .count
            .checked_add(1)
            .context("unknown count overflow")?;
        self.hash.update(u64::try_from(bytes.len())?.to_be_bytes());
        self.hash.update(bytes);
        self.count = count;
        self.previous = Some(row.key.clone());
        Ok(())
    }
    pub fn finish(self) -> Inventory {
        Inventory {
            unknown_count: self.count,
            digest: self.hash.finalize().into(),
        }
    }
}
