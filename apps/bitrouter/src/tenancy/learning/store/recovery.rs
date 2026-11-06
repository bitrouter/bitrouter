//! Bootstrap-only immutable audit and repair; no live scope or dispatch is created.
mod audit;
mod closure;
mod native;
mod projection;
mod write;

use super::*;
use crate::tenancy::{
    admission::Repair,
    model::AuthorityHeader,
    origin::StartupOwner,
    receipts::{
        capacity::Inventory,
        store::{Candidate, obligations},
    },
    resources::Resources,
    transaction::{Disposition, Proof},
};
use bitrouter_ai::tenancy::identity::{TenantId, Timestamp};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

pub(in crate::tenancy) struct Plan {
    rows: Vec<records::Model>,
    configurations: BTreeMap<(TenantId, RevisionId), Arc<closure::Configuration>>,
    writes: Vec<Write>,
    sources: Vec<String>,
    unknown_before: BTreeMap<obligations::Key, Option<obligations::Record>>,
    at: Timestamp,
}
pub(in crate::tenancy) struct Access<'a> {
    pub(in crate::tenancy) database: &'a sea_orm::DatabaseConnection,
    pub(in crate::tenancy) state: &'a AuthorityHeader,
    pub(in crate::tenancy) origin: &'a StartupOwner,
    pub(in crate::tenancy) proof: &'a Arc<Proof>,
    pub(in crate::tenancy) resources: &'a Resources,
}
struct Write {
    association: namespace::Stored,
    row: records::Model,
    pending: records::Model,
    terminal: Record,
    assessment: Option<AssessmentRevision>,
    receipt: Arc<Candidate>,
    missing_receipt: bool,
}

/// This frame lives outside the caught bootstrap future, beside the real H owner.
#[derive(Default)]
pub(in crate::tenancy) struct Bootstrap {
    plan: OnceLock<Plan>,
    inventory: OnceLock<Inventory>,
    expected: OnceLock<Inventory>,
    reservations: Mutex<Vec<Repair>>,
    committed: AtomicBool,
    settled: AtomicBool,
}
impl Bootstrap {
    pub(in crate::tenancy) fn inventory(&self, value: &Inventory) -> Result<()> {
        self.inventory
            .set(value.clone())
            .map_err(|_| anyhow::anyhow!("paid receipt baseline already fixed"))
    }

    /// Reconcile each fixed source key after all source writes, while streaming
    /// the original projection past those keys. Retain only the expected scalars.
    /// The returned bounded changes belong to pre-COMMIT admission publication.
    pub(in crate::tenancy) async fn prepare_inventory(
        &self,
        tx: &DatabaseTransaction,
        installation: &bitrouter_ai::tenancy::identity::InstallationId,
        budget: &OperationBudget<'_>,
    ) -> Result<Vec<obligations::Change>> {
        let original = self
            .inventory
            .get()
            .context("paid receipt baseline absent")?;
        let (expected, changes) =
            projection::prepare(tx, installation, self.plan()?, original, budget).await?;
        self.expected
            .set(expected)
            .map_err(|_| anyhow::anyhow!("paid expected inventory already fixed"))?;
        Ok(changes)
    }

