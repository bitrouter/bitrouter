//! Bootstrap-only immutable audit and repair; no live scope or dispatch is created.
mod audit;
mod closure;
mod native;
mod write;

use super::*;
use crate::tenancy::{
    admission::Repair,
    model::Snapshot,
    origin::StartupOwner,
    receipts::store::Candidate,
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
    at: Timestamp,
}
pub(in crate::tenancy) struct Access<'a> {
    pub(in crate::tenancy) database: &'a sea_orm::DatabaseConnection,
    pub(in crate::tenancy) state: &'a Snapshot,
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
    inventory: OnceLock<crate::tenancy::receipts::capacity::Inventory>,
    reservations: Mutex<Vec<Repair>>,
    committed: AtomicBool,
    settled: AtomicBool,
}
impl Bootstrap {
    pub(in crate::tenancy) fn inventory(
        &self,
        value: &crate::tenancy::receipts::capacity::Inventory,
    ) -> Result<()> {
        ensure!(
            value.unknown.len() <= 1000,
            "paid startup receipt inventory bound"
        );
        self.inventory
            .set(crate::tenancy::receipts::capacity::Inventory {
                rows: value.rows,
                bytes: value.bytes,
                unknown: value.unknown.clone(),
            })
            .map_err(|_| anyhow::anyhow!("paid receipt baseline already fixed"))
    }
    pub(in crate::tenancy) fn verify_inventory(
        &self,
        actual: &crate::tenancy::receipts::capacity::Inventory,
    ) -> Result<()> {
        let original = self
            .inventory
            .get()
            .context("paid receipt baseline absent")?;
        let mut bytes = original.bytes;
        let mut rows = original.rows;
        let mut unknown: BTreeMap<_, _> = original
            .unknown
            .iter()
            .map(|(tenant, id, at)| ((tenant.clone(), id.clone()), *at))
            .collect();
        ensure!(
            unknown.len() == original.unknown.len(),
            "paid duplicate receipt baseline"
        );
        for write in &self.plan()?.writes {
            if write.missing_receipt {
                rows = rows.checked_add(1).context("paid receipt row overflow")?;
                bytes = bytes
                    .checked_add(write.receipt.bytes())
                    .context("paid receipt byte overflow")?;
                let receipt = &write.receipt.receipt;
                let at = chrono::DateTime::parse_from_rfc3339(receipt.recorded_at.as_str())?
                    .with_timezone(&chrono::Utc);
                ensure!(
                    unknown
                        .insert(
                            (receipt.tenant_id.clone(), receipt.execution_id.clone()),
                            at
                        )
                        .is_none(),
                    "paid receipt identity collision"
                );
            }
        }
        let observed: BTreeMap<_, _> = actual
            .unknown
            .iter()
            .map(|(tenant, id, at)| ((tenant.clone(), id.clone()), *at))
            .collect();
        ensure!(
            actual.rows == rows
                && actual.bytes == bytes
                && actual.unknown.len() == observed.len()
                && observed == unknown,
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
                    .context("paid receipt byte overflow")?;
                group.1.push((
                    write.receipt.receipt.execution_id.clone(),
                    chrono::DateTime::parse_from_rfc3339(
                        write.receipt.receipt.recorded_at.as_str(),
                    )?
                    .with_timezone(&chrono::Utc),
                ));
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
            match admission.reserve_repair(&tenant, bytes, entries) {
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
