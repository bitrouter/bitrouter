use super::*;
use crate::tenancy::transaction::{Disposition, Phase};
use bitrouter_ai::tenancy::execution::Outcome;
use sea_orm::ConnectionTrait;

#[tokio::test]
async fn connected_json_and_sse_terminal_wait_for_real_receipt_commit_in_h1_and_h2() -> Result<()> {
    let modes: &[bool] = if cfg!(feature = "http2") {
        &[false, true]
    } else {
        &[false]
    };
    for &h2 in modes {
        for stream in [false, true] {
            let body = if stream {
                super::wire::chat_stream(full_usage("chat_completions"), true)?
            } else {
                serde_json::to_string(&json_response(
                    "chat_completions",
                    full_usage("chat_completions"),
                )?)?
            };
            let mut harness =
                Harness::with_reply("chat_completions", StatusCode::OK, body, stream, None).await?;
            let gate = harness.fixture.service.cleanup.hold(Phase::Committing);
            let client = Harness::client(h2)?;
            let request = harness.send(&client);
            let task = AbortOnDropHandle::new(tokio::spawn(async move {
                let response = request.send().await?;
                let id = execution_id(&response)?;
                let status = response.status();
                let version = response.version();
                let body = response.bytes().await?;
                Ok::<_, anyhow::Error>((id, status, version, body))
            }));
            tokio::time::timeout(TIMEOUT, harness.received.recv())
                .await?
                .context("provider send")?;
            tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
            ensure!(harness.fixture.service.cleanup.disposition() == Disposition::Pending);
            tokio::task::yield_now().await;
            ensure!(
                !task.is_finished(),
                "caller terminal escaped an uncommitted receipt"
            );
            ensure!(
                tokio::time::timeout(
                    Duration::from_millis(20),
                    harness.fixture.service.admission.wait_drained()
                )
                .await
                .is_err()
            );
            harness.fixture.service.cleanup.release_gate();
            let (id, status, version, body) = tokio::time::timeout(TIMEOUT, task).await???;
            ensure!(status == StatusCode::OK && !body.is_empty());
            ensure!(
                version
                    == if h2 {
                        ::http::Version::HTTP_2
                    } else {
                        ::http::Version::HTTP_11
                    }
            );
            let receipt = harness.receipt(&client, &id).await?;
            ensure!(receipt.outcome == Outcome::Completed && receipt.attempts.len() == 1);
            ensure!(harness.calls.load(Ordering::SeqCst) == 1);
            harness.fixture.service.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn disconnect_during_commit_keeps_writer_registry_and_reservation_until_settled() -> Result<()>
{
    for stream in [false, true] {
        let body = if stream {
            super::wire::chat_stream(full_usage("chat_completions"), true)?
        } else {
            serde_json::to_string(&json_response(
                "chat_completions",
                full_usage("chat_completions"),
            )?)?
        };
        let harness =
            Harness::with_reply("chat_completions", StatusCode::OK, body, stream, None).await?;
        let gate = harness.fixture.service.cleanup.hold(Phase::Committing);
        let client = Harness::client(false)?;
        let request = harness.send(&client);
        let caller =
            AbortOnDropHandle::new(tokio::spawn(
                async move { request.send().await?.bytes().await },
            ));
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        caller.abort();
        ensure!(caller.await.is_err_and(|error| error.is_cancelled()));
        let service = harness.fixture.service.clone();
        let shutdown =
            AbortOnDropHandle::new(tokio::spawn(async move { service.shutdown().await }));
        tokio::task::yield_now().await;
        ensure!(!shutdown.is_finished());
        ensure!(harness.fixture.service.cleanup.disposition() == Disposition::Pending);
        ensure!(
            tokio::time::timeout(
                Duration::from_millis(20),
                harness.fixture.service.admission.wait_drained()
            )
            .await
            .is_err()
        );
        harness.fixture.service.cleanup.release_gate();
        tokio::time::timeout(TIMEOUT, shutdown).await???;
        let tx = sea_orm::TransactionTrait::begin(&harness.fixture.db).await?;
        let page = store::page(
            &tx,
            &harness.fixture.installation,
            &harness.fixture.tenant,
            &"same-correlation".try_into()?,
            None,
            100,
        )
        .await?;
        ensure!(page.execution_ids.len() == 1 && !page.multiple_executions);
        let receipt = store::lookup(
            &tx,
            &harness.fixture.installation,
            &harness.fixture.tenant,
            &page.execution_ids[0],
        )
        .await?
        .context("disconnected durable receipt")?;
        ensure!(receipt.attempts.len() == 1 && receipt.attempts[0].outcome == Outcome::Completed);
        tx.rollback().await?;
        ensure!(harness.calls.load(Ordering::SeqCst) == 1);
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn confirmed_receipt_rollback_returns_unavailable_and_reconciles_identical_evidence_without_inference()
-> Result<()> {
    reconcile_receipt_rollback(true).await
}

#[tokio::test]
async fn first_shutdown_reconciles_confirmed_receipt_rollback_without_an_earlier_waiter()
-> Result<()> {
    reconcile_receipt_rollback(false).await
}

async fn reconcile_receipt_rollback(first_shutdown_refused: bool) -> Result<()> {
    let harness = Harness::new(
        "chat_completions",
        StatusCode::OK,
        json_response("chat_completions", full_usage("chat_completions"))?,
    )
    .await?;
    harness.fixture.db.execute_unprepared("CREATE TRIGGER receipt_test_failure BEFORE INSERT ON tenancy_execution_receipts BEGIN SELECT RAISE(ABORT, 'receipt fixture refusal'); END").await?;
    let client = Harness::client(false)?;
    let response = harness.send(&client).send().await?;
    ensure!(response.status() == StatusCode::SERVICE_UNAVAILABLE);
    let id = execution_id(&response)?;
    let _ = response.bytes().await?;
    ensure!(harness.fixture.service.cleanup.disposition() == Disposition::RolledBack);
    ensure!(
        store::lookup(
            &harness.fixture.db,
            &harness.fixture.installation,
            &harness.fixture.tenant,
            &id
        )
        .await?
        .is_none()
    );
    ensure!(
        tokio::time::timeout(
            Duration::from_millis(20),
            harness.fixture.service.admission.wait_drained()
        )
        .await
        .is_err()
    );
    let rejected = harness.send(&client).send().await?;
    ensure!(!rejected.status().is_success());
    let _ = rejected.bytes().await?;
    ensure!(harness.calls.load(Ordering::SeqCst) == 1);
    // Cover both an already-observed failure and a fresh shutdown waiter.
    // The latter must join the original drain before deciding to reconcile.
    if first_shutdown_refused {
        ensure!(
            tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown())
                .await?
                .is_err()
        );
    }
    harness
        .fixture
        .db
        .execute_unprepared("DROP TRIGGER receipt_test_failure")
        .await?;
    tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await??;
    let receipt = store::lookup(
        &harness.fixture.db,
        &harness.fixture.installation,
        &harness.fixture.tenant,
        &id,
    )
    .await?
    .context("reconciled immutable evidence")?;
    ensure!(
        receipt.execution_id == id
            && receipt.attempts.len() == 1
            && receipt.attempts[0].outcome == Outcome::Completed
    );
    ensure!(receipt.usage.input_tokens == Some(11) && receipt.usage.output_tokens == Some(7));
    let before = serde_json::to_vec(&receipt)?;
    harness.fixture.service.shutdown().await?;
    let after = store::lookup(
        &harness.fixture.db,
        &harness.fixture.installation,
        &harness.fixture.tenant,
        &id,
    )
    .await?
    .context("receipt after repeated drain")?;
    ensure!(serde_json::to_vec(&after)? == before && harness.calls.load(Ordering::SeqCst) == 1);
    Ok(())
}

#[tokio::test]
async fn dropping_an_admitted_request_before_pipeline_attachment_persists_no_send_receipt()
-> Result<()> {
    let harness = Harness::new(
        "chat_completions",
        StatusCode::OK,
        json_response("chat_completions", full_usage("chat_completions"))?,
    )
    .await?;
    let execution = super::super::admitted(&harness.fixture, &harness.headers).await?;
    let id = execution.id().clone();
    drop(execution);
    tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
    let receipt = harness.receipt(&Harness::client(false)?, &id).await?;
    ensure!(receipt.outcome == Outcome::Cancelled && receipt.attempts.is_empty());
    ensure!(receipt.usage.input_tokens.is_none() && harness.calls.load(Ordering::SeqCst) == 0);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn receipt_timeout_holds_transaction_and_late_commit_never_reopens_dispatch() -> Result<()> {
    let harness = Harness::new(
        "chat_completions",
        StatusCode::OK,
        json_response("chat_completions", full_usage("chat_completions"))?,
    )
    .await?;
    let gate = harness.fixture.service.cleanup.hold(Phase::Committing);
    let request = harness
        .send(&Harness::client(false)?)
        .timeout(Duration::from_secs(120));
    let caller = AbortOnDropHandle::new(tokio::spawn(async move {
        let response = request.send().await?;
        let status = response.status();
        let id = execution_id(&response)?;
        let _ = response.bytes().await?;
        Ok::<_, anyhow::Error>((status, id))
    }));
    let entered = tokio::time::timeout(TIMEOUT, gate.entered.notified()).await;
    let held: Result<()> = async {
        entered?;
        ensure!(harness.fixture.service.cleanup.disposition() == Disposition::Pending);
        // Advance only after the actual provider has completed and SQL is owned.
        // Both independent receipt wait and actor watchdog expire; neither may
        // drop a started transaction or manufacture a clean lease release.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(31)).await;
        tokio::time::resume();
        tokio::task::yield_now().await;
        ensure!(harness.fixture.service.cleanup.disposition() == Disposition::Pending);
        ensure!(
            tokio::time::timeout(
                Duration::from_millis(20),
                harness.fixture.service.admission.wait_drained()
            )
            .await
            .is_err()
        );
        Ok(())
    }
    .await;
    harness.fixture.service.cleanup.release_gate();
    held?;
    let (status, id) = tokio::time::timeout(TIMEOUT, caller).await???;
    ensure!(status == StatusCode::SERVICE_UNAVAILABLE);
    let receipt = tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Some(value) = store::lookup(
                &harness.fixture.db,
                &harness.fixture.installation,
                &harness.fixture.tenant,
                &id,
            )
            .await?
            {
                break Ok::<_, anyhow::Error>(value);
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    ensure!(receipt.attempts.len() == 1 && receipt.attempts[0].outcome == Outcome::Completed);
    ensure!(receipt.usage.input_tokens == Some(11) && receipt.usage.output_tokens == Some(7));
    ensure!(
        !harness.fixture.service.admission.receipts_ready(),
        "late acknowledgement silently re-enabled dispatch"
    );
    ensure!(harness.calls.load(Ordering::SeqCst) == 1);
    let committed = serde_json::to_vec(&receipt)?;
    tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await??;
    harness.fixture.service.cleanup.ensure_clean()?;
    let after_shutdown = store::lookup(
        &harness.fixture.db,
        &harness.fixture.installation,
        &harness.fixture.tenant,
        &id,
    )
    .await?
    .context("late committed receipt after shutdown")?;
    ensure!(serde_json::to_vec(&after_shutdown)? == committed);
    ensure!(harness.calls.load(Ordering::SeqCst) == 1);
    Ok(())
}
