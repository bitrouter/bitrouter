use super::*;
use crate::tenancy::transaction::Phase;

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn cancelled_manual_queue_wait_is_reconciled_by_the_next_real_acp_callback() -> Result<()> {
    let mut host = Host::new(false, usize::MAX).await?;
    let result = async {
        let native = host.open().await?;
        let current = host.current()?;
        current.prompt(&native, "before canceled editor").await?;
        current.checkpoint(&native).await?;
        drained(&host.ready).await?;
        let input = submission(&current.review_checkpoint(&native, None).await?, "canceled-before-send");
        let service = &host.ready.fixture.service;
        let before = service.cleanup.sql_counts_for_test();
        let reads = service.reads.available_permits();
        let mut held = Vec::new();
        // Own every actual bounded mailbox permit; do not invent an admitted
        // actor outcome or change its queue/operation limits.
        for _ in 0..service.commands.max_capacity() {
            held.push(service.commands.clone().try_reserve_owned()?);
        }
        let mut request = Box::pin(current.submit_assessment(&native, input.clone()));
        tokio::select! {
            biased;
            reply = &mut request => anyhow::bail!("manual request did not wait for the mailbox: {reply:?}"),
            () = tokio::task::yield_now() => {}
        }
        ensure!(service.reads.available_permits() + 1 == reads);
        drop(request);
        ensure!(service.reads.available_permits() == reads && service.cleanup.sql_counts_for_test() == before);
        drop(held);
        // No human call precedes this real new ACP request/response sequence.
        current.prompt(&native, "ordinary ACP goes first").await?;
        ensure!(count(&host.ready.fixture, "acp_assessment_revisions").await? == 0);
        let accepted = current.submit_assessment(&native, input).await?;
        ensure!(accepted.revision.selected_on_submission
            && count(&host.ready.fixture, "acp_assessment_revisions").await? == 1
            && service.current().is_ok());
        Ok(())
    }.await;
    host.finish(result).await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn cancelled_manual_commit_or_domain_rollback_hands_off_before_the_next_acp_operation()
-> Result<()> {
    for commit in [true, false] {
        let mut host = Host::new(false, usize::MAX).await?;
        let result = async {
            let native = host.open().await?;
            let current = host.current()?;
            current.prompt(&native, "handoff task").await?;
            current.checkpoint(&native).await?;
            drained(&host.ready).await?;
            let view = current.review_checkpoint(&native, None).await?;
            let input = submission(&view, "canceled-after-h");
            if !commit {
                current
                    .submit_assessment(&native, submission(&view, "real-winning-head"))
                    .await?;
            }
            let before = host.ready.fixture.service.cleanup.sql_counts_for_test();
            let gate = host.ready.fixture.service.cleanup.hold(if commit {
                Phase::Committing
            } else {
                Phase::RollingBack
            });
            let mut request = Box::pin(current.submit_assessment(&native, input.clone()));
            tokio::select! {
                reply = &mut request => anyhow::bail!("manual request missed H gate: {reply:?}"),
                result = tokio::time::timeout(TIMEOUT, gate.entered.notified()) => { result?; }
            }
            drop(request);
            host.ready.fixture.service.cleanup.release_gate();
            current
                .prompt(&native, "ACP must settle its predecessor first")
                .await?;
            ensure!(
                host.ready.fixture.service.current().is_ok()
                    && count(&host.ready.fixture, "acp_assessment_revisions").await? == 1
            );
            let row = host
                .ready
                .fixture
                .db
                .query_one(Statement::from_string(
                    DatabaseBackend::Sqlite,
                    "SELECT revision_json FROM acp_assessment_revisions".to_owned(),
                ))
                .await?
                .context("actual revision")?;
            let retained: Value =
                serde_json::from_str(&row.try_get::<String>("", "revision_json")?)?;
            let replay = current.submit_assessment(&native, input).await;
            if commit {
                ensure!(
                    serde_json::to_value(replay?.revision)? == retained,
                    "lost reply retry changed committed identity or timestamp"
                );
            } else {
                rejected(
                    &replay.err().context("stale canceled input became valid")?,
                    ErrorCode::RevisionConflict,
                )?;
                ensure!(host.ready.fixture.service.cleanup.sql_counts_for_test().1 > before.1);
            }
            ensure!(count(&host.ready.fixture, "acp_assessment_revisions").await? == 1);
            Ok(())
        }
        .await;
        host.finish(result).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn current_revoke_after_a_known_manual_commit_denies_new_public_actions() -> Result<()> {
    let mut host = Host::new(false, usize::MAX).await?;
    let result = async {
        let native = host.open().await?;
        let current = host.current()?;
        current.prompt(&native, "human authority task").await?;
        current.checkpoint(&native).await?;
        drained(&host.ready).await?;
        let input = submission(
            &current.review_checkpoint(&native, None).await?,
            "known-before-revoke",
        );
        current.submit_assessment(&native, input.clone()).await?;
        change_authority(
            &host.ready.fixture,
            &host.ready.credential,
            &host.ready.subject,
            "revoke",
        )
        .await?;
        ensure!(
            current.review_checkpoint(&native, None).await.is_err()
                && current.submit_assessment(&native, input).await.is_err()
        );
        ensure!(count(&host.ready.fixture, "acp_assessment_revisions").await? == 1);
        Ok(())
    }
    .await;
    host.finish(result).await
}
