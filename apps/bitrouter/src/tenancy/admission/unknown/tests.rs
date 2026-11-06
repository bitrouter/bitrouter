use super::*;
use crate::tenancy::{
    receipts::{capacity::Inventory, store::obligations::Record},
    transaction::Control,
};
use anyhow::{Result, ensure};
use sea_orm::{Database, DatabaseConnection};

fn failure(error: ErrorCode) -> anyhow::Error {
    anyhow::anyhow!("{error:?}")
}
fn key(tenant: &str, execution: &str) -> Result<Key> {
    Ok(Key {
        tenant: tenant.try_into()?,
        execution: execution.try_into()?,
    })
}
fn change(key: &Key, before: bool, after: bool) -> Change {
    let now = chrono::Utc::now();
    let record = Record {
        key: key.clone(),
        source_mask: 4,
        first: now,
        due: now + chrono::Duration::minutes(15),
        escalated: false,
    };
    Change {
        key: key.clone(),
        before: before.then(|| record.clone()),
        after: after.then_some(record),
    }
}
async fn acquire(admission: &Arc<Admission>, key: &Key) -> Result<Lease> {
    let lease = admission
        .acquire(
            &key.tenant,
            &Limits {
                active_executions: 4,
                queued_work: 0,
                learning_jobs: 0,
            },
        )
        .await
        .map_err(failure)?;
    lease.bind_execution(&key.execution).map_err(failure)?;
    Ok(lease)
}
async fn apply(
    db: &DatabaseConnection,
    admission: &Arc<Admission>,
    key: &Key,
    before: bool,
    after: bool,
    terminal: Option<u64>,
) -> Result<()> {
    let read = admission.unknown_read(true).map_err(failure)?;
    let proof = Proof::new(Arc::new(AtomicBool::new(true)));
    let control = Control::new(proof.clone());
    let tx = control.begin(db).await?;
    let terminals = terminal
        .map(|bytes| Terminal {
            key: key.clone(),
            bytes,
        })
        .into_iter()
        .collect();
    let publication = admission
        .prepare_unknown(&read, vec![change(key, before, after)], terminals, proof)
        .map_err(failure)?;
    control.commit(tx).await?;
    publication.publish().map_err(failure)?;
    Ok(())
}
fn sizes(admission: &Admission) -> (usize, usize, usize) {
    let state = admission.lock();
    (state.active, state.tenants.len(), state.executions.len())
}
fn totals(admission: &Admission) -> Totals {
    admission.lock().receipts.totals
}

#[tokio::test]
async fn oauth_coverage_and_zero_delta_terminal_release_once_without_freeing_bytes_early()
-> Result<()> {
    let db = Database::connect("sqlite::memory:").await?;
    let admission = Admission::new();
    let key = key("tenant", "execution")?;
    let lease = acquire(&admission, &key).await?;
    for (before, after, expected_unknown, expected_uncovered) in [
        (false, true, 1, 0),
        (true, true, 1, 0),
        (true, false, 0, 1),
        (false, true, 1, 0),
    ] {
        apply(&db, &admission, &key, before, after, None).await?;
        let value = totals(&admission);
        ensure!(value.unknown == expected_unknown && value.uncovered == expected_uncovered);
        ensure!(value.live == 1 && value.reserved == RESERVATION);
    }
    apply(&db, &admission, &key, true, true, Some(13)).await?;
    let finished = totals(&admission);
    ensure!(
        finished.unknown == 1
            && finished.uncovered == 0
            && finished.live == 0
            && finished.reserved == 0
    );
    ensure!(finished.bytes == 13 + ROW_OVERHEAD);
    lease.confirm_receipt(13, None).map_err(failure)?;
    apply(&db, &admission, &key, true, true, Some(0)).await?;
    lease.confirm_receipt(0, None).map_err(failure)?;
    ensure!(totals(&admission) == finished);
    // Pending OAuth may settle after terminal publication while body cleanup still owns the lease.
    apply(&db, &admission, &key, true, false, None).await?;
    ensure!(totals(&admission).unknown == 0 && totals(&admission).uncovered == 0);
    drop(lease);
    ensure!((sizes(&admission).1 == 0) && (sizes(&admission).2 == 0));
    Ok(())
}

#[tokio::test]
async fn cold_count_is_owner_generation_bound_and_idle_durable_tenants_are_evicted() -> Result<()> {
    let db = Database::connect("sqlite::memory:").await?;
    let admission = Admission::new();
    admission
        .initialize_receipts(
            Inventory {
                unknown_count: 3,
                ..Default::default()
            },
            u64::MAX,
        )
        .map_err(failure)?;
    let tenant: TenantId = "cold".try_into()?;
    ensure!(
        admission
            .pin_unknown(&tenant, false)
            .map_err(failure)?
            .is_none()
    );
    let read = admission.unknown_read(false).map_err(failure)?;
    let pin = admission.seed_unknown(&read, &tenant, 2).map_err(failure)?;
    let other = Admission::new();
    ensure!(other.seed_unknown(&read, &tenant, 0).is_err());
    apply(&db, &admission, &key("cold", "new")?, false, true, None).await?;
    ensure!(
        admission
            .lock()
            .tenants
            .get(&tenant)
            .and_then(|c| c.unknown)
            == Some(3)
    );
    ensure!(admission.seed_unknown(&read, &tenant, 2).is_err());
    drop(pin);
    ensure!((sizes(&admission).1 == 0) && totals(&admission).unknown == 4);
    let hot = admission.pin_unknown(&tenant, false).map_err(failure)?;
    ensure!(hot.is_none());
    Ok(())
}

