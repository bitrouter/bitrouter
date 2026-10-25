use super::*;
use crate::tenancy::transaction::{CLEANUP_TIMEOUT, Disposition, Phase};
use futures::FutureExt;
use std::sync::atomic::Ordering;

pub(super) fn principal(fixture: &Fixture, name: &str) -> Result<Command> {
    Ok(Command::Principal(
        name.try_into()?,
        PrincipalPut {
            operation_id: name.try_into()?,
            expected_revision: None,
            issuer: "custody-test".into(),
            subject: name.into(),
            installation_id: fixture.installation.clone(),
        },
    ))
}

pub(super) async fn commit_fault(db: &DatabaseConnection) -> Result<()> {
    // This is a real COMMIT error, not a mocked error after a successful commit.
    // https://www.sqlite.org/foreignkeys.html#fk_deferred
    for sql in [
        "PRAGMA foreign_keys=ON",
        "CREATE TABLE custody_parent (id INTEGER PRIMARY KEY)",
        "CREATE TABLE custody_child (parent INTEGER REFERENCES custody_parent(id) DEFERRABLE INITIALLY DEFERRED)",
        "CREATE TRIGGER custody_commit_fault AFTER INSERT ON tenancy_operations WHEN NEW.operation_id = 'deferred-commit' BEGIN INSERT INTO custody_child(parent) VALUES (77); END",
    ] {
        db.execute_unprepared(sql).await?;
    }
    Ok(())
}

async fn count(db: &DatabaseConnection, sql: &str) -> Result<i64> {
    Ok(db
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            sql,
        ))
        .await?
        .context("count missing")?
        .try_get("", "count")?)
}

