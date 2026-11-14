use super::*;
use crate::tenancy::{
    Service,
    actor::Job,
    transaction::{Disposition, Phase},
};
use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseConnection};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::Instant;

const WAIT: Duration = Duration::from_secs(10);
const ERROR_DEADLINE: Duration = Duration::from_millis(500);
// A test free-space window fitting one escalation estimate, not a product limit.
const FREE: u64 = 2 * 1024 * 1024;

async fn open() -> Result<(DatabaseConnection, Service)> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let service = Service::open(db.clone(), None).await?;
    Ok((db, service))
}

fn enqueue(
    service: &Service,
    deadline: Option<Instant>,
) -> Result<oneshot::Receiver<Result<usize, Failure>>> {
    let (reply, receive) = oneshot::channel();
    service
        .commands
        .try_send(Job::Escalate {
            now: chrono::Utc::now(),
            deadline,
            reply,
        })
        .map_err(|_| anyhow::anyhow!("escalation queue unavailable"))?;
    Ok(receive)
}

fn code(error: ErrorCode) -> anyhow::Error {
    anyhow::anyhow!("{error:?}")
}

#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
fn physical(service: &Service) -> Result<u64> {
    Ok(service
        .admission
        .observed_capacity()
        .map_err(|error| anyhow::anyhow!("capacity observation: {error:?}"))?
        .physical_write_reserved)
}