#[tokio::test]
async fn pending_commit_blocks_grants_and_pins_but_preserves_concurrent_cleanup() -> Result<()> {
    let db = Database::connect("sqlite::memory:").await?;
    let admission = Admission::new();
    let affected = key("a", "one")?;
    let unrelated = key("b", "two")?;
    let lease = acquire(&admission, &affected).await?;
    let other = acquire(&admission, &unrelated).await?;
    let read = admission.unknown_read(true).map_err(failure)?;
    let proof = Proof::new(Arc::new(AtomicBool::new(true)));
    let control = Control::new(proof.clone());
    let tx = control.begin(&db).await?;
    let publication = admission
        .prepare_unknown(
            &read,
            vec![change(&affected, false, true)],
            vec![Terminal {
                key: affected.clone(),
                bytes: 1,
            }],
            proof,
        )
        .map_err(failure)?;
    ensure!(admission.pin_unknown(&affected.tenant, false).is_err());
    ensure!(admission.seed_unknown(&read, &affected.tenant, 0).is_err());
    ensure!(admission.reserve_physical_write(0, true).is_err());
    ensure!(acquire(&admission, &key("c", "three")?).await.is_err());
    drop(other);
    ensure!(sizes(&admission).0 == 1 && totals(&admission).reserved == RESERVATION);
    drop(lease);
    ensure!(sizes(&admission).0 == 1 && totals(&admission).reserved == RESERVATION);
    control.commit(tx).await?;
    publication.publish().map_err(failure)?;
    ensure!(
        sizes(&admission).0 == 0
            && totals(&admission).reserved == 0
            && totals(&admission).unknown == 1
    );
    ensure!((sizes(&admission).1 == 0) && (sizes(&admission).2 == 0));
    Ok(())
}

#[tokio::test]
async fn known_rollback_keeps_durable_count_and_closed_admission_allows_cleanup() -> Result<()> {
    let db = Database::connect("sqlite::memory:").await?;
    let admission = Admission::new();
    let key = key("tenant", "execution")?;
    let lease = acquire(&admission, &key).await?;
    admission.close();
    ensure!(admission.unknown_read(false).is_err());
    let read = admission.unknown_read(true).map_err(failure)?;
    let proof = Proof::new(Arc::new(AtomicBool::new(true)));
    let control = Control::new(proof.clone());
    let tx = control.begin(&db).await?;
    let publication = admission
        .prepare_unknown(
            &read,
            vec![change(&key, false, true)],
            vec![Terminal {
                key: key.clone(),
                bytes: 1,
            }],
            proof,
        )
        .map_err(failure)?;
    control.rollback(tx).await?;
    publication.rollback().map_err(failure)?;
    ensure!(
        totals(&admission).unknown == 0
            && totals(&admission).uncovered == 1
            && totals(&admission).reserved == RESERVATION
    );
    apply(&db, &admission, &key, false, false, Some(7)).await?;
    ensure!(totals(&admission).unknown == 0 && totals(&admission).reserved == 0);
    drop(lease);
    admission.wait_drained().await;
    Ok(())
}

#[tokio::test]
async fn unconsumed_publication_retains_custody_before_and_after_commit() -> Result<()> {
    for commit in [false, true] {
        let db = Database::connect("sqlite::memory:").await?;
        let admission = Admission::new();
        let key = key("tenant", "execution")?;
        let lease = acquire(&admission, &key).await?;
        let read = admission.unknown_read(true).map_err(failure)?;
        let proof = Proof::new(Arc::new(AtomicBool::new(true)));
        let control = Control::new(proof.clone());
        let tx = control.begin(&db).await?;
        let publication = admission
            .prepare_unknown(
                &read,
                vec![change(&key, false, true)],
                vec![Terminal {
                    key: key.clone(),
                    bytes: 1,
                }],
                proof.clone(),
            )
            .map_err(failure)?;
        if commit {
            control.commit(tx).await?;
        } else {
            control.rollback(tx).await?;
        }
        // Even a known SQL outcome is not evidence that publication was consumed.
        drop(publication);
        drop(lease);
        ensure!(proof.disposition() == Disposition::Ambiguous && !admission.receipts_ready());
        ensure!(totals(&admission).reserved == RESERVATION && sizes(&admission).0 == 1);
        ensure!(admission.unknown_read(true).is_err());
        ensure!(admission.reserve_physical_write(1, true).is_err());
    }
    Ok(())
}

#[test]
fn lowered_restart_budgets_refuse_before_cold_count_without_retaining_catalog() -> Result<()> {
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
    let tenant: TenantId = "retained".try_into()?;
    ensure!(matches!(
        admission.unknown_read(false),
        Err(ErrorCode::CapacityExceeded)
    ));
    ensure!(matches!(
        admission.pin_unknown(&tenant, false),
        Err(ErrorCode::CapacityExceeded)
    ));
    ensure!((sizes(&admission).1 == 0));
    let read = admission.unknown_read(true).map_err(failure)?;
    let pin = admission
        .seed_unknown(&read, &tenant, 100_001)
        .map_err(failure)?;
    drop(pin);
    ensure!((sizes(&admission).1 == 0) && totals(&admission).unknown == 100_001);
    Ok(())
}
