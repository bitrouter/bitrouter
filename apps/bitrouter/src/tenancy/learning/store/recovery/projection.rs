//! A complete ordered baseline with only the fixed repair keys retained.
use super::{Plan, Write};
use crate::{
    storage::operation_budget::{Error as BudgetError, OperationBudget},
    tenancy::receipts::{
        capacity::{Inventory, ROW_OVERHEAD},
        store::obligations::{self, Change, Fold, Key, Record},
    },
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{identity::InstallationId, operation::ErrorCode};
use sea_orm::DatabaseTransaction;
use std::collections::BTreeMap;

#[cfg(test)]
mod tests;

pub(super) fn key(write: &Write) -> Key {
    Key {
        tenant: write.receipt.receipt.tenant_id.clone(),
        execution: write.receipt.receipt.execution_id.clone(),
    }
}

struct Merge {
    baseline: Fold,
    expected: Fold,
    changes: Vec<Change>,
}
impl Merge {
    fn new(keys: usize) -> Result<Self> {
        let mut changes = Vec::new();
        changes.try_reserve_exact(keys)?;
        Ok(Self {
            baseline: Fold::new(),
            expected: Fold::new(),
            changes,
        })
    }
    fn unchanged(&mut self, installation: &InstallationId, row: &Record) -> Result<()> {
        self.baseline.push(installation, row)?;
        self.expected.push(installation, row)
    }
    fn changed(&mut self, installation: &InstallationId, change: Change) -> Result<()> {
        ensure!(
            change
                .before
                .as_ref()
                .is_none_or(|row| row.key == change.key)
                && change
                    .after
                    .as_ref()
                    .is_none_or(|row| row.key == change.key),
            "paid repair projection key changed"
        );
        if let Some(row) = &change.before {
            self.baseline.push(installation, row)?;
        }
        if let Some(row) = &change.after {
            self.expected.push(installation, row)?;
        }
        self.changes.push(change);
        Ok(())
    }
}

async fn reconcile_at(
    tx: &DatabaseTransaction,
    installation: &InstallationId,
    key: &Key,
    fixed: &Option<Record>,
    observed: Option<&Record>,
    budget: &OperationBudget<'_>,
) -> Result<Change> {
    ensure!(
        fixed.as_ref() == observed,
        "paid repair projection input changed"
    );
    let charge = |rows, bytes| budget.charge(rows, bytes).map_err(Into::into);
    let change = obligations::reconcile(tx, installation, key, &charge).await?;
    ensure!(
        change.before.as_ref() == observed,
        "paid repair projection frontier changed"
    );
    Ok(change)
}

/// All receipt/job source writes have completed in this same transaction. Only
/// reconcile keys at or behind the current read frontier: later pages still
/// contain the original baseline, including when an absent key is inserted.
async fn merge_projection(
    tx: &DatabaseTransaction,
    installation: &InstallationId,
    fixed: &BTreeMap<Key, Option<Record>>,
    budget: &OperationBudget<'_>,
) -> Result<Merge> {
    let mut pending = fixed.iter().peekable();
    let mut merge = Merge::new(fixed.len())?;
    let mut after = None;
    let check = || {
        budget.check().map_err(|error| match error {
            BudgetError::Rejected(code) => code,
            BudgetError::Capacity => ErrorCode::CapacityExceeded,
            BudgetError::Unavailable => ErrorCode::AuthorityUnavailable,
        })
    };
    loop {
        // The complete baseline is not charged to the fixed repair's cumulative
        // row budget. Release the working allowance after each keyset page.
        let page_budget = OperationBudget::with_limits(&check, 64, 128 * 1024);
        let charge = |rows, bytes| page_budget.charge(rows, bytes).map_err(Into::into);
        let page = obligations::page(tx, installation, after.as_ref(), &charge).await?;
        if page.is_empty() {
            break;
        }
        for row in page {
            while pending.peek().is_some_and(|(key, _)| *key < &row.key) {
                let (key, fixed) = pending.next().context("paid repair key absent")?;
                let change = reconcile_at(tx, installation, key, fixed, None, budget).await?;
                merge.changed(installation, change)?;
            }
            if pending.peek().is_some_and(|(key, _)| *key == &row.key) {
                let (key, fixed) = pending.next().context("paid repair key absent")?;
                let change = reconcile_at(tx, installation, key, fixed, Some(&row), budget).await?;
                merge.changed(installation, change)?;
            } else {
                merge.unchanged(installation, &row)?;
            }
            after = Some(row.key);
        }
    }
    for (key, fixed) in pending {
        let change = reconcile_at(tx, installation, key, fixed, None, budget).await?;
        merge.changed(installation, change)?;
    }
    Ok(merge)
}

pub(super) async fn prepare(
    tx: &DatabaseTransaction,
    installation: &InstallationId,
    plan: &Plan,
    original: &Inventory,
    budget: &OperationBudget<'_>,
) -> Result<(Inventory, Vec<Change>)> {
    ensure!(
        plan.unknown_before.len() == plan.writes.len(),
        "paid repair key coverage"
    );
    let merge = merge_projection(tx, installation, &plan.unknown_before, budget).await?;
    let baseline = merge.baseline.finish();
    ensure!(
        baseline.unknown_count == original.unknown_count
            && baseline.digest == original.unknown_digest,
        "paid unknown baseline differs from startup inventory"
    );
    let folded = merge.expected.finish();
    let mut expected = original.clone();
    expected.unknown_count = folded.unknown_count;
    expected.unknown_digest = folded.digest;
    for write in &plan.writes {
        if !write.missing_receipt {
            continue;
        }
        let receipt = &write.receipt;
        ensure!(
            receipt.receipt.installation_id == *installation && receipt.unknown(),
            "paid missing receipt is not a canonical unknown repair"
        );
        let bytes = receipt
            .bytes()
            .checked_add(receipt.auxiliary_bytes())
            .context("paid receipt byte overflow")?;
        add_missing_receipt(&mut expected, bytes)?;
    }
    budget.check()?;
    Ok((expected, merge.changes))
}

fn add_missing_receipt(expected: &mut Inventory, bytes: u64) -> Result<()> {
    let rows = expected
        .rows
        .checked_add(1)
        .context("paid receipt row overflow")?;
    let total = expected
        .bytes
        .checked_add(bytes)
        .context("paid receipt byte overflow")?;
    let live = expected
        .live_bytes
        .checked_add(bytes)
        .and_then(|value| value.checked_add(ROW_OVERHEAD))
        .context("paid live receipt byte overflow")?;
    expected.rows = rows;
    expected.bytes = total;
    expected.live_bytes = live;
    Ok(())
}