#[tokio::test]
async fn dml_failure_explicitly_rolls_back_before_cancelled_shutdown_can_finish() -> Result<()> {
    let fixture = Fixture::open(crate::db::connect("sqlite::memory:").await?, None).await?;
    fixture.db.execute_unprepared(
        "CREATE TRIGGER custody_dml_fault BEFORE INSERT ON tenancy_operations WHEN NEW.operation_id = 'failed-dml' BEGIN SELECT RAISE(ABORT, 'injected DML failure'); END"
    ).await?;
    let before = fixture.service.current().map_err(failure)?;
    let gate = fixture.service.cleanup.hold(Phase::RollingBack);
    let mut response = fixture.enqueue(principal(&fixture, "failed-dml")?)?;
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await?;
    ensure!(fixture.service.cleanup.disposition() == Disposition::Pending);
    ensure!(matches!(
        response.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    let snapshot = fixture
        .service
        .snapshot
        .read()
        .map_err(|_| anyhow::anyhow!("snapshot poisoned"))?
        .clone();
    ensure!(snapshot.revision == before.revision && snapshot.operation_ids == before.operation_ids);
    let service = fixture.service.clone();
    let shutdown = tokio::spawn(async move { service.shutdown().await });
    tokio::time::timeout(Duration::from_secs(3), fixture.service.stop.cancelled()).await?;
    ensure!(!shutdown.is_finished());
    shutdown.abort();
    ensure!(shutdown.await.is_err());
    ensure!(fixture.service.cleanup.disposition() == Disposition::Pending);
    fixture.service.cleanup.release_gate();
    let result = tokio::time::timeout(Duration::from_secs(3), response).await??;
    ensure!(matches!(result, Err(error) if error.0.code == ErrorCode::StorageUnavailable));
    tokio::time::timeout(Duration::from_secs(3), fixture.service.shutdown()).await??;
    ensure!(fixture.service.cleanup.disposition() == Disposition::RolledBack);
    ensure!(
        fixture
            .operation(&"failed-dml".try_into()?)
            .await?
            .is_none()
    );
    ensure!(
        count(
            &fixture.db,
            "SELECT COUNT(*) AS count FROM tenancy_principals WHERE id = 'failed-dml'"
        )
        .await?
            == 0
    );
    Ok(())
}

#[tokio::test]
async fn deferred_commit_error_quarantines_even_after_implicit_rollback_finishes() -> Result<()> {
    let fixture = Fixture::open(crate::db::connect("sqlite::memory:").await?, None).await?;
    commit_fault(&fixture.db).await?;
    let response = fixture.enqueue(principal(&fixture, "deferred-commit")?)?;
    let result = tokio::time::timeout(Duration::from_secs(3), response).await??;
    ensure!(matches!(result, Err(error) if error.0.code == ErrorCode::StorageUnavailable));
    tokio::time::timeout(
        Duration::from_secs(3),
        fixture.service.cleanup.quarantined(),
    )
    .await?;
    ensure!(fixture.service.current().is_err());
    ensure!(
        tokio::time::timeout(Duration::from_secs(3), fixture.service.shutdown())
            .await?
            .is_err()
    );
    // A subsequent successful query is deliberately not used as cleanup proof.
    ensure!(
        tokio::time::timeout(
            Duration::from_secs(3),
            fixture.operation(&"deferred-commit".try_into()?)
        )
        .await??
        .is_none()
    );
    ensure!(fixture.service.cleanup.disposition() == Disposition::Ambiguous);
    ensure!(fixture.service.completion.clone().now_or_never().is_none());
    ensure!(fixture.service.shutdown().await.is_err());
    Ok(())
}

#[tokio::test]
async fn disconnected_credential_waiter_keeps_read_capacity_until_rollback() -> Result<()> {
    let fixture = Fixture::open(crate::db::connect("sqlite::memory:").await?, None).await?;
    let gate = fixture.service.cleanup.hold(Phase::Active);
    let service = fixture.service.clone();
    let caller = fixture.operator.clone();
    let tenant = fixture.tenant.clone();
    let id = fixture.credential.credential_id.clone();
    let waiter = tokio::spawn(async move { service.credential(&caller, &tenant, &id).await });
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await?;
    ensure!(fixture.service.reads.available_permits() == 7);
    waiter.abort();
    ensure!(waiter.await.is_err());
    ensure!(fixture.service.reads.available_permits() == 7);
    fixture.service.cleanup.release_gate();
    // The priority revoke must wait for read cleanup, then use this same actor.
    applied(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                fixture.revoke("after-read-cleanup")?,
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(fixture.service.reads.available_permits() == 8);
    let status = fixture
        .service
        .credential(
            &fixture.operator,
            &fixture.tenant,
            &fixture.credential.credential_id,
        )
        .await
        .map_err(failure)?;
    ensure!(status.state == bitrouter_ai::tenancy::authority::CredentialState::Revoked);
    fixture.service.shutdown().await?;
    Ok(())
}

async fn overdue_read(phase: Phase) -> Result<()> {
    let fixture = Fixture::open(crate::db::connect("sqlite::memory:").await?, None).await?;
    let gate = fixture.service.cleanup.hold(phase);
    let service = fixture.service.clone();
    let caller = fixture.operator.clone();
    let tenant = fixture.tenant.clone();
    let id = fixture.credential.credential_id.clone();
    let waiter = tokio::spawn(async move { service.credential(&caller, &tenant, &id).await });
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await?;
    tokio::time::pause();
    tokio::time::advance(crate::tenancy::MANAGEMENT_TIMEOUT).await;
    tokio::time::timeout(
        Duration::from_secs(1),
        fixture.service.cleanup.read_stopped.notified(),
    )
    .await?;
    tokio::time::advance(CLEANUP_TIMEOUT + Duration::from_millis(1)).await;
    tokio::time::timeout(
        Duration::from_secs(1),
        fixture.service.cleanup.wait_fenced(),
    )
    .await?;
    tokio::time::resume();
    ensure!(!fixture.service.live.load(Ordering::Acquire));
    ensure!(fixture.service.reads.available_permits() == 7);
    let result = tokio::time::timeout(Duration::from_secs(3), waiter).await??;
    ensure!(matches!(result, Err(error) if error.0.code == ErrorCode::AuthorityUnavailable));
    let service = fixture.service.clone();
    let shutdown = tokio::spawn(async move { service.shutdown().await });
    tokio::time::timeout(Duration::from_secs(3), fixture.service.stop.cancelled()).await?;
    ensure!(!shutdown.is_finished());
    fixture.service.cleanup.release_gate();
    tokio::time::timeout(Duration::from_secs(3), shutdown).await???;
    ensure!(fixture.service.cleanup.disposition() == Disposition::RolledBack);
    ensure!(fixture.service.reads.available_permits() == 8);
    ensure!(!fixture.service.live.load(Ordering::Acquire));
    Ok(())
}

#[tokio::test]
async fn timely_deadline_rollback_preserves_authority_and_read_capacity() -> Result<()> {
    let fixture = Fixture::open(crate::db::connect("sqlite::memory:").await?, None).await?;
    let gate = fixture.service.cleanup.hold(Phase::Active);
    let service = fixture.service.clone();
    let caller = fixture.operator.clone();
    let tenant = fixture.tenant.clone();
    let id = fixture.credential.credential_id.clone();
    let waiter = tokio::spawn(async move { service.credential(&caller, &tenant, &id).await });
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await?;
    tokio::time::pause();
    tokio::time::advance(crate::tenancy::MANAGEMENT_TIMEOUT).await;
    tokio::time::timeout(
        Duration::from_secs(1),
        fixture.service.cleanup.read_stopped.notified(),
    )
    .await?;
    tokio::time::resume();
    fixture.service.cleanup.release_gate();
    ensure!(
        tokio::time::timeout(Duration::from_secs(3), waiter)
            .await??
            .is_err()
    );
    fixture
        .service
        .credential(
            &fixture.operator,
            &fixture.tenant,
            &fixture.credential.credential_id,
        )
        .await
        .map_err(failure)?;
    ensure!(fixture.service.current().is_ok());
    ensure!(fixture.service.cleanup.disposition() == Disposition::RolledBack);
    ensure!(fixture.service.reads.available_permits() == 8);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn expired_queued_credential_read_never_enters_begin() -> Result<()> {
    use crate::tenancy::actor::{CredentialRead, Job};
    let fixture = Fixture::open(crate::db::connect("sqlite::memory:").await?, None).await?;
    let gate = fixture.service.cleanup.hold(Phase::Beginning);
    let permit = fixture.service.reads.clone().try_acquire_owned()?;
    let (reply, response) = oneshot::channel();
    fixture
        .service
        .enqueue(
            Job::Credential(CredentialRead {
                caller: fixture.operator.clone(),
                id: fixture.credential.credential_id.clone(),
                deadline: tokio::time::Instant::now(),
                permit,
                reply,
            }),
            false,
        )
        .map_err(failure)?;
    let result = tokio::time::timeout(Duration::from_secs(3), response).await??;
    ensure!(matches!(result, Err(error) if error.0.code == ErrorCode::AuthorityUnavailable));
    ensure!(gate.entered.notified().now_or_never().is_none());
    ensure!(fixture.service.cleanup.disposition() == Disposition::NoTransaction);
    ensure!(fixture.service.reads.available_permits() == 8);
    ensure!(fixture.service.current().is_ok());
    fixture.service.cleanup.release_gate();
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn held_begin_and_active_read_fence_on_cleanup_deadline() -> Result<()> {
    overdue_read(Phase::Beginning).await?;
    overdue_read(Phase::Active).await
}

#[tokio::test]
async fn early_failed_write_has_its_own_cleanup_deadline() -> Result<()> {
    let fixture = Fixture::open(crate::db::connect("sqlite::memory:").await?, None).await?;
    fixture.db.execute_unprepared(
        "CREATE TRIGGER custody_early_fault BEFORE INSERT ON tenancy_operations WHEN NEW.operation_id = 'early-error' BEGIN SELECT RAISE(ABORT, 'injected early failure'); END"
    ).await?;
    let gate = fixture.service.cleanup.hold(Phase::RollingBack);
    let response = fixture.enqueue(principal(&fixture, "early-error")?)?;
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await?;
    ensure!(fixture.service.live.load(Ordering::Acquire));
    tokio::time::pause();
    tokio::time::advance(CLEANUP_TIMEOUT + Duration::from_millis(1)).await;
    tokio::time::timeout(
        Duration::from_secs(1),
        fixture.service.cleanup.wait_fenced(),
    )
    .await?;
    tokio::time::resume();
    ensure!(fixture.service.cleanup.disposition() == Disposition::Pending);
    fixture.service.cleanup.release_gate();
    ensure!(
        tokio::time::timeout(Duration::from_secs(3), response)
            .await??
            .is_err()
    );
    fixture.service.shutdown().await?;
    ensure!(fixture.service.cleanup.disposition() == Disposition::RolledBack);
    Ok(())
}

#[tokio::test]
async fn consuming_rollback_error_is_not_cleanup_confirmation() -> Result<()> {
    use crate::tenancy::transaction::{Control, Proof};
    use std::sync::{Arc, atomic::AtomicBool};
    let db = crate::db::connect("sqlite::memory:").await?;
    let live = Arc::new(AtomicBool::new(true));
    let proof = Proof::new(live.clone());
    let control = Control::new(proof.clone());
    let transaction = control.begin(&db).await?;
    // Deliberately desynchronize this test connection behind the ORM so its
    // consuming rollback really returns Err. This is fault injection, not an
    // application transaction pattern or an acknowledgement of Drop cleanup.
    transaction.execute_unprepared("ROLLBACK").await?;
    ensure!(control.rollback(transaction).await.is_err());
    ensure!(proof.disposition() == Disposition::Ambiguous);
    ensure!(!live.load(Ordering::Acquire));
    ensure!(proof.ensure_clean().is_err());
    tokio::time::timeout(Duration::from_secs(3), db.close()).await??;
    ensure!(proof.ensure_clean().is_err());
    Ok(())
}

#[tokio::test]
async fn typed_closed_pool_failure_proves_no_transaction_was_created() -> Result<()> {
    use crate::tenancy::transaction::{Control, Proof};
    use std::sync::{Arc, atomic::AtomicBool};
    let db = crate::db::connect("sqlite::memory:").await?;
    db.clone().close().await?;
    let live = Arc::new(AtomicBool::new(true));
    let proof = Proof::new(live.clone());
    let control = Control::new(proof.clone());
    ensure!(control.begin(&db).await.is_err());
    ensure!(proof.disposition() == Disposition::NoTransaction);
    ensure!(live.load(Ordering::Acquire));
    proof.ensure_clean()?;
    Ok(())
}

#[tokio::test]
async fn transaction_work_unwind_quarantines_and_retains_active_read_capacity() -> Result<()> {
    let fixture = Fixture::open(crate::db::connect("sqlite::memory:").await?, None).await?;
    let gate = fixture.service.cleanup.hold(Phase::Active);
    let service = fixture.service.clone();
    let caller = fixture.operator.clone();
    let tenant = fixture.tenant.clone();
    let id = fixture.credential.credential_id.clone();
    let waiter = tokio::spawn(async move { service.credential(&caller, &tenant, &id).await });
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await?;
    fixture.service.cleanup.unwind_gate()?;
    let result = tokio::time::timeout(Duration::from_secs(3), waiter).await??;
    ensure!(matches!(result, Err(error) if error.0.code == ErrorCode::AuthorityUnavailable));
    ensure!(fixture.service.cleanup.disposition() == Disposition::Ambiguous);
    ensure!(fixture.service.reads.available_permits() == 7);
    ensure!(fixture.service.shutdown().await.is_err());
    ensure!(fixture.service.completion.clone().now_or_never().is_none());
    ensure!(fixture.service.reads.available_permits() == 7);
    Ok(())
}
