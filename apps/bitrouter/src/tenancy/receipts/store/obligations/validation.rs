//! Complete forward and reverse coverage; a projection-only digest cannot prove
//! that a canonical source was not omitted. No all-history collection is used.
use super::*;
use crate::tenancy::oauth::storage as oauth;

// Fixed allocation allowances from the existing bounded readers, summed even
// where phases do not overlap. This is a working-set reservation, not an object
// count/size policy or a cumulative validation limit.
// - preflight: guarded schema definitions/comparison copies + scalar queries;
// - outer page: up to PAGE Records (larger than source Keys), cursor and SHA fold;
// - candidate: bounded_row's three receipt/private/metadata copies, archive load
//   (two stored copies + four plaintext copies + 256 KiB decoder workspace),
//   auxiliary validation and inline usage validation (the latter is disjoint
//   from archive decode, but retained conservatively in this sum);
// - sources::read retains that candidate across a whole pending metadata page,
//   its one-row payload decode and one auxiliary witness decode. It drops each
//   page before fetching the next. Validation drops each candidate before the
//   next outer key and retains no history-sized vector.
const RECEIPT: usize = super::super::MAX_RECEIPT + super::super::MAX_DISPOSITION + 4096;
const USAGE: usize = bitrouter_ai::tenancy::usage_evidence::MAX_BYTES;
const AUXILIARY: usize = crate::tenancy::receipts::auxiliary::MAX_BYTES;
pub(super) const WORKING_ROWS: usize = 64 + PAGE + oauth::pending::WORKING_ROWS + 16;
pub(super) const WORKING_BYTES: usize = 1024 * 1024
    + PAGE * 1024
    + 16 * 1024
    + 3 * RECEIPT
    + 6 * (super::super::MAX_RECEIPT + USAGE)
    + 256 * 1024
    + 3 * AUXILIARY
    + 4096
    + 3 * (USAGE + 128)
    + 4096
    + oauth::pending::WORKING_BYTES
    + 2 * AUXILIARY;

pub(super) async fn preflight(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    charge: &Charge<'_>,
) -> Result<()> {
    charge(64, 1024 * 1024)?;
    crate::db::migration::validate_unknown_obligations(db).await?;
    crate::db::migration::m20240101_000040_usage_evidence::validate(db).await?;
    for table in [
        TABLE,
        "tenancy_execution_receipts",
        "tenancy_receipt_auxiliary",
        "tenancy_usage_evidence",
        "tenancy_receipt_archive",
        "tenancy_oauth_slots",
    ] {
        charge(1, 256)?;
        ensure!(db.query_one(sql(format!("SELECT 1 FROM {table} WHERE typeof(installation_id)<>'text' OR installation_id<>? LIMIT 1"), [installation.as_str().into()])).await?.is_none(), "foreign unknown inventory installation");
    }
    for table in [
        "tenancy_receipt_auxiliary",
        "tenancy_usage_evidence",
        "tenancy_receipt_archive",
    ] {
        charge(1, 256)?;
        ensure!(db.query_one(sql(format!("SELECT 1 FROM {table} e LEFT JOIN tenancy_execution_receipts r ON e.installation_id=r.installation_id AND e.tenant_id=r.tenant_id AND e.execution_id=r.execution_id WHERE r.execution_id IS NULL LIMIT 1"), [])).await?.is_none(), "orphan unknown inventory evidence");
    }
    Ok(())
}
async fn check(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    key: &Key,
    charge: &Charge<'_>,
) -> Result<()> {
    let source = sources::read(db, installation, key, charge).await?;
    let stored = lookup(db, installation, key, charge).await?;
    match (source.first, stored) {
        (None, None) => ensure!(source.mask == 0, "unknown source time missing"),
        (Some(at), Some(row)) => ensure!(
            row.source_mask == source.mask && row.first <= at,
            "unknown source projection mismatch"
        ),
        _ => anyhow::bail!("unknown source coverage mismatch"),
    }
    Ok(())
}
pub(super) async fn run(
    tx: &impl ConnectionTrait,
    installation: &InstallationId,
    charge: &Charge<'_>,
) -> Result<Inventory> {
    preflight(tx, installation, charge).await?;
    // Inspect every slot, not just those whose mutable scalar projections claim
    // pending ownership. This catches Ready/null projection corruption.
    let mut slot: Option<String> = None;
    loop {
        let rows = oauth::pending::page(tx, installation, slot.as_deref(), charge).await?;
        if rows.is_empty() {
            break;
        }
        slot = rows.last().map(|row| row.slot.clone());
    }
    let mut after = None;
    loop {
        let keys = sources::receipt_keys(tx, installation, after.as_ref(), charge).await?;
        if keys.is_empty() {
            break;
        }
        for key in keys {
            check(tx, installation, &key, charge).await?;
            after = Some(key);
        }
    }
    after = None;
    loop {
        let keys = sources::pending_keys(tx, installation, after.as_ref(), charge).await?;
        if keys.is_empty() {
            break;
        }
        for key in keys {
            check(tx, installation, &key, charge).await?;
            after = Some(key);
        }
    }
    let mut inventory = Fold::new();
    after = None;
    loop {
        let rows = page(tx, installation, after.as_ref(), charge).await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            check(tx, installation, &row.key, charge).await?;
            inventory.push(installation, &row)?;
            after = Some(row.key);
        }
    }
    charge(0, 0)?;
    Ok(inventory.finish())
}
