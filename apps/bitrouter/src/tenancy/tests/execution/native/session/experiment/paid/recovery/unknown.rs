//! A real concurrent Human H failure cannot discard the held paid producer.
use super::*;
use crate::tenancy::{learning::source::Binding, transaction::Phase};
use bitrouter_ai::{
    core::protocol::CommitStatus,
    tenancy::{
        execution::{Outcome, Provenance},
        identity::ExecutionId,
        operation::DispatchState,
    },
};
use futures::FutureExt;

fn leased(root: &Path) -> Result<()> {
    let denied = crate::tenancy::ownership::acquire_path(&root.join("native-paid.db"), false)
        .err()
        .context("original native owner released its file lease")?;
    ensure!(matches!(
        denied.downcast_ref::<std::fs::TryLockError>(),
        Some(std::fs::TryLockError::WouldBlock)
    ));
    Ok(())
}

fn usage(facts: &crate::tenancy::receipts::journal::Frozen) -> Result<()> {
    ensure!(facts.dispatch == DispatchState::Started && facts.attempts.len() == 1);
    let attempt = &facts.attempts[0];
    ensure!(attempt.outcome == Outcome::Completed && facts.http_statuses == [Some(200)]);
    ensure!(
        attempt.usage == facts.usage
            && facts.usage.input_tokens == Some(4)
            && facts.usage.output_tokens == Some(2)
            && facts.usage.cache_read_tokens == Some(0)
            && facts.usage.cache_write_tokens == Some(0)
            && facts.usage.reasoning_tokens == Some(0)
            && facts.usage.complete
            && facts.usage.provenance == Provenance::Provider
    );
    Ok(())
}

async fn committed_human(fixture: &Fixture, checkpoint: &str) -> Result<()> {
    // Read only the real submitted Human row, after the original consuming
    // COMMIT resumes. Neither this observation nor the late SQL ACK clears H.
    let rows = fixture
        .db
        .query_all(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT revision_json FROM acp_assessment_revisions WHERE installation_id=? \
         AND tenant_id=? AND creator_principal_id=? LIMIT 2",
            [
                fixture.installation.as_str().into(),
                fixture.tenant.as_str().into(),
                fixture.principal.as_str().into(),
            ],
        ))
        .await?;
    ensure!(
        rows.len() == 1,
        "late original Human COMMIT did not publish exactly one revision"
    );
    let body: String = rows[0].try_get("", "revision_json")?;
    ensure!(body.len() <= 1024 * 1024);
    let row: crate::acp_trajectory::checkpoint::types::AssessmentRevision =
        serde_json::from_str(&body)?;
    ensure!(
        row.input.submission_id == "human-unknown-with-paid"
            && row.input.checkpoint_id == checkpoint
            && row.input.source == AssessmentSource::Human
    );
    Ok(())
}

