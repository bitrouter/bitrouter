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
