use super::*;
use crate::tenancy::transaction::{Control, Disposition};
use anyhow::{Result, ensure};
use sea_orm::ConnectionTrait;
use std::sync::atomic::{AtomicBool, Ordering};

fn error(code: ErrorCode) -> anyhow::Error {
    anyhow::anyhow!("{code:?}")
}

fn fixture() -> (Arc<Admission>, Arc<AtomicBool>, Arc<Proof>, Budget) {
    let admission = Admission::new();
    let live = Arc::new(AtomicBool::new(true));
    let proof = Proof::new(live.clone());
    let budget = Budget::new(admission.clone(), proof.clone());
    (admission, live, proof, budget)
}

#[cfg(feature = "core")]
#[tokio::test]
async fn statement_refusal_requires_typed_constraint_without_integrity_or_health_failure()
-> Result<()> {
    use crate::core::eval::store::OwnedError;
    use anyhow::Context;

    let db = crate::db::connect("sqlite::memory:").await?;
    db.execute_unprepared("CREATE TABLE observation_refusal (value INTEGER NOT NULL)")
        .await?;
    db.execute_unprepared("CREATE TRIGGER refuse_observation BEFORE INSERT ON observation_refusal BEGIN SELECT RAISE(ABORT, 'fixture FULL IOERR refusal'); END")
        .await?;
    for integrity in [false, true] {
        let error: anyhow::Error = db
            .execute_unprepared("INSERT INTO observation_refusal VALUES (1)")
            .await
            .err()
            .context("trigger did not refuse actual INSERT")?
            .into();
        ensure!(sqlite_statement_refusal(&error));
        ensure!(!storage_health_error(&error));
        let error: anyhow::Error = if integrity {
            OwnedError::Integrity(error)
        } else {
            OwnedError::Storage(error)
        }
        .into();
        ensure!(sqlite_statement_refusal(&error.context("owned publication")) != integrity);
    }
    db.execute_unprepared("DROP TRIGGER refuse_observation")
        .await?;
    db.execute_unprepared("PRAGMA query_only=ON").await?;
    let error: anyhow::Error = db
        .execute_unprepared("INSERT INTO observation_refusal VALUES (1)")
        .await
        .err()
        .context("query-only connection accepted an actual INSERT")?
        .into();
    let error = anyhow::Error::from(OwnedError::Storage(error)).context("owned publication");
    ensure!(storage_health_error(&error) && !sqlite_statement_refusal(&error));
    ensure!(!sqlite_statement_refusal(&anyhow::anyhow!(
        "SQLite constraint failed (code: 1811)"
    )));
    let unrecognized: anyhow::Error = db
        .execute_unprepared("SELECT missing_column FROM observation_refusal")
        .await
        .err()
        .context("invalid column unexpectedly succeeded")?
        .into();
    ensure!(!storage_health_error(&unrecognized));
    ensure!(!sqlite_statement_refusal(&unrecognized));
    db.close().await?;
    Ok(())
}

#[test]
fn shared_estimate_checks_only_representational_overflow() -> Result<()> {
    ensure!(estimate(0) == Ok(BASE_BYTES));
    ensure!(estimate(16 * 1024) == Ok(BASE_BYTES + 8 * 16 * 1024));
    let maximum = (u64::MAX - BASE_BYTES) / AMPLIFICATION;
    ensure!(estimate(maximum).is_ok());
    ensure!(estimate(maximum + 1) == Err(ErrorCode::CapacityExceeded));
    Ok(())
}

#[cfg(unix)]
#[test]
fn exclusive_preflight_probes_fresh_available_space_on_the_owned_volume() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let owner =
        crate::tenancy::ownership::acquire_path(&directory.path().join("preflight.sqlite"), true)?;
    let required = estimate(16 * 1024).map_err(error)?;
    let calls = std::cell::Cell::new(0);
    for free in [required - 1, required] {
        let result = exclusive_write_with(&owner, 16 * 1024, |volume| {
            ensure!(Some(volume) == owner.path.parent());
            calls.set(calls.get() + 1);
            Ok(free)
        });
        ensure!(result.is_ok() == (free == required));
    }
    ensure!(calls.get() == 2);
    let shared =
        crate::tenancy::ownership::acquire_writer(&directory.path().join("shared.sqlite"), true)?;
    ensure!(
        exclusive_write_with(&shared, 0, |_| {
            calls.set(calls.get() + 1);
            Ok(u64::MAX)
        })
        .is_err()
    );
    ensure!(calls.get() == 2);
    Ok(())
}