#[tokio::test]
#[ignore = "internal held paid/H producer; select bounded process parent"]
async fn producer() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("native paid root")?);
    let mode = std::env::var(MODE)?;
    ensure!(matches!(mode.as_str(), "late-commit" | "commit-unwind"));
    let subject = std::env::var(SUBJECT)?;
    let (fixture, headers) = ready(&root, &subject, &std::env::var(JUDGE)?).await?;
    let mut current = open(&fixture, &headers, &subject, &root, "paid-human-unknown").await?;
    let checked: Result<()> = async {
        current.run(human::observed_task(), Arc::new(Approval), CancellationToken::new()).await?;
        let review = current.review_run().await?;
        let body = serde_json::to_vec(&human::label(&review, "actual-child-source", true)?.evaluation)?;
        ensure!(body.len() <= 64 * 1024);
        std::fs::write(root.join("answer.json"), body)?;
        signal("native-paid-source-ready")?;
        advance().await?;
        let mut paid = Box::pin(current.assess_run(&review.scoring.evidence.checkpoint_id, None));
        let accepted = tokio::select! {
            result = &mut paid => { result?; anyhow::bail!("paid reply preceded actual acceptance"); }
            job = accepted(&fixture.db) => job?,
        };
        signal("native-paid-accepted")?;
        // The parent acknowledges only after the actual judge socket receives
        // this request. Its response remains held throughout the Human failure.
        advance().await?;
        let execution: ExecutionId = accepted["execution"].as_str().context("paid execution")?.try_into()?;
        let (receipt, tasks) = fixture.service.pipelines.callback_owner_for_test(&execution)?;
        let learning = receipt.stopped_learning().map_err(|code| anyhow::anyhow!("learning attachment: {code:?}"))?
            .context("actual accepted learning owner")?;
        let Binding::Native(binding) = &learning.binding else { anyhow::bail!("native paid binding changed"); };
        let deadline = receipt.deadline;
        let cleanup_deadline = receipt.cleanup_deadline();
        ensure!(receipt.id() == &execution && learning.execution == execution && learning.deadline == deadline
            && binding.has_paid_source().map_err(|code| anyhow::anyhow!("source pin: {code:?}"))?);
        ensure!(learning.stopped().now_or_never().is_none() && !receipt.durable());
        let reads = fixture.service.reads.available_permits();
        let gate = fixture.service.cleanup.hold(Phase::Committing);
        let mut human = Box::pin(current.submit_assessment(human::label(&review, "human-unknown-with-paid", false)?));
        tokio::select! {
            result = &mut human => { result?; anyhow::bail!("Human reply preceded its consuming COMMIT gate"); }
            result = tokio::time::timeout(TIMEOUT, gate.entered.notified()) => { result?; }
        }
        ensure!(fixture.service.cleanup.disposition() == Disposition::Pending
            && fixture.service.reads.available_permits().checked_add(1) == Some(reads));
        // Use the real public operation deadline, not an injected clock. The H
        // actor still owns its original transaction and command/read permits.
        let failure = tokio::time::timeout(TIMEOUT, &mut human).await?.err().context("Human unexpectedly acknowledged COMMIT")?;
        ensure!(failure.commit_status == CommitStatus::Unknown);
        tokio::time::timeout(TIMEOUT, async {
            while fixture.service.host_cleanup_unknown_site_for_test().is_none() {
                tokio::task::yield_now().await;
            }
        }).await?;
        ensure!(fixture.service.cleanup.disposition() == Disposition::Ambiguous
            && fixture.service.current().is_err() && fixture.service.host_slots_for_test() == 0);
        // This is the fail-before-fix witness: dropping serve's local paid
        // future runs Runtime::drop and makes stopped() ready with no output,
        // even though the real judge has not returned a single response byte.
        ensure!(learning.stopped().now_or_never().is_none(), "Human Unknown discarded the live paid runtime");
        ensure!(paid.as_mut().now_or_never().is_none(), "paid public reply lost its original producer");
        ensure!(binding.has_paid_source().map_err(|code| anyhow::anyhow!("source pin: {code:?}"))?);
        ensure!(fixture.service.reads.available_permits().checked_add(1) == Some(reads));
        let blocked = current.review_run().await.err().context("Human command permit released before H settled")?;
        ensure!(blocked.code == bitrouter_ai::core::protocol::ErrorCode::Busy
            && blocked.commit_status == CommitStatus::NotCommitted);
        leased(&root)?;
        signal("native-paid-human-unknown")?;
        advance().await?;
        // The same live provider now finishes. These are original journal facts,
        // not a successful receipt/SQL acknowledgement under quarantined H.
        tokio::time::timeout(TIMEOUT, async {
            loop {
                match receipt.journal.fallback_evidence() {
                    Err(crate::tenancy::receipts::journal::Error::Busy) => tokio::task::yield_now().await,
                    Ok(Some(facts)) => {
                        ensure!(facts.outcome == Outcome::Completed && facts.http_status == Some(200));
                        break Ok::<_, anyhow::Error>(());
                    }
                    Ok(None) => anyhow::bail!("actual judge attempt disappeared"),
                    Err(error) => return Err(error.into()),
                }
            }
        }).await??;
        // Freeze only after the non-mutating witness proves every original
        // transport permit has finished. The test never terminates a producer.
        let facts = tokio::time::timeout(TIMEOUT, receipt.journal.settled()).await??;
        ensure!(facts.belongs_to_identity(&fixture.installation, &execution));
        usage(&facts)?;
        ensure!(learning.stopped().now_or_never().is_none() && paid.as_mut().now_or_never().is_none());
        if mode == "commit-unwind" { fixture.service.cleanup.unwind_gate()?; }
        else { fixture.service.cleanup.release_gate(); }
        let failure = tokio::time::timeout(TIMEOUT, &mut paid).await?.err().context("quarantined paid work claimed success")?;
        ensure!(failure.commit_status == CommitStatus::Unknown
            && failure.message == "native paid assessment unavailable",
            "reply must come from the retained native paid producer, not a dropped reply channel");
        ensure!(tokio::time::timeout(TIMEOUT, learning.stopped()).await?
            .map_err(|code| anyhow::anyhow!("actual runtime completion: {code:?}"))?);
        let after = tokio::time::timeout(TIMEOUT, receipt.journal.settled()).await??;
        usage(&after)?;
        ensure!(after.attempts[0].attempt_id == facts.attempts[0].attempt_id
            && after.usage == facts.usage && receipt.deadline == deadline && receipt.cleanup_deadline() == cleanup_deadline);
        ensure!(!receipt.durable() && fixture.service.cleanup.ensure_clean().is_err()
            && fixture.service.cleanup.disposition() == Disposition::Ambiguous
            && fixture.service.current().is_err() && fixture.service.host_slots_for_test() == 0
            && fixture.service.completion.clone().now_or_never().is_none());
        ensure!(fixture.service.reads.available_permits().checked_add(1) == Some(reads)
            && binding.has_paid_source().map_err(|code| anyhow::anyhow!("source pin: {code:?}"))?);
        let (retained, retained_tasks) = fixture.service.pipelines.callback_owner_for_test(&execution)?;
        ensure!(Arc::ptr_eq(&receipt, &retained) && Arc::ptr_eq(&tasks, &retained_tasks));
        if mode == "late-commit" { committed_human(&fixture, &review.scoring.evidence.checkpoint_id).await?; }
        // A missing receipt is NOT cleanup evidence. No stopped/native_cleanup
        // proof can be manufactured while the original H/file owner is unknown.
        absent(&fixture.db, execution.as_str()).await?;
        let jobs = rows(&fixture.db, "owned_judge_active_v3").await?;
        ensure!(jobs.len() == 1 && jobs[0] == accepted
            && jobs[0]["recovery"]["stopped"].is_null() && jobs[0]["recovery"]["native_cleanup"].is_null());
        let budgets = rows(&fixture.db, "owned_judge_budget").await?;
        ensure!(budgets.len() == 1 && budgets[0]["accepted"] == 1);
        let blocked = current.review_run().await.err().context("quarantined H released its original command permit")?;
        ensure!(blocked.code == bitrouter_ai::core::protocol::ErrorCode::Busy
            && blocked.commit_status == CommitStatus::NotCommitted);
        leased(&root)?;
        signal("native-paid-unknown-retained")?;
        advance().await?;
        anyhow::bail!("bounded parent must reap the actual quarantined owner")
    }.await;
    fixture.service.cleanup.release_gate();
    checked
}