    pub(in crate::tenancy) fn verify_inventory(&self, actual: &Inventory) -> Result<()> {
        let expected = self
            .expected
            .get()
            .context("paid expected inventory absent")?;
        ensure!(
            actual.rows == expected.rows
                && actual.bytes == expected.bytes
                && actual.live_bytes == expected.live_bytes
                && actual.history_bytes == expected.history_bytes
                && actual.history_rows == expected.history_rows
                && actual.oauth_bytes == expected.oauth_bytes
                && actual.unknown_count == expected.unknown_count
                && actual.unknown_digest == expected.unknown_digest,
            "paid final receipt inventory differs from fixed repair"
        );
        Ok(())
    }
    pub(in crate::tenancy) fn install(&self, plan: Plan) -> Result<()> {
        self.plan
            .set(plan)
            .map_err(|_| anyhow::anyhow!("paid repair proposal already fixed"))
    }
    pub(in crate::tenancy) fn plan(&self) -> Result<&Plan> {
        self.plan.get().context("paid repair proposal absent")
    }
    pub(in crate::tenancy) fn reserve(
        &self,
        admission: &Arc<crate::tenancy::admission::Admission>,
        read: &crate::tenancy::admission::unknown::Read,
    ) -> Result<()> {
        let mut groups: BTreeMap<TenantId, (u64, Vec<_>)> = BTreeMap::new();
        for write in &self.plan()?.writes {
            if write.missing_receipt {
                let group = groups
                    .entry(write.receipt.receipt.tenant_id.clone())
                    .or_default();
                group.0 = group
                    .0
                    .checked_add(write.receipt.bytes())
                    .and_then(|bytes| bytes.checked_add(write.receipt.auxiliary_bytes()))
                    .context("paid receipt byte overflow")?;
                let key = projection::key(write);
                let before = self
                    .plan()?
                    .unknown_before
                    .get(&key)
                    .context("paid repair projection input absent")?;
                group
                    .1
                    .push((write.receipt.receipt.execution_id.clone(), before.is_some()));
            }
        }
        let mut reservations = self
            .reservations
            .lock()
            .map_err(|_| anyhow::anyhow!("paid reservation unavailable"))?;
        ensure!(
            reservations.is_empty() && !self.settled.load(Ordering::Acquire),
            "paid reservation repeated"
        );
        for (tenant, (bytes, entries)) in groups {
            match admission.reserve_repair(read, &tenant, bytes, entries) {
                Ok(reservation) => reservations.push(reservation),
                Err(_) => {
                    // No write transaction has started. Do not strand earlier
                    // tenants when a later independent capacity check refuses.
                    for reservation in reservations.iter_mut() {
                        reservation
                            .finish(false)
                            .map_err(|_| anyhow::anyhow!("paid reservation release failed"))?;
                    }
                    reservations.clear();
                    anyhow::bail!("paid receipt repair capacity unavailable");
                }
            }
        }
        Ok(())
    }
    pub(in crate::tenancy) fn committed(&self) {
        self.committed.store(true, Ordering::Release);
    }
    /// Called outside supervision. Unknown keeps the exact proposal/reservations
    /// alive in the existing Worker quarantine, even after its opener disappears.
    pub(in crate::tenancy) fn settle(&self, disposition: Disposition) -> Result<()> {
        if self.settled.load(Ordering::Acquire) {
            return Ok(());
        }
        let committed = self.committed.load(Ordering::Acquire);
        let mut reservations = self
            .reservations
            .lock()
            .map_err(|_| anyhow::anyhow!("paid reservation unavailable"))?;
        if !reservations.is_empty() {
            ensure!(
                committed
                    || matches!(
                        disposition,
                        Disposition::NoTransaction | Disposition::RolledBack
                    ),
                "paid receipt repair settlement is unknown"
            );
            for reservation in reservations.iter_mut() {
                reservation
                    .finish(committed)
                    .map_err(|_| anyhow::anyhow!("paid receipt repair accounting failed"))?;
            }
            reservations.clear();
        }
        self.settled.store(true, Ordering::Release);
        Ok(())
    }
}

impl Plan {
    // Read-only exact-candidate witness for the actual consuming COMMIT below.
    // Plan construction already bounds writes to 8; no test can install a plan.
    #[cfg(test)]
    pub(in crate::tenancy) fn observe_receipt_commits_for_test(
        &self,
        proof: &Proof,
    ) -> Vec<Option<usize>> {
        self.writes
            .iter()
            .filter(|write| write.missing_receipt)
            .map(|write| proof.receipt_started_for_test(&write.receipt))
            .collect()
    }

    pub(in crate::tenancy) fn has_writes(&self) -> bool {
        !self.writes.is_empty()
    }
}

pub(in crate::tenancy) async fn prepare(
    tx: &DatabaseTransaction,
    access: &Access<'_>,
    budget: &OperationBudget<'_>,
) -> Result<Plan> {
    ensure!(
        cfg!(unix) && tx.get_database_backend() == sea_orm::DatabaseBackend::Sqlite,
        "paid recovery requires the exclusive file SQLite profile"
    );
    access.origin.bootstrap_stamp(access.proof)?;
    let at = chrono::Utc::now()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        .try_into()?;
    audit::inspect(tx, access, &at, budget, None).await
}

pub(in crate::tenancy) async fn apply(
    tx: &DatabaseTransaction,
    access: &Access<'_>,
    fixed: &Plan,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    // Full recheck is charged to the SAME operation, never a fresh allowance.
    let checked =
        audit::inspect(tx, access, &fixed.at, budget, Some(&fixed.configurations)).await?;
    ensure!(
        checked.rows == fixed.rows
            && checked.sources == fixed.sources
            && checked.unknown_before == fixed.unknown_before
            && checked.writes.len() == fixed.writes.len(),
        "paid recovery source changed"
    );
    for (a, b) in checked.writes.iter().zip(&fixed.writes) {
        ensure!(
            a.row == b.row
                && a.pending == b.pending
                && a.missing_receipt == b.missing_receipt
                && a.receipt.digest == b.receipt.digest
                && io::json(&a.terminal, RECORD_BYTES)? == io::json(&b.terminal, RECORD_BYTES)?
                && io::json(&a.assessment, RECORD_BYTES)? == io::json(&b.assessment, RECORD_BYTES)?,
            "paid recovery proposal changed"
        );
    }
    drop(checked);
    write::apply(tx, fixed, budget).await
}
