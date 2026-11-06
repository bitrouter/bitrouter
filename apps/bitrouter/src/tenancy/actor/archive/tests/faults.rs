use super::*;
use crate::tenancy::{
    actor::Job,
    transaction::{Disposition as SqlDisposition, Phase},
};
use sea_orm::{ConnectionTrait, Statement};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::Instant;

const WAIT: Duration = Duration::from_secs(10);
// Stop while the error's rollback is held, before its one-second cleanup fence.
const ERROR_DEADLINE: Duration = Duration::from_millis(500);

fn enqueue(
    service: &Service,
    deadline: Option<Instant>,
) -> Result<oneshot::Receiver<Result<bool, Failure>>> {
    let (reply, receive) = oneshot::channel();
    service
        .commands
        .try_send(Job::Archive { deadline, reply })
        .map_err(|_| anyhow::anyhow!("archive queue unavailable"))?;
    Ok(receive)
}

#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
fn physical(service: &Service) -> Result<u64> {
    Ok(service
        .admission
        .observed_capacity()
        .map_err(|error| anyhow::anyhow!("capacity observation: {error:?}"))?
        .physical_write_reserved)
}

async fn inline_only(db: &DatabaseConnection) -> Result<()> {
    let row = db.query_one(Statement::from_string(db.get_database_backend(),
        "SELECT (SELECT COUNT(*) FROM tenancy_receipt_archive) AS archived, (SELECT COUNT(*) FROM tenancy_execution_receipts WHERE storage_kind=0) AS inline"))
        .await?.context("receipt storage counts")?;
    ensure!(row.try_get::<i64>("", "archived")? == 0);
    ensure!(row.try_get::<i64>("", "inline")? == 1);
    Ok(())
}