async fn parent(mode: &str) -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(120),
        Box::pin(async {
            let subject = upstream().await?;
            let mut judge = Judge::start().await?;
            let root = tempfile::tempdir()?;
            let subject_url = format!("{}/v1", subject.uri());
            let mut child = Child::start(
                "unknown::producer",
                root.path(),
                mode,
                Some((&subject_url, &judge.url)),
            )?;
            let checked = async {
                child.line("native-paid-source-ready").await?;
                let answer = std::fs::read(root.path().join("answer.json"))?;
                ensure!(answer.len() <= 64 * 1024);
                judge.retained_answer(serde_json::from_slice(&answer)?)?;
                child.advance().await?;
                child.line("native-paid-accepted").await?;
                ensure!(judge.received().await?["model"] == "judge-model");
                child.advance().await?;
                child.line("native-paid-human-unknown").await?;
                ensure!(!judge.extra_request());
                judge.release.cancel();
                child.advance().await?;
                child.line("native-paid-unknown-retained").await?;
                ensure!(
                    !judge.extra_request()
                        && subject
                            .received_requests()
                            .await
                            .context("subject sends")?
                            .len()
                            == 1
                );
                Ok(())
            }
            .await;
            judge.release.cancel();
            let reaped = child.finish(true, checked).await;
            let checked = async {
                reaped?;
                // Only an actual process wait/reap is allowed to release this lease.
                drop(crate::tenancy::ownership::acquire_path(
                    &root.path().join("native-paid.db"),
                    false,
                )?);
                ensure!(!judge.extra_request());
                Ok::<_, anyhow::Error>(())
            }
            .await;
            checked.and(judge.finish().await)
        }),
    )
    .await?
}

#[tokio::test]
async fn human_unknown_late_commit_retains_paid_future_and_original_receipt_until_reap()
-> Result<()> {
    parent("late-commit").await
}

#[tokio::test]
async fn human_commit_unwind_retains_paid_future_and_original_receipt_until_reap() -> Result<()> {
    parent("commit-unwind").await
}