#[tokio::test]
async fn header_creation_refuses_before_insert_but_existing_header_needs_no_probe() -> Result<()> {
    use crate::tenancy::store;
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let now = chrono::Utc::now();
    let calls = std::cell::Cell::new(0);
    ensure!(
        store::initial_header_with(&db, now, || {
            calls.set(calls.get() + 1);
            anyhow::bail!("injected insufficient free space")
        })
        .await
        .is_err()
    );
    ensure!(calls.get() == 1);
    ensure!(store::startup::read_header_in(&db, 4096).await?.is_none());
    let original = store::initial_header_with(&db, now, || Ok(())).await?;
    db.execute_unprepared("PRAGMA query_only=ON").await?;
    let loaded = store::initial_header_with(&db, now, || {
        calls.set(calls.get() + 1);
        anyhow::bail!("injected insufficient free space")
    })
    .await?;
    ensure!(calls.get() == 1 && loaded.installation == original.installation);
    db.close().await?;
    Ok(())
}

#[test]
fn ordinary_refusal_preserves_maintenance_and_unstarted_refund() -> Result<()> {
    let (admission, live, _, budget) = fixture();
    admission.receipt_probe(BASE_BYTES);
    ensure!(matches!(
        budget.reserve(0, false),
        Err(ErrorCode::CapacityExceeded)
    ));
    let charge = budget.reserve(0, true).map_err(error)?;
    ensure!(matches!(
        budget.reserve(0, true),
        Err(ErrorCode::CapacityExceeded)
    ));
    drop(charge);
    let charge = budget.reserve(0, true).map_err(error)?;
    charge.complete().map_err(error)?;
    ensure!(live.load(Ordering::Acquire) && admission.receipts_ready());
    Ok(())
}

#[test]
fn dropping_started_sql_fences_and_cannot_refund_its_reservation() -> Result<()> {
    let (admission, live, proof, budget) = fixture();
    admission.receipt_probe(BASE_BYTES);
    let mut charge = budget.reserve(0, true).map_err(error)?;
    charge.start();
    drop(charge);
    ensure!(!live.load(Ordering::Acquire) && !admission.receipts_ready());
    ensure!(proof.disposition() == Disposition::Ambiguous);
    admission.receipt_probe(u64::MAX);
    ensure!(budget.reserve(0, true).is_err());
    Ok(())
}

#[cfg(feature = "harness")]
#[test]
fn aggregate_growth_preserves_class_and_holds_prior_reservations_on_refusal() -> Result<()> {
    let (admission, live, proof, budget) = fixture();
    let mut charge = budget.reserve(10, false).map_err(error)?;
    charge.start();
    charge.grow(20).map_err(error)?;
    ensure!(charge.bytes == estimate(30).map_err(error)?);
    let held = (charge.encoded_bytes, charge.bytes);
    // Enough physical space for a small maintenance delta, below ordinary's
    // floor. Growing must not silently reclassify this existing ordinary owner.
    admission.receipt_probe(charge.bytes + 8);
    ensure!(charge.grow(1) == Err(ErrorCode::CapacityExceeded));
    ensure!(charge.grow(usize::MAX) == Err(ErrorCode::CapacityExceeded));
    ensure!((charge.encoded_bytes, charge.bytes) == held);
    ensure!(live.load(Ordering::Acquire));
    ensure!(proof.disposition() != Disposition::Ambiguous);
    ensure!(budget.reserve(0, true).is_err());
    charge.complete().map_err(error)?;
    admission.receipt_probe(BASE_BYTES + 8);
    let mut maintenance = budget.reserve(0, true).map_err(error)?;
    maintenance.start();
    maintenance.grow(1).map_err(error)?;
    ensure!(maintenance.bytes == BASE_BYTES + 8);
    ensure!(maintenance.grow(1) == Err(ErrorCode::CapacityExceeded));
    drop(maintenance);
    ensure!(proof.disposition() == Disposition::Ambiguous);
    ensure!(!live.load(Ordering::Acquire));
    Ok(())
}

