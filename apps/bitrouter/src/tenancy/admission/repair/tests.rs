use super::*;
use crate::tenancy::{
    receipts::capacity::{Inventory, ROW_OVERHEAD},
    transaction::{Control, Proof},
};
use anyhow::{Result, ensure};
use sea_orm::Database;
fn snapshot(admission: &Admission) -> (crate::tenancy::receipts::capacity::Totals, usize, usize) {
    let state = admission.lock();
    (state.receipts.totals, state.active, state.tenants.len())
}
fn failure(error: ErrorCode) -> anyhow::Error {
    anyhow::anyhow!("{error:?}")
}

#[tokio::test]
async fn repair_above_policy_uses_one_owner_and_only_projection_publication_counts() -> Result<()> {
    let admission = Admission::new();
    admission
        .initialize_receipts(
            Inventory {
                unknown_count: 100_001,
                ..Default::default()
            },
            u64::MAX,
        )
        .map_err(failure)?;
    let tenant: TenantId = "repair".try_into()?;
    let execution: ExecutionId = "execution".try_into()?;
    let read = admission.unknown_read(true).map_err(failure)?;
    let mut repair = admission
        .reserve_repair(&read, &tenant, 17, vec![(execution.clone(), true)])
        .map_err(failure)?;
    ensure!(
        admission
            .reserve_repair(&read, &tenant, 17, vec![(execution.clone(), true)])
            .is_err()
    );
    ensure!(snapshot(&admission).1 == 0 && snapshot(&admission).0.uncovered == 0);
    let db = Database::connect("sqlite::memory:").await?;
    let proof = Proof::new(Arc::new(AtomicBool::new(true)));
    let control = Control::new(proof.clone());
    let tx = control.begin(&db).await?;
    let key = Key {
        tenant: tenant.clone(),
        execution,
    };
    let now = chrono::Utc::now();
    let row = crate::tenancy::receipts::store::obligations::Record {
        key: key.clone(),
        source_mask: 4,
        first: now,
        due: now + chrono::Duration::minutes(15),
        escalated: false,
    };
    let change = crate::tenancy::receipts::store::obligations::Change {
        key,
        before: Some(row.clone()),
        after: Some(row),
    };
    let publication = admission
        .prepare_unknown(&read, vec![change], vec![], proof)
        .map_err(failure)?;
    ensure!(repair.finish(true).is_err());
    control.commit(tx).await?;
    publication.publish().map_err(failure)?;
    repair.finish(true).map_err(failure)?;
    ensure!(repair.finish(true).is_err());
    let state = admission.lock();
    ensure!(
        state.receipts.totals.unknown == 100_001
            && state.receipts.totals.live == 0
            && state.receipts.totals.reserved == 0
    );
    ensure!(
        state.receipts.totals.bytes == 17 + ROW_OVERHEAD
            && state.tenants.is_empty()
            && state.executions.is_empty()
    );
    Ok(())
}

#[test]
fn known_repair_rollback_releases_but_abandonment_retains_and_fences() -> Result<()> {
    let admission = Admission::new();
    let tenant: TenantId = "repair".try_into()?;
    let read = admission.unknown_read(true).map_err(failure)?;
    let mut repair = admission
        .reserve_repair(&read, &tenant, 1, vec![("one".try_into()?, false)])
        .map_err(failure)?;
    repair.finish(false).map_err(failure)?;
    ensure!(snapshot(&admission).0.live == 0 && (snapshot(&admission).2 == 0));
    let repair = admission
        .reserve_repair(&read, &tenant, 1, vec![("two".try_into()?, false)])
        .map_err(failure)?;
    drop(repair);
    ensure!(!admission.receipts_ready() && snapshot(&admission).0.reserved == ROW_OVERHEAD + 1);
    Ok(())
}
