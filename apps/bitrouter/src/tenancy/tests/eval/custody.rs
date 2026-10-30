use super::*;
use crate::tenancy::transaction::{Disposition, Phase};
use std::sync::Arc;

#[tokio::test]
async fn cancelled_eval_http_read_retains_actor_capacity_until_explicit_rollback() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let fixture = Fixture::new().await?;
        let issued = fixture.issue("eval-cancel").await?;
        let secret = issued.secret.as_deref().context("secret")?;
        let router = router(&fixture)?;
        let gate = fixture.service.cleanup.hold(Phase::Active);
        let handler = tokio::spawn(router.clone().oneshot(request(
            &fixture,
            secret,
            "GET",
            "status",
            Vec::new(),
        )?));
        tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await?;
        handler.abort();
        ensure!(handler.await.is_err_and(|error| error.is_cancelled()));
        ensure!(fixture.service.cleanup.disposition() == Disposition::Pending);
        ensure!(fixture.service.eval.leases.available_permits() == 7);
        fixture.service.cleanup.release_gate();
        let _: StatusReport = json(
            router
                .oneshot(request(&fixture, secret, "GET", "status", Vec::new())?)
                .await?,
        )
        .await?;
        ensure!(fixture.service.eval.leases.available_permits() == 8);
        ensure!(fixture.service.cleanup.disposition() == Disposition::RolledBack);
        fixture.service.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn expired_owned_job_never_polls_begin_and_keeps_authority_live() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let fixture = Fixture::new().await?;
        let issued = fixture.issue("eval-expired").await?;
        let secret = issued.secret.as_deref().context("secret")?;
        let request = request(&fixture, secret, "GET", "status", Vec::new())?;
        let identity = fixture
            .service
            .eval_identity(
                request.headers(),
                fixture.tenant.clone(),
                crate::tenancy::eval::Action::Read,
            )
            .map_err(failure)?;
        let gate = fixture.service.cleanup.hold(Phase::Beginning);
        let (reply, receive) = tokio::sync::oneshot::channel();
        fixture
            .service
            .enqueue_eval(crate::tenancy::eval::Job {
                identity,
                request: crate::tenancy::eval::Request::Status,
                deadline: tokio::time::Instant::now(),
                lease: fixture.service.eval_lease().map_err(failure)?,
                reply,
            })
            .map_err(failure)?;
        let result = tokio::time::timeout(Duration::from_secs(3), receive).await??;
        ensure!(matches!(result, Err(error) if error.0.code == ErrorCode::CapacityExceeded));
        ensure!(fixture.service.cleanup.disposition() == Disposition::NoTransaction);
        ensure!(fixture.service.current().is_ok());
        ensure!(fixture.service.eval.leases.available_permits() == 8);
        // An accidentally polled BEGIN would have blocked at this gate and
        // could not have produced the response above.
        drop(gate);
        fixture.service.cleanup.release_gate();
        fixture.service.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn safety_revoke_ahead_of_queued_eval_prevents_ledger_mutation() -> Result<()> {
    use sea_orm::{ConnectionTrait, Statement};
    tokio::time::timeout(TIMEOUT, async {
        let fixture = Fixture::new().await?;
        let issued = fixture.issue("eval-queued-revoke").await?;
        let secret = issued.secret.as_deref().context("secret")?;
        let router = router(&fixture)?;
        let gate = fixture.service.cleanup.hold(Phase::Active);
        let first = tokio::spawn(router.clone().oneshot(request(
            &fixture,
            secret,
            "GET",
            "status",
            Vec::new(),
        )?));
        tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await?;
        let queued = tokio::spawn(router.oneshot(request(
            &fixture,
            secret,
            "POST",
            "subjects",
            serde_json::to_vec(&subject()?)?,
        )?));
        tokio::time::timeout(Duration::from_secs(3), async {
            while fixture.service.commands.capacity() == 16 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let service = fixture.service.clone();
        let operator = fixture.operator.clone();
        let command = Command::Revoke(
            fixture.tenant.clone(),
            issued.credential_id,
            Mutation {
                operation_id: "eval-safety-revoke".try_into()?,
                expected_revision: issued.credential_revision,
                installation_id: fixture.installation.clone(),
            },
        );
        let revoked =
            tokio::spawn(async move { service.execute(operator, command).await.map_err(failure) });
        tokio::time::timeout(Duration::from_secs(3), async {
            while fixture.service.safety.capacity() == 8 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        fixture.service.cleanup.release_gate();
        result(revoked.await??)?;
        drop(first.await??);
        let denied = queued.await??;
        ensure!(denied.status() == StatusCode::UNAUTHORIZED);
        drop(denied);
        let row = fixture
            .db
            .query_one(Statement::from_string(
                fixture.db.get_database_backend(),
                "SELECT COUNT(*) AS total FROM eval_subjects".to_owned(),
            ))
            .await?
            .context("subject count")?;
        ensure!(row.try_get::<i64>("", "total")? == 0);
        ensure!(fixture.service.eval.leases.available_permits() == 8);
        fixture.service.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn http_waiter_deadline_is_unavailable_and_timely_rollback_preserves_authority() -> Result<()>
{
    let fixture = Fixture::new().await?;
    let issued = fixture.issue("eval-timely").await?;
    let secret = issued.secret.as_deref().context("secret")?;
    let router = router(&fixture)?;
    let gate = fixture.service.cleanup.hold(Phase::Active);
    let handler = tokio::spawn(router.clone().oneshot(request(
        &fixture,
        secret,
        "POST",
        "subjects",
        serde_json::to_vec(&subject()?)?,
    )?));
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await?;
    tokio::time::pause();
    tokio::time::advance(crate::tenancy::eval::WORK).await;
    tokio::time::timeout(
        Duration::from_secs(1),
        fixture.service.cleanup.read_stopped.notified(),
    )
    .await?;
    tokio::time::resume();
    let timed_out = tokio::time::timeout(Duration::from_secs(3), handler).await???;
    ensure!(timed_out.status() == StatusCode::SERVICE_UNAVAILABLE);
    ensure!(fixture.service.eval.leases.available_permits() == 7);
    fixture.service.cleanup.release_gate();
    let status: StatusReport = json(
        tokio::time::timeout(
            Duration::from_secs(3),
            router.oneshot(request(&fixture, secret, "GET", "status", Vec::new())?),
        )
        .await??,
    )
    .await?;
    ensure!(status.subjects == 0 && fixture.service.current().is_ok());
    ensure!(fixture.service.cleanup.disposition() == Disposition::RolledBack);
    ensure!(fixture.service.eval.leases.available_permits() == 8);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn held_eval_begin_fences_after_cleanup_deadline_and_retains_capacity() -> Result<()> {
    let fixture = Fixture::new().await?;
    let issued = fixture.issue("eval-overdue").await?;
    let secret = issued.secret.as_deref().context("secret")?;
    let gate = fixture.service.cleanup.hold(Phase::Beginning);
    let handler = tokio::spawn(router(&fixture)?.oneshot(request(
        &fixture,
        secret,
        "GET",
        "status",
        Vec::new(),
    )?));
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await?;
    tokio::time::pause();
    tokio::time::advance(crate::tenancy::eval::WORK).await;
    tokio::time::timeout(
        Duration::from_secs(1),
        fixture.service.cleanup.read_stopped.notified(),
    )
    .await?;
    tokio::time::advance(crate::tenancy::transaction::CLEANUP_TIMEOUT + Duration::from_millis(1))
        .await;
    tokio::time::timeout(
        Duration::from_secs(1),
        fixture.service.cleanup.wait_fenced(),
    )
    .await?;
    tokio::time::resume();
    ensure!(fixture.service.current().is_err());
    ensure!(fixture.service.eval.leases.available_permits() == 7);
    ensure!(fixture.service.cleanup.disposition() == Disposition::Pending);
    ensure!(
        tokio::time::timeout(Duration::from_secs(3), handler)
            .await???
            .status()
            == StatusCode::SERVICE_UNAVAILABLE
    );
    let service = fixture.service.clone();
    let shutdown = tokio::spawn(async move { service.shutdown().await });
    tokio::time::timeout(Duration::from_secs(3), fixture.service.stop.cancelled()).await?;
    ensure!(!shutdown.is_finished());
    fixture.service.cleanup.release_gate();
    tokio::time::timeout(Duration::from_secs(3), shutdown).await???;
    ensure!(fixture.service.cleanup.disposition() == Disposition::RolledBack);
    ensure!(fixture.service.eval.leases.available_permits() == 8);
    Ok(())
}

#[tokio::test]
async fn late_eval_commit_remains_persisted_and_fenced_after_http_unavailable() -> Result<()> {
    use sea_orm::{ConnectionTrait, Statement};
    let fixture = Fixture::new().await?;
    let issued = fixture.issue("eval-late-commit").await?;
    let secret = issued.secret.as_deref().context("secret")?;
    let gate = fixture.service.cleanup.hold(Phase::Committing);
    let handler = tokio::spawn(router(&fixture)?.oneshot(request(
        &fixture,
        secret,
        "POST",
        "subjects",
        serde_json::to_vec(&subject()?)?,
    )?));
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await?;
    tokio::time::pause();
    tokio::time::advance(crate::tenancy::eval::WORK).await;
    tokio::time::timeout(
        Duration::from_secs(1),
        fixture.service.cleanup.wait_fenced(),
    )
    .await?;
    tokio::time::resume();
    ensure!(fixture.service.cleanup.disposition() == Disposition::Pending);
    ensure!(fixture.service.eval.leases.available_permits() == 7);
    ensure!(
        tokio::time::timeout(Duration::from_secs(3), handler)
            .await???
            .status()
            == StatusCode::SERVICE_UNAVAILABLE
    );
    fixture.service.cleanup.release_gate();
    tokio::time::timeout(Duration::from_secs(3), fixture.service.shutdown()).await??;
    ensure!(fixture.service.cleanup.disposition() == Disposition::Committed);
    ensure!(fixture.service.current().is_err());
    ensure!(fixture.service.eval.leases.available_permits() == 8);
    let row = fixture
        .db
        .query_one(Statement::from_string(
            fixture.db.get_database_backend(),
            "SELECT COUNT(*) AS total FROM eval_subjects".to_owned(),
        ))
        .await?
        .context("committed subject count")?;
    ensure!(row.try_get::<i64>("", "total")? == 1);
    Ok(())
}

#[tokio::test]
async fn consumed_eval_commit_error_retains_ingress_lease_and_never_acknowledges_drop_cleanup()
-> Result<()> {
    use sea_orm::{ConnectionTrait, Statement};
    tokio::time::timeout(TIMEOUT, async {
        let fixture = Fixture::new().await?;
        let issued = fixture.issue("eval-commit-error").await?;
        let secret = issued.secret.as_deref().context("secret")?;
        // Actual deferred-constraint COMMIT failure, not an injected postcommit Err.
        // https://www.sqlite.org/foreignkeys.html#fk_deferred
        for sql in [
            "PRAGMA foreign_keys=ON",
            "CREATE TABLE eval_custody_parent (id INTEGER PRIMARY KEY)",
            "CREATE TABLE eval_custody_child (parent INTEGER REFERENCES eval_custody_parent(id) DEFERRABLE INITIALLY DEFERRED)",
            "CREATE TRIGGER eval_commit_fault AFTER INSERT ON eval_subjects BEGIN INSERT INTO eval_custody_child(parent) VALUES (77); END",
        ] { fixture.db.execute_unprepared(sql).await?; }
        let response = router(&fixture)?.oneshot(request(&fixture, secret, "POST", "subjects", serde_json::to_vec(&subject()?)?)?).await?;
        ensure!(response.status() == StatusCode::SERVICE_UNAVAILABLE);
        drop(response);
        fixture.service.cleanup.quarantined().await;
        ensure!(fixture.service.cleanup.disposition() == Disposition::Ambiguous);
        ensure!(fixture.service.eval.leases.available_permits() == 7);
        ensure!(fixture.service.shutdown().await.is_err());
        let row = fixture.db.query_one(Statement::from_string(fixture.db.get_database_backend(), "SELECT COUNT(*) AS total FROM eval_subjects".to_owned())).await?.context("post-error count")?;
        ensure!(row.try_get::<i64>("", "total")? == 0);
        // A usable connection and absent rows do not acknowledge consumed cleanup.
        ensure!(fixture.service.cleanup.disposition() == Disposition::Ambiguous);
        ensure!(fixture.service.eval.leases.available_permits() == 7);
        Ok::<_, anyhow::Error>(())
    }).await?
}

#[tokio::test]
async fn revocation_after_commit_suppresses_http_success_without_deleting_evidence() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let fixture = Fixture::new().await?;
        let issued = fixture.issue("eval-reply-revoke").await?;
        let reader = fixture.issue("eval-reply-reader").await?;
        let secret = issued.secret.as_deref().context("secret")?;
        let gate = Arc::new(crate::tenancy::eval::Handoff::default());
        *fixture
            .service
            .eval
            .handoff
            .lock()
            .map_err(|_| anyhow::anyhow!("handoff fixture lock"))? = Some(gate.clone());
        let handler = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(
            router(&fixture)?.oneshot(request(
                &fixture,
                secret,
                "POST",
                "subjects",
                serde_json::to_vec(&subject()?)?,
            )?),
        ));
        tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await?;
        ensure!(fixture.service.cleanup.disposition() == Disposition::Committed);
        ensure!(fixture.service.eval.leases.available_permits() == 7);
        result(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Revoke(
                        fixture.tenant.clone(),
                        issued.credential_id,
                        Mutation {
                            operation_id: "eval-reply-revoke-command".try_into()?,
                            expected_revision: issued.credential_revision,
                            installation_id: fixture.installation.clone(),
                        },
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
        gate.release.notify_one();
        let denied = handler.await??;
        ensure!(denied.status() == StatusCode::UNAUTHORIZED);
        let error: bitrouter_ai::tenancy::operation::Error =
            serde_json::from_slice(&axum::body::to_bytes(denied.into_body(), 4096).await?)?;
        ensure!(error.operation_id.is_none() && error.execution_id.is_none());
        let persisted: EvalSubject = json(
            router(&fixture)?
                .oneshot(request(
                    &fixture,
                    reader.secret.as_deref().context("reader secret")?,
                    "GET",
                    "subjects/owned-subject",
                    Vec::new(),
                )?)
                .await?,
        )
        .await?;
        ensure!(persisted == subject()?);
        ensure!(fixture.service.eval.leases.available_permits() == 8);
        fixture.service.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
