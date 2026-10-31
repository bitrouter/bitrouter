//! A real paid child shares its parent's total deadline and owes its own ACK.
use super::*;
use bitrouter_ai::tenancy::execution::ReceiptState;

#[tokio::test]
async fn choice_crossing_parent_total_keeps_both_receipts_and_error_outcome_ack() -> Result<()> {
    let generation = Provider::start(false).await?;
    let mut choice = wire::Endpoint::start(true, false).await?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let mut bound = fixture::sqlite(&generation, &choice, directory.path(), 2).await?;
    bound
        .fixture
        .service
        .execution_settings
        .execution_timeout_secs = Some(90);
    let checked = async {
        let mut session = fixture::open(
            &bound,
            &generation,
            &workspace,
            Arc::new(HostSpies::default()),
        )
        .await?;
        let core = session.core().clone();
        let running = AbortOnDropHandle::new(tokio::spawn(async move {
            let result = session
                .run(
                    fixture::task(),
                    Arc::new(Approval),
                    CancellationToken::new(),
                )
                .await;
            (session, result)
        }));
        choice.receive().await?;
        let saved = fixture::journal(&bound).await?;
        let (id, link) = saved["owned_choices"]
            .as_object()
            .context("Choice intent")?
            .iter()
            .next()
            .context("Choice link")?;
        let parent_id = link["parent_execution"]
            .as_str()
            .context("parent execution")?;
        let child_id = link["child_execution"]
            .as_str()
            .context("child execution")?;
        let (parent, _) = bound
            .fixture
            .service
            .pipelines
            .callback_owner_for_test(&parent_id.try_into()?)?;
        let (child, _) = bound
            .fixture
            .service
            .pipelines
            .callback_owner_for_test(&child_id.try_into()?)?;
        ensure!(
            parent.deadline == child.deadline,
            "child renewed the parent's total deadline"
        );
        ensure!(parent.deadline.instant().is_some());
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(75)).await;
        tokio::time::resume();
        ensure!(
            !running.is_finished()
                && !parent.cancelled.is_cancelled()
                && !child.cancelled.is_cancelled()
        );
        ensure!(
            core.snapshot().await.context_store.decisions[id]
                .outcome
                .is_none()
        );
        let remaining = parent
            .deadline
            .instant()
            .context("configured parent deadline")?
            .saturating_duration_since(tokio::time::Instant::now());
        tokio::time::pause();
        tokio::time::advance(remaining + Duration::from_secs(1)).await;
        tokio::time::resume();
        // Do not release the provider. Expiry itself must cancel the paid call,
        // settle its immutable receipt, and persist Core's distinct error ACK.
        let (session, result) = tokio::time::timeout(TIMEOUT, running).await??;
        ensure!(result.as_ref().map_or(true, |state| {
            state
                .run
                .as_ref()
                .is_none_or(|run| run.status != RunStatus::Completed)
        }));
        tokio::time::timeout(TIMEOUT, bound.fixture.service.admission.wait_drained()).await?;
        ensure!(parent.cancelled.is_cancelled() && child.cancelled.is_cancelled());
        let child_receipt = receipt(&bound, child_id).await?;
        let parent_receipt = receipt(&bound, parent_id).await?;
        ensure!(
            child_receipt.state == ReceiptState::Unknown
                && child_receipt.outcome != Outcome::Completed
                && child_receipt.attempts.len() == 1
                && child_receipt.attempts[0].outcome == Outcome::Unknown
        );
        ensure!(parent_receipt.outcome != Outcome::Completed && parent_receipt.attempts.is_empty());
        let snapshot = core.snapshot().await;
        let outcome = snapshot.context_store.decisions[id]
            .outcome
            .as_ref()
            .context("Core outcome ACK")?;
        ensure!(outcome.as_ref().is_err_and(|error| error.may_have_run));
        let after = fixture::journal(&bound).await?;
        ensure!(
            after["owned_choices"][id]["outcome_digest"]
                .as_str()
                .is_some_and(|digest| digest.len() == 64)
        );
        let receipts = page(&bound).await?;
        ensure!(receipts.execution_ids.len() == 2 && receipts.multiple_executions);
        ensure!(
            receipts
                .execution_ids
                .contains(&parent_receipt.execution_id)
                && receipts.execution_ids.contains(&child_receipt.execution_id)
        );
        let parent_evidence = evidence(&bound, parent_id).await?;
        let child_evidence = evidence(&bound, child_id).await?;
        ensure!(parent_evidence.parent_execution_id.is_none());
        ensure!(parent_evidence.child_execution_ids == vec![child_receipt.execution_id.clone()]);
        ensure!(child_evidence.parent_execution_id.as_ref() == Some(&parent_receipt.execution_id));
        ensure!(child_evidence.child_execution_ids.is_empty());
        ensure!(choice.calls() == 1 && generation.calls() == 0);
        session.close().await?;
        ensure!(bound.fixture.service.host_slots_for_test() == 1);
        ensure!(fixture::journal(&bound).await?["released"] == true);
        Ok(())
    }
    .await;
    let closed = choice.finish().await;
    finish(&bound.fixture, generation, checked).await?;
    closed
}

async fn evidence(
    bound: &Bound,
    id: &str,
) -> Result<bitrouter_ai::tenancy::usage_evidence::ExecutionUsageEvidenceV1> {
    let bytes = bound
        .fixture
        .service
        .read_receipt(
            bound.fixture.operator.clone(),
            bound.fixture.tenant.clone(),
            read::Request::UsageEvidence(id.try_into()?),
        )
        .await
        .map_err(failure)?;
    Ok(serde_json::from_slice(&bytes)?)
}