#[tokio::test]
async fn sql_error_survives_read_stop_and_acknowledged_rollback_refunds_physical_charge()
-> Result<()> {
    let (db, candidate) = seeded().await?;
    let service = Service::open(db.clone(), None).await?;
    // ABORT fails the real INSERT but leaves the transaction for Control's ACK.
    // https://www.sqlite.org/lang_createtrigger.html#the_raise_function
    db.execute_unprepared("CREATE TRIGGER archive_sql_fault BEFORE INSERT ON tenancy_receipt_archive BEGIN SELECT RAISE(ABORT,'archive SQL fixture'); END").await?;
    let proof = &service.cleanup;
    let before = proof.sql_counts_for_test();
    let rollback = proof.hold(Phase::RollingBack);
    ensure!(ERROR_DEADLINE < crate::tenancy::transaction::CLEANUP_TIMEOUT);
    let receive = enqueue(&service, Some(Instant::now() + ERROR_DEADLINE))?;
    tokio::time::timeout(WAIT, rollback.entered.notified()).await?;
    ensure!(proof.sql_counts_for_test() == (before.0 + 1, before.1));
    // Keep the same gate for the write rollback; release only selection's ACK.
    rollback.release.notify_one();
    tokio::time::timeout(WAIT, rollback.entered.notified()).await?;
    ensure!(proof.sql_counts_for_test() == (before.0 + 2, before.1 + 1));
    ensure!(proof.disposition() == SqlDisposition::Pending);
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    ensure!(physical(&service)? > 0);
    tokio::time::timeout(WAIT, proof.read_stopped.notified()).await?;
    ensure!(service.admission.receipts_ready() && service.current().is_ok());
    proof.release_gate();
    ensure!(tokio::time::timeout(WAIT, receive).await??.is_err());
    ensure!(proof.sql_counts_for_test() == (before.0 + 2, before.1 + 2));
    ensure!(proof.disposition() == SqlDisposition::RolledBack);
    ensure!(!service.admission.receipts_ready() && service.current().is_err());
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    ensure!(physical(&service)? == 0);
    inline_only(&db).await?;
    let receipt = &candidate.receipt;
    let saved = store::lookup_candidate(
        &db,
        &receipt.installation_id,
        &receipt.tenant_id,
        &receipt.execution_id,
    )
    .await?
    .context("rolled-back receipt missing")?;
    ensure!(saved.digest == candidate.digest);
    service.shutdown().await?;
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn canonical_decode_error_survives_read_stop_during_selection_rollback() -> Result<()> {
    let (db, candidate) = seeded().await?;
    let service = Service::open(db.clone(), None).await?;
    db.execute_unprepared(
        "UPDATE tenancy_execution_receipts SET receipt_json='{' WHERE execution_id='execution'",
    )
    .await?;
    let receipt = &candidate.receipt;
    let error = store::lookup_candidate(
        &db,
        &receipt.installation_id,
        &receipt.tenant_id,
        &receipt.execution_id,
    )
    .await
    .err()
    .context("expected actual canonical decode error")?;
    ensure!(error.is::<serde_json::Error>());
    let proof = &service.cleanup;
    let before = proof.sql_counts_for_test();
    let rollback = proof.hold(Phase::RollingBack);
    ensure!(ERROR_DEADLINE < crate::tenancy::transaction::CLEANUP_TIMEOUT);
    let receive = enqueue(&service, Some(Instant::now() + ERROR_DEADLINE))?;
    tokio::time::timeout(WAIT, rollback.entered.notified()).await?;
    tokio::time::timeout(WAIT, proof.read_stopped.notified()).await?;
    ensure!(service.admission.receipts_ready() && service.current().is_ok());
    proof.release_gate();
    ensure!(tokio::time::timeout(WAIT, receive).await??.is_err());
    ensure!(proof.sql_counts_for_test() == (before.0 + 1, before.1 + 1));
    ensure!(proof.disposition() == SqlDisposition::RolledBack);
    ensure!(!service.admission.receipts_ready() && service.current().is_err());
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    ensure!(physical(&service)? == 0);
    inline_only(&db).await?;
    service.shutdown().await?;
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn failed_sql_rollback_retains_fence_and_physical_charge() -> Result<()> {
    let (db, _) = seeded().await?;
    let service = Service::open(db.clone(), None).await?;
    // SQLite rolls back inside the failed INSERT. The consuming ROLLBACK then
    // fails with no active transaction; no acknowledged refund is permitted.
    // https://www.sqlite.org/lang_conflict.html
    db.execute_unprepared("CREATE TRIGGER archive_rollback_fault BEFORE INSERT ON tenancy_receipt_archive BEGIN SELECT RAISE(ROLLBACK,'archive rollback fixture'); END").await?;
    let proof = &service.cleanup;
    let before = proof.sql_counts_for_test();
    let rollback = proof.hold(Phase::RollingBack);
    let receive = enqueue(&service, None)?;
    tokio::time::timeout(WAIT, rollback.entered.notified()).await?;
    rollback.release.notify_one();
    tokio::time::timeout(WAIT, rollback.entered.notified()).await?;
    ensure!(proof.sql_counts_for_test() == (before.0 + 2, before.1 + 1));
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    let reserved = physical(&service)?;
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    ensure!(reserved > 0);
    proof.release_gate();
    ensure!(tokio::time::timeout(WAIT, receive).await??.is_err());
    tokio::time::timeout(WAIT, proof.quarantined()).await?;
    ensure!(proof.disposition() == SqlDisposition::Ambiguous && proof.ensure_clean().is_err());
    ensure!(proof.sql_counts_for_test() == (before.0 + 2, before.1 + 1));
    ensure!(!service.admission.receipts_ready() && service.current().is_err());
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    ensure!(physical(&service)? == reserved);
    inline_only(&db).await?;
    ensure!(
        tokio::time::timeout(WAIT, service.shutdown())
            .await?
            .is_err()
    );
    // The quarantined actor intentionally survives until this test runtime exits.
    db.close().await?;
    Ok(())
}
