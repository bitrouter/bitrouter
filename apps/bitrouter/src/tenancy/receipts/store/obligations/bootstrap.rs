//! One-time canonical backfill, called only by migration043 before its ledger
//! entry commits. Normal startup calls validate, never this migration entry.
use super::*;
use crate::tenancy::oauth::storage as oauth;
use std::collections::VecDeque;

pub(super) async fn run(tx: &DatabaseTransaction) -> Result<()> {
    sqlite(tx)?;
    crate::db::migration::validate_unknown_obligations(tx).await?;
    crate::db::migration::m20240101_000040_usage_evidence::validate(tx).await?;
    ensure!(
        tx.query_one(sql("SELECT 1 FROM tenancy_unknown_obligations LIMIT 1", []))
            .await?
            .is_none(),
        "migration obligation table is not new"
    );
    let headers=tx.query_all(sql("SELECT singleton,CASE WHEN typeof(installation_id)='text' AND length(CAST(installation_id AS BLOB)) BETWEEN 1 AND 128 THEN installation_id ELSE NULL END AS installation_id FROM tenancy_installation LIMIT 2", [])).await?;
    if headers.is_empty() {
        // Fresh/disabled installations have no source data to assign. Never
        // infer an installation owner for retained evidence lacking its header.
        for table in [
            "tenancy_execution_receipts",
            "tenancy_receipt_auxiliary",
            "tenancy_usage_evidence",
            "tenancy_receipt_archive",
            "tenancy_oauth_slots",
        ] {
            ensure!(
                tx.query_one(sql(format!("SELECT 1 FROM {table} LIMIT 1"), []))
                    .await?
                    .is_none(),
                "unowned obligation source during migration"
            );
        }
        return Ok(());
    }
    let header = headers.first().context("migration installation header")?;
    ensure!(
        headers.len() == 1 && header.try_get::<i64>("", "singleton")? == 1,
        "migration installation header shape"
    );
    let installation: InstallationId = header
        .try_get::<String>("", "installation_id")?
        .try_into()?;
    // The exclusive migration/transaction owner supplies operation lifetime and
    // physical custody. These routines bound individual buffers and decode work,
    // without imposing an ordinary management timeout or cumulative row quota.
    let charge = |_, _| Ok(());
    validation::preflight(tx, &installation, &charge).await?;
    let mut slot = None;
    loop {
        let rows = oauth::pending::page(tx, &installation, slot.as_deref(), &charge).await?;
        if rows.is_empty() {
            break;
        }
        slot = rows.last().map(|row| row.slot.clone());
    }
    // Merge two sorted key streams. A receipt and any number of pending slots
    // for that execution are reconciled once, without a historical-key set.
    let mut receipts = VecDeque::new();
    let mut pending = VecDeque::new();
    let mut receipt_after = None;
    let mut pending_after = None;
    let mut receipts_done = false;
    let mut pending_done = false;
    loop {
        if receipts.is_empty() && !receipts_done {
            let keys =
                sources::receipt_keys(tx, &installation, receipt_after.as_ref(), &charge).await?;
            receipts_done = keys.is_empty();
            if let Some(key) = keys.last() {
                receipt_after = Some(key.clone());
            }
            receipts.extend(keys);
        }
        if pending.is_empty() && !pending_done {
            let keys =
                sources::pending_keys(tx, &installation, pending_after.as_ref(), &charge).await?;
            pending_done = keys.is_empty();
            if let Some(key) = keys.last() {
                pending_after = Some(key.clone());
            }
            pending.extend(keys);
        }
        let key = match (receipts.front(), pending.front()) {
            (Some(a), Some(b)) if a == b => {
                let _duplicate = pending.pop_front().context("migration pending duplicate")?;
                receipts.pop_front()
            }
            (Some(a), Some(b)) if a < b => receipts.pop_front(),
            (Some(_), Some(_)) | (None, Some(_)) => pending.pop_front(),
            (Some(_), None) => receipts.pop_front(),
            (None, None) => break,
        }
        .context("migration source merge")?;
        let change = reconcile(tx, &installation, &key, &charge).await?;
        ensure!(change.before.is_none(), "duplicate migration source key");
    }
    // Includes reverse coverage and ordered count/digest. A late bad source or
    // orphan rolls back all earlier row/DDL work through the caller transaction.
    validate(tx, &installation, &charge).await?;
    Ok(())
}