#[tokio::test]
async fn only_acknowledged_transaction_outcomes_release_the_charge() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    db.execute_unprepared("CREATE TABLE write_charge_test (value INTEGER NOT NULL)")
        .await?;
    let (admission, live, proof, budget) = fixture();
    admission.receipt_probe(BASE_BYTES);
    for commit in [false, true] {
        let control = Control::new(proof.clone()).with_storage(budget.clone());
        control.reserve_write(0, true).map_err(error)?;
        let tx = control.begin(&db).await?;
        tx.execute_unprepared("INSERT INTO write_charge_test VALUES (1)")
            .await?;
        ensure!(budget.reserve(0, true).is_err());
        if commit {
            control.commit(tx).await?;
        } else {
            control.rollback(tx).await?;
        }
        let next = budget.reserve(0, true).map_err(error)?;
        next.complete().map_err(error)?;
        ensure!(live.load(Ordering::Acquire));
    }
    let row = db
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM write_charge_test",
        ))
        .await?
        .ok_or_else(|| anyhow::anyhow!("write count missing"))?;
    ensure!(row.try_get::<i64>("", "count")? == 1);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn acknowledged_rollback_does_not_clear_storage_failure() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    let (admission, live, proof, budget) = fixture();
    let control = Control::new(proof.clone()).with_storage(budget.clone());
    control.reserve_write(0, true).map_err(error)?;
    let tx = control.begin(&db).await?;
    control.storage_failed();
    control.rollback(tx).await?;
    ensure!(proof.disposition() == Disposition::RolledBack);
    ensure!(!live.load(Ordering::Acquire) && !admission.receipts_ready());
    ensure!(budget.reserve(0, true).is_err());
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn queued_execution_wakes_when_writer_charge_is_released_without_new_arrivals() -> Result<()>
{
    use bitrouter_ai::tenancy::{configuration::Limits, identity::TenantId};
    use futures::task::{ArcWake, waker_ref};
    use std::{
        future::Future,
        sync::atomic::AtomicUsize,
        task::{Context, Poll},
    };

    struct WakeCount(AtomicUsize);
    impl ArcWake for WakeCount {
        fn wake_by_ref(this: &Arc<Self>) {
            this.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let db = crate::db::connect("sqlite::memory:").await?;
    db.execute_unprepared("CREATE TABLE queued_write_test (value INTEGER NOT NULL)")
        .await?;
    let mut config = crate::config::tenancy::capacity::CapacityConfig::default();
    config.execution.instance_active = 1;
    config.execution.tenant_active = 1;
    config.receipts.maintenance_free_bytes = super::super::receipts::capacity::RESERVATION;
    let floor = 2 * config.receipts.maintenance_free_bytes;
    config.receipts.admission_free_bytes = floor;
    config.validate()?;
    let admission = Admission::configured(&config);
    let live = Arc::new(AtomicBool::new(true));
    let proof = Proof::new(live.clone());
    let budget = Budget::new(admission.clone(), proof.clone());
    let tenant: TenantId = "queued-writer".try_into()?;
    let limits = Limits {
        active_executions: 1,
        queued_work: 1,
        learning_jobs: 0,
    };

    for commit in [false, true] {
        admission.receipt_probe(u64::MAX);
        let active = admission.acquire(&tenant, &limits).await.map_err(error)?;
        let mut queued = Box::pin(admission.acquire(&tenant, &limits));
        let wakes = Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = waker_ref(&wakes);
        let mut context = Context::from_waker(&waker);
        ensure!(matches!(queued.as_mut().poll(&mut context), Poll::Pending));

        let control = Control::new(proof.clone()).with_storage(budget.clone());
        control.reserve_write(0, false).map_err(error)?;
        let tx = control.begin(&db).await?;
        tx.execute_unprepared("INSERT INTO queued_write_test VALUES (1)")
            .await?;
        admission.receipt_probe(floor - 1);
        drop(active);
        ensure!(matches!(queued.as_mut().poll(&mut context), Poll::Pending));
        // More available space still cannot reuse the actual SQL owner's bytes.
        admission.receipt_probe(floor + BASE_BYTES - 1);
        ensure!(matches!(queued.as_mut().poll(&mut context), Poll::Pending));
        let before = wakes.0.load(Ordering::SeqCst);
        if commit {
            control.commit(tx).await?;
        } else {
            control.rollback(tx).await?;
        }
        ensure!(
            wakes.0.load(Ordering::SeqCst) > before,
            "cleanup must wake without a new arrival"
        );
        let Poll::Ready(result) = queued.as_mut().poll(&mut context) else {
            anyhow::bail!("acknowledged cleanup did not admit queued execution");
        };
        drop(result.map_err(error)?);
        ensure!(live.load(Ordering::Acquire) && admission.receipts_ready());
    }
    let row = db
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM queued_write_test",
        ))
        .await?
        .ok_or_else(|| anyhow::anyhow!("write count missing"))?;
    ensure!(row.try_get::<i64>("", "count")? == 1);
    db.close().await?;
    Ok(())
}