#[tokio::test]
async fn read_expired_escalation_waits_for_rollback_without_fencing_and_refunds_reservation()
-> Result<()> {
    let (db, service) = open().await?;
    let proof = &service.cleanup;
    let before = proof.sql_counts_for_test();
    service.admission.receipt_probe(FREE);
    let active = proof.hold(Phase::Active);
    let mut receive = enqueue(&service, None)?;
    tokio::time::timeout(WAIT, active.entered.notified()).await?;
    ensure!(proof.sql_counts_for_test() == (before.0 + 1, before.1));
    ensure!(matches!(
        service.admission.reserve_physical_write(FREE, true),
        Err(ErrorCode::CapacityExceeded)
    ));
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    ensure!(physical(&service)? > 0);
    // Observe the actual five-second watchdog, then let due_page see ReadExpired.
    tokio::time::timeout(WAIT, proof.read_stopped.notified()).await?;
    ensure!(service.admission.receipts_ready() && service.current().is_ok());
    let rollback = proof.hold(Phase::RollingBack);
    active.release.notify_one();
    tokio::time::timeout(WAIT, rollback.entered.notified()).await?;
    ensure!(matches!(
        receive.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    ensure!(proof.disposition() == Disposition::Pending);
    ensure!(proof.sql_counts_for_test() == (before.0 + 1, before.1));
    ensure!(service.admission.receipts_ready());
    ensure!(matches!(
        service.admission.reserve_physical_write(FREE, true),
        Err(ErrorCode::CapacityExceeded)
    ));
    proof.release_gate();
    let error = tokio::time::timeout(WAIT, receive)
        .await??
        .err()
        .context("expired escalation unexpectedly succeeded")?;
    ensure!(error.0.code == ErrorCode::AuthorityUnavailable);
    ensure!(proof.disposition() == Disposition::RolledBack);
    ensure!(proof.sql_counts_for_test() == (before.0 + 1, before.1 + 1));
    ensure!(service.admission.receipts_ready() && service.current().is_ok());
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    ensure!(physical(&service)? == 0);
    // Every reserved byte must be available again, not just enough for one job.
    service
        .admission
        .reserve_physical_write(FREE, true)
        .map_err(code)?;
    service
        .admission
        .finish_physical_write(FREE)
        .map_err(code)?;
    ensure!(
        service
            .escalate_obligations_for_test(chrono::Utc::now())
            .await
            .map_err(|error| code(error.0.code))?
            == 0
    );
    service.shutdown().await?;
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn escalation_sql_error_is_not_replaced_by_read_expired_after_rollback() -> Result<()> {
    let (db, service) = open().await?;
    // The real due-page SELECT must fail after successful startup, before stop.
    db.execute_unprepared("DROP TABLE tenancy_unknown_obligations")
        .await?;
    let proof = &service.cleanup;
    let before = proof.sql_counts_for_test();
    let rollback = proof.hold(Phase::RollingBack);
    // Rollback starts cleanup before entering its gate. Expire this job first.
    ensure!(ERROR_DEADLINE < crate::tenancy::transaction::CLEANUP_TIMEOUT);
    let receive = enqueue(&service, Some(Instant::now() + ERROR_DEADLINE))?;
    tokio::time::timeout(WAIT, rollback.entered.notified()).await?;
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    ensure!(physical(&service)? > 0);
    tokio::time::timeout(WAIT, proof.read_stopped.notified()).await?;
    ensure!(service.admission.receipts_ready() && service.current().is_ok());
    proof.release_gate();
    ensure!(tokio::time::timeout(WAIT, receive).await??.is_err());
    ensure!(proof.disposition() == Disposition::RolledBack);
    ensure!(proof.sql_counts_for_test() == (before.0 + 1, before.1 + 1));
    ensure!(!service.admission.receipts_ready() && service.current().is_err());
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    ensure!(physical(&service)? == 0);
    service.shutdown().await?;
    db.close().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn worker_post_intent_stop_rolls_back_and_keeps_storage_authority_and_budget_usable()
-> Result<()> {
    use sea_orm::sqlx::{
        Connection as _, Executor as _, SqliteConnection, sqlite::SqliteConnectOptions,
    };
    let home = tempfile::tempdir()?;
    let path = home.path().join("worker-stop.sqlite");
    let owner = crate::tenancy::ownership::acquire_path(&path, true)?;
    let db = crate::db::connect_tenancy(&format!("sqlite://{}?mode=rwc", path.display()), &owner)
        .await?;
    crate::db::run_migrations(&db).await?;
    let service = Service::open(db.clone(), Some(owner.clone())).await?;
    service.admission.receipt_probe(FREE);
    let before = service.current().map_err(|e| code(e.0.code))?;
    let proof = &service.cleanup;
    let sql = proof.sql_counts_for_test();
    let intent = proof.write_intent_counts_for_test();
    let mut writer = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&owner.path)
            .create_if_missing(false),
    )
    .await?;
    writer.execute("BEGIN IMMEDIATE").await?;
    let mut rollback_attempted = false;
    let checked: Result<()> = async {
        let mut receive = enqueue(&service, Some(Instant::now() + ERROR_DEADLINE))?;
        tokio::time::timeout(WAIT, proof.wait_write_intent_entered_for_test(intent.0)).await??;
        ensure!(proof.write_intent_counts_for_test().1 == intent.1);
        tokio::time::timeout(WAIT, proof.read_stopped.notified()).await?;
        let rollback = proof.hold(Phase::RollingBack);
        rollback_attempted = true;
        writer.execute("ROLLBACK").await?;
        tokio::time::timeout(WAIT, rollback.entered.notified()).await?;
        ensure!(proof.disposition() == Disposition::Pending);
        ensure!(proof.write_intent_counts_for_test() == (intent.0 + 1, intent.1 + 1));
        ensure!(service.admission.receipts_ready() && service.current().is_ok());
        ensure!(matches!(
            receive.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        proof.release_gate();
        let error = tokio::time::timeout(WAIT, receive)
            .await??
            .err()
            .context("stopped worker unexpectedly succeeded")?;
        ensure!(error.0.code == ErrorCode::AuthorityUnavailable);
        ensure!(proof.disposition() == Disposition::RolledBack);
        ensure!(proof.sql_counts_for_test() == (sql.0 + 1, sql.1 + 1));
        ensure!(service.admission.receipts_ready() && service.current().is_ok());
        let after = service.current().map_err(|e| code(e.0.code))?;
        ensure!(after.revision == before.revision && after.operation_ids == before.operation_ids);
        #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
        ensure!(physical(&service)? == 0);
        service
            .admission
            .reserve_physical_write(FREE, true)
            .map_err(code)?;
        service
            .admission
            .finish_physical_write(FREE)
            .map_err(code)?;
        ensure!(
            service
                .escalate_obligations_for_test(chrono::Utc::now())
                .await
                .map_err(|e| code(e.0.code))?
                == 0
        );
        Ok(())
    }
    .await;
    proof.release_gate();
    let rollback = if rollback_attempted {
        Ok(())
    } else {
        writer.execute("ROLLBACK").await.map(|_| ())
    };
    let writer_closed = writer.close().await;
    let stopped = tokio::time::timeout(WAIT, service.shutdown()).await;
    let closed = db.close().await;
    checked?;
    rollback?;
    writer_closed?;
    stopped??;
    closed?;
    drop(owner);
    Ok(())
}

#[tokio::test]
async fn worker_intent_stop_never_reopens_prior_authority_or_storage_fence() -> Result<()> {
    for storage_only in [false, true] {
        let (db, service) = open().await?;
        service.admission.receipt_probe(FREE);
        let proof = &service.cleanup;
        let before = proof.sql_counts_for_test();
        let active = proof.hold(Phase::Active);
        let checked: Result<()> = async {
            let receive = enqueue(&service, Some(Instant::now() + ERROR_DEADLINE))?;
            tokio::time::timeout(WAIT, active.entered.notified()).await?;
            if storage_only {
                service.admission.fence_storage();
            } else {
                proof.fenced();
            }
            tokio::time::timeout(WAIT, proof.read_stopped.notified()).await?;
            let rollback = proof.hold(Phase::RollingBack);
            active.release.notify_one();
            tokio::time::timeout(WAIT, rollback.entered.notified()).await?;
            proof.release_gate();
            ensure!(tokio::time::timeout(WAIT, receive).await??.is_err());
            ensure!(proof.disposition() == Disposition::RolledBack);
            ensure!(proof.sql_counts_for_test() == (before.0 + 1, before.1 + 1));
            ensure!(!service.admission.receipts_ready() && service.current().is_err());
            ensure!(!service.live.load(std::sync::atomic::Ordering::Acquire));
            Ok(())
        }
        .await;
        active.release.notify_one();
        proof.release_gate();
        let stopped = tokio::time::timeout(WAIT, service.shutdown()).await;
        let closed = db.close().await;
        checked?;
        stopped??;
        closed?;
    }
    Ok(())
}

#[tokio::test]
async fn worker_real_intent_sql_error_is_not_a_clean_stop_even_after_rollback_ack() -> Result<()> {
    let (db, service) = open().await?;
    service.admission.receipt_probe(FREE);
    db.execute_unprepared("PRAGMA query_only=ON").await?;
    let proof = &service.cleanup;
    let before = proof.sql_counts_for_test();
    let intent = proof.write_intent_counts_for_test();
    let rollback = proof.hold(Phase::RollingBack);
    let checked: Result<()> = async {
        let receive = enqueue(&service, Some(Instant::now() + ERROR_DEADLINE))?;
        tokio::time::timeout(WAIT, rollback.entered.notified()).await?;
        ensure!(proof.write_intent_counts_for_test() == (intent.0 + 1, intent.1));
        ensure!(!service.admission.receipts_ready());
        tokio::time::timeout(WAIT, proof.read_stopped.notified()).await?;
        proof.release_gate();
        ensure!(tokio::time::timeout(WAIT, receive).await??.is_err());
        ensure!(proof.disposition() == Disposition::RolledBack);
        ensure!(proof.sql_counts_for_test() == (before.0 + 1, before.1 + 1));
        ensure!(!service.admission.receipts_ready() && service.current().is_err());
        Ok(())
    }
    .await;
    proof.release_gate();
    let restored = db.execute_unprepared("PRAGMA query_only=OFF").await;
    let stopped = tokio::time::timeout(WAIT, service.shutdown()).await;
    let closed = db.close().await;
    checked?;
    restored?;
    stopped??;
    closed?;
    Ok(())
}

#[tokio::test]
async fn worker_stopped_intent_unconfirmed_rollback_stays_ambiguous_and_fenced() -> Result<()> {
    let (db, service) = open().await?;
    service.admission.receipt_probe(FREE);
    let proof = &service.cleanup;
    let before = proof.sql_counts_for_test();
    let active = proof.hold(Phase::Active);
    let checked: Result<()> = async {
        let receive = enqueue(&service, Some(Instant::now() + ERROR_DEADLINE))?;
        tokio::time::timeout(WAIT, active.entered.notified()).await?;
        tokio::time::timeout(WAIT, proof.read_stopped.notified()).await?;
        let rollback = proof.hold(Phase::RollingBack);
        active.release.notify_one();
        tokio::time::timeout(WAIT, rollback.entered.notified()).await?;
        proof.unwind_gate()?;
        ensure!(tokio::time::timeout(WAIT, receive).await??.is_err());
        tokio::time::timeout(WAIT, proof.quarantined()).await?;
        ensure!(proof.disposition() == Disposition::Ambiguous);
        ensure!(proof.sql_counts_for_test() == (before.0 + 1, before.1));
        ensure!(!service.admission.receipts_ready() && service.current().is_err());
        ensure!(proof.ensure_clean().is_err());
        Ok(())
    }
    .await;
    active.release.notify_one();
    proof.release_gate();
    let stopped = tokio::time::timeout(WAIT, service.shutdown()).await;
    let closed = db.close().await;
    checked?;
    ensure!(
        stopped?.is_err(),
        "unknown worker cleanup was called successful shutdown"
    );
    closed?;
    ensure!(
        proof.ensure_clean().is_err(),
        "pool close was treated as transaction ACK"
    );
    Ok(())
}
