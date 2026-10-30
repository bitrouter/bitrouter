use super::*;
use crate::tenancy::{command::Command, transaction::Phase};
use bitrouter_ai::tenancy::{execution::Outcome, operation::Mutation};
use sea_orm::ConnectionTrait;

mod additional;

#[tokio::test]
async fn revoke_during_refresh_persists_replacement_but_forbids_first_model_send() -> Result<()> {
    let mut harness = Harness::new(true).await?;
    let request = harness.send(&Harness::client(false)?, false);
    let caller = AbortOnDropHandle::new(tokio::spawn(async move {
        let response = request.send().await?;
        let id = execution_id(&response)?;
        let status = response.status();
        response.bytes().await?;
        Ok::<_, anyhow::Error>((id, status))
    }));
    tokio::time::timeout(TIMEOUT, harness.refresh_seen.recv())
        .await?
        .context("actual refresh POST")?;
    harness
        .fixture
        .service
        .execute(
            harness.fixture.operator.clone(),
            Command::Revoke(
                harness.fixture.tenant.clone(),
                harness.credential.credential_id.clone(),
                Mutation {
                    operation_id: "revoke-refresh".try_into()?,
                    expected_revision: harness.credential.credential_revision.clone(),
                    installation_id: harness.fixture.installation.clone(),
                },
            ),
        )
        .await
        .map_err(failure)?;
    harness.provider.refresh_gate.add_permits(1);
    let (id, status) = tokio::time::timeout(TIMEOUT, caller).await???;
    ensure!(
        status == StatusCode::UNAUTHORIZED && harness.provider.models.load(Ordering::SeqCst) == 0
    );
    let row = harness.row().await?;
    ensure!(row.metadata.generation == 1 && matches!(row.metadata.state, storage::State::Ready));
    let receipt = harness.receipt(&id).await?;
    ensure!(receipt.attempts.is_empty() && receipt.outcome != Outcome::Completed);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn invalid_or_oversized_refresh_quarantines_the_seed_without_automatic_retry() -> Result<()> {
    for reply in [
        "{}".to_owned(),
        json!({"access_token":"new","expires_in":3600}).to_string(),
        "x".repeat(256 * 1024 + 1),
    ] {
        let mut harness = Harness::new(true).await?;
        *harness
            .provider
            .reply
            .lock()
            .map_err(|_| anyhow::anyhow!("reply lock"))? = (StatusCode::OK, reply);
        harness.provider.refresh_gate.add_permits(1);
        let client = Harness::client(false)?;
        let response = harness.send(&client, false).send().await?;
        ensure!(response.status() != StatusCode::OK);
        let id = execution_id(&response)?;
        response.bytes().await?;
        ensure!(
            harness.row().await?.metadata.generation == 0
                && matches!(
                    harness.row().await?.metadata.state,
                    storage::State::Quarantined(_)
                )
        );
        ensure!(harness.receipt(&id).await?.attempts.is_empty());
        harness.reopen().await?;
        let response = harness.send(&client, false).send().await?;
        ensure!(response.status() != StatusCode::OK);
        response.bytes().await?;
        ensure!(
            harness.provider.refreshes.load(Ordering::SeqCst) == 1
                && harness.provider.models.load(Ordering::SeqCst) == 0
        );
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn held_refresh_start_commit_retains_admission_and_a_lost_http_waiter_cannot_repeat_refresh()
-> Result<()> {
    let harness = Harness::new(true).await?;
    let gate = harness.fixture.service.cleanup.hold(Phase::Committing);
    let request = harness.send(&Harness::client(false)?, true);
    let caller =
        AbortOnDropHandle::new(tokio::spawn(
            async move { request.send().await?.bytes().await },
        ));
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    ensure!(harness.provider.refreshes.load(Ordering::SeqCst) == 0);
    caller.abort();
    ensure!(caller.await.is_err_and(|error| error.is_cancelled()));
    ensure!(
        tokio::time::timeout(
            Duration::from_millis(20),
            harness.fixture.service.admission.wait_drained()
        )
        .await
        .is_err()
    );
    harness.provider.refresh_gate.add_permits(1);
    harness.provider.model_gate.add_permits(1);
    harness.fixture.service.cleanup.release_gate();
    tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await??;
    // The owned inline task may continue or cancel before refresh. Neither case
    // permits a duplicate POST; started state cannot be reused as ready seed.
    let row = harness.row().await?;
    ensure!(harness.provider.refreshes.load(Ordering::SeqCst) <= 1);
    ensure!(harness.provider.models.load(Ordering::SeqCst) <= 1);
    if harness.provider.refreshes.load(Ordering::SeqCst) == 1 {
        ensure!(
            row.metadata.generation == 1 || !matches!(row.metadata.state, storage::State::Ready)
        );
    }
    Ok(())
}

#[tokio::test]
async fn real_refresh_replacement_rollback_keeps_same_ciphertext_candidate_and_never_reposts()
-> Result<()> {
    let harness = Harness::new(true).await?;
    harness.fixture.db.execute_unprepared("CREATE TRIGGER oauth_refuse_replace BEFORE UPDATE ON tenancy_oauth_slots WHEN NEW.generation=1 BEGIN SELECT RAISE(ABORT,'owned OAuth replacement refused'); END").await?;
    harness.provider.refresh_gate.add_permits(1);
    let response = harness.send(&Harness::client(false)?, false).send().await?;
    ensure!(response.status() != StatusCode::OK);
    response.bytes().await?;
    ensure!(
        harness.provider.refreshes.load(Ordering::SeqCst) == 1
            && harness.provider.models.load(Ordering::SeqCst) == 0
    );
    ensure!(
        harness.row().await?.metadata.generation == 0
            && !matches!(harness.row().await?.metadata.state, storage::State::Ready)
    );
    ensure!(
        tokio::time::timeout(
            Duration::from_millis(20),
            harness.fixture.service.admission.wait_drained()
        )
        .await
        .is_err()
    );
    harness
        .fixture
        .db
        .execute_unprepared("DROP TRIGGER oauth_refuse_replace")
        .await?;
    // No timer or request can refresh the retained seed after a failed release.
    let response = harness.send(&Harness::client(false)?, false).send().await?;
    ensure!(response.status() != StatusCode::OK);
    response.bytes().await?;
    ensure!(harness.provider.refreshes.load(Ordering::SeqCst) == 1);
    // Terminal cleanup is deliberately unclean; the test does not declare drain.
    ensure!(
        tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown())
            .await?
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn actual_chunked_refresh_response_is_bounded_before_parsing_without_content_length()
-> Result<()> {
    let mut harness = Harness::new(true).await?;
    harness
        .provider
        .chunked_refresh
        .store(true, Ordering::Release);
    *harness
        .provider
        .reply
        .lock()
        .map_err(|_| anyhow::anyhow!("reply lock"))? = (StatusCode::OK, "x".repeat(256 * 1024 + 1));
    harness.provider.refresh_gate.add_permits(1);
    let response = harness.send(&Harness::client(false)?, false).send().await?;
    ensure!(!response.status().is_success());
    let id = execution_id(&response)?;
    response.bytes().await?;
    tokio::time::timeout(TIMEOUT, harness.refresh_seen.recv())
        .await?
        .context("actual chunked refresh")?;
    ensure!(
        harness.provider.refreshes.load(Ordering::SeqCst) == 1
            && harness.provider.models.load(Ordering::SeqCst) == 0
    );
    ensure!(matches!(
        harness.row().await?.metadata.state,
        storage::State::Quarantined(_)
    ));
    ensure!(harness.receipt(&id).await?.attempts.is_empty());
    harness.fixture.service.shutdown().await?;
    harness.close().await
}

#[tokio::test]
async fn model_auth_rejection_after_refresh_never_replays_refresh_or_model() -> Result<()> {
    for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
        let harness = Harness::new(true).await?;
        *harness
            .provider
            .model_status
            .lock()
            .map_err(|_| anyhow::anyhow!("model status lock"))? = status;
        harness.provider.refresh_gate.add_permits(1);
        harness.provider.model_gate.add_permits(1);
        let response = harness.send(&Harness::client(false)?, false).send().await?;
        ensure!(!response.status().is_success());
        let id = execution_id(&response)?;
        response.bytes().await?;
        let receipt = harness.receipt(&id).await?;
        ensure!(receipt.attempts.len() == 1 && receipt.outcome != Outcome::Completed);
        ensure!(
            harness.provider.refreshes.load(Ordering::SeqCst) == 1
                && harness.provider.models.load(Ordering::SeqCst) == 1
        );
        ensure!(harness.row().await?.metadata.generation == 1);
        harness.fixture.service.shutdown().await?;
        harness.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn lost_http_waiter_after_real_refresh_keeps_replacement_commit_and_never_reuses_seed()
-> Result<()> {
    let mut harness = Harness::new(true).await?;
    let request = harness.send(&Harness::client(false)?, false);
    let caller =
        AbortOnDropHandle::new(tokio::spawn(
            async move { request.send().await?.bytes().await },
        ));
    tokio::time::timeout(TIMEOUT, harness.refresh_seen.recv())
        .await?
        .context("actual refresh before cancellation")?;
    let gate = harness.fixture.service.cleanup.hold(Phase::Committing);
    harness.provider.refresh_gate.add_permits(1);
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    ensure!(harness.provider.refreshes.load(Ordering::SeqCst) == 1);
    caller.abort();
    ensure!(caller.await.is_err_and(|error| error.is_cancelled()));
    ensure!(
        tokio::time::timeout(
            Duration::from_millis(20),
            harness.fixture.service.admission.wait_drained()
        )
        .await
        .is_err()
    );
    harness.provider.model_gate.add_permits(2);
    harness.fixture.service.cleanup.release_gate();
    tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await??;
    ensure!(harness.row().await?.metadata.generation == 1);
    let stale_headers = harness.headers.clone();
    let models_before_reopen = harness.provider.models.load(Ordering::SeqCst);
    harness.reopen().await?;
    ensure!(harness.headers["bitrouter-instance-id"] != stale_headers["bitrouter-instance-id"]);
    for name in [
        "authorization",
        "bitrouter-installation-id",
        "bitrouter-execution-revision",
        "bitrouter-correlation-id",
    ] {
        ensure!(harness.headers[name] == stale_headers[name]);
    }
    let client = Harness::client(false)?;
    let stale = client
        .post(format!("{}/v1/messages", harness.base))
        .headers(stale_headers)
        .json(
            &json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"max_tokens":25}),
        )
        .send()
        .await?;
    ensure!(stale.status() == StatusCode::CONFLICT);
    ensure!(!stale.headers().contains_key("bitrouter-execution-id"));
    let rejected: bitrouter_ai::tenancy::operation::Error = stale.json().await?;
    ensure!(
        rejected.code == bitrouter_ai::tenancy::operation::ErrorCode::CapabilityMismatch
            && rejected.execution_id.is_none()
            && rejected.dispatch_state
                == bitrouter_ai::tenancy::operation::DispatchState::NotStarted
    );
    ensure!(
        harness.provider.refreshes.load(Ordering::SeqCst) == 1
            && harness.provider.models.load(Ordering::SeqCst) == models_before_reopen
    );
    let response = harness.send(&client, false).send().await?;
    ensure!(response.status() == StatusCode::OK);
    let id = execution_id(&response)?;
    response.bytes().await?;
    ensure!(harness.receipt(&id).await?.attempts.len() == 1);
    ensure!(harness.provider.refreshes.load(Ordering::SeqCst) == 1);
    harness.fixture.service.shutdown().await?;
    harness.close().await
}

#[tokio::test]
async fn revocation_at_actual_oauth_read_wait_forbids_refresh_and_model() -> Result<()> {
    let harness = Harness::new(true).await?;
    let gate = harness
        .fixture
        .service
        .hold_receipt_credential_read_for_test()?;
    let request = harness.send(&Harness::client(false)?, false);
    let caller = AbortOnDropHandle::new(tokio::spawn(async move { request.send().await }));
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    let command = Command::Revoke(
        harness.fixture.tenant.clone(),
        harness.credential.credential_id.clone(),
        Mutation {
            operation_id: "revoke-oauth-read".try_into()?,
            expected_revision: harness.credential.credential_revision.clone(),
            installation_id: harness.fixture.installation.clone(),
        },
    );
    let revoked = harness
        .fixture
        .service
        .execute(harness.fixture.operator.clone(), command)
        .await;
    gate.release.cancel();
    revoked.map_err(failure)?;
    let response = tokio::time::timeout(TIMEOUT, caller).await???;
    ensure!(response.status() == StatusCode::UNAUTHORIZED);
    let id = execution_id(&response)?;
    response.bytes().await?;
    ensure!(gate.execution.get() == Some(&id));
    ensure!(harness.receipt(&id).await?.attempts.is_empty());
    ensure!(
        harness.provider.refreshes.load(Ordering::SeqCst) == 0
            && harness.provider.models.load(Ordering::SeqCst) == 0
    );
    ensure!(
        harness.row().await?.metadata.generation == 0
            && matches!(harness.row().await?.metadata.state, storage::State::Ready)
    );
    harness.fixture.service.shutdown().await?;
    harness.close().await
}

#[tokio::test]
async fn a_real_other_refresh_invalidates_the_old_generation_held_at_the_read_boundary()
-> Result<()> {
    let harness = Harness::new(true).await?;
    let gate = harness
        .fixture
        .service
        .hold_receipt_credential_read_for_test()?;
    let request = harness.send(&Harness::client(false)?, false);
    let first = AbortOnDropHandle::new(tokio::spawn(async move { request.send().await }));
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    harness.provider.refresh_gate.add_permits(1);
    harness.provider.model_gate.add_permits(1);
    let second = harness.send(&Harness::client(false)?, false).send().await;
    // Always free the read gate even if the second request failed, so diagnostics
    // cannot turn a fixture error into a leaked owner.
    let second_result = async {
        let second = second?;
        ensure!(second.status() == StatusCode::OK);
        let id = execution_id(&second)?;
        second.bytes().await?;
        ensure!(harness.receipt(&id).await?.attempts.len() == 1);
        ensure!(harness.row().await?.metadata.generation == 1);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    gate.release.cancel();
    second_result?;
    let response = tokio::time::timeout(TIMEOUT, first).await???;
    ensure!(response.status() == StatusCode::FORBIDDEN);
    let id = execution_id(&response)?;
    response.bytes().await?;
    ensure!(gate.execution.get() == Some(&id) && harness.receipt(&id).await?.attempts.is_empty());
    ensure!(
        harness.provider.refreshes.load(Ordering::SeqCst) == 1
            && harness.provider.models.load(Ordering::SeqCst) == 1
    );
    harness.fixture.service.shutdown().await?;
    harness.close().await
}
