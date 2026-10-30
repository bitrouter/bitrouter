//! Additional actual public-facade consumers. No smaller limits or SQL-made labels.
use super::*;
use crate::tenancy::transaction::{Disposition, Phase};
mod faults;

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn two_public_editors_compare_the_same_head_and_only_one_commits() -> Result<()> {
    let mut host = Host::new(false, usize::MAX).await?;
    let result = async {
        let native = host.open().await?;
        let current = host.current()?;
        current
            .prompt(&native, "two independent editor views")
            .await?;
        current.checkpoint(&native).await?;
        let left = current.review_checkpoint(&native, None).await?;
        let right = current.review_checkpoint(&native, None).await?;
        ensure!(left.scoring.expected_revision == right.scoring.expected_revision);
        let a = submission(&left, "editor-a");
        let b = submission(&right, "editor-b");
        let (first, second) = tokio::join!(
            current.submit_assessment(&native, a.clone()),
            current.submit_assessment(&native, b.clone()),
        );
        let (winner, losing, losing_input) = match (first, second) {
            (Ok(value), Err(error)) => (value, error, b),
            (Err(error), Ok(value)) => (value, error, a),
            other => anyhow::bail!("expected exactly one committed editor: {other:?}"),
        };
        // The shared Port admits one Human operation without queuing waiters.
        // A simultaneous editor is rejected before reaching the head CAS.
        rejected(&losing, ErrorCode::CapacityExceeded)?;
        // Once the slot is free, the unchanged losing submission must reach
        // the CAS and reject its stale head without inserting an orphan row.
        rejected(
            &current
                .submit_assessment(&native, losing_input.clone())
                .await
                .err()
                .context("stale editor unexpectedly committed")?,
            ErrorCode::RevisionConflict,
        )?;
        let view = current.review_checkpoint(&native, None).await?;
        ensure!(
            view.history.len() == 1
                && view.scoring.expected_revision.as_deref()
                    == Some(winner.revision.revision_id.as_str())
        );
        let mut corrected = losing_input;
        corrected.expected_revision = view.scoring.expected_revision;
        let next = current.submit_assessment(&native, corrected).await?;
        ensure!(next.revision.supersedes.as_deref() == Some(winner.revision.revision_id.as_str()));
        ensure!(count(&host.ready.fixture, "acp_assessment_revisions").await? == 2);
        current
            .prompt(&native, "controller continues after stale editor")
            .await?;
        Ok(())
    }
    .await;
    host.finish(result).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn cancelled_human_predecessor_keeps_its_original_deadline_on_the_next_acp_callback()
-> Result<()> {
    let mut host = Host::new(false, usize::MAX).await?;
    let result = async {
        let native = host.open().await?;
        let current = host.current()?;
        current.prompt(&native, "deadline predecessor").await?;
        current.checkpoint(&native).await?;
        drained(&host.ready).await?;
        let input = submission(
            &current.review_checkpoint(&native, None).await?,
            "original-deadline",
        );
        let service = &host.ready.fixture.service;
        let execution = service
            .admit_execution(
                service
                    .submission(&host.ready.headers()?)
                    .map_err(failure)?,
                host.ready.subject.resource_id.as_str(),
                &ApiProtocol::ChatCompletions,
            )
            .await
            .map_err(failure)?;
        // Genuine admitted no-send receipt: its 60+30s custody can cover a5s
        // Human queue wait. Do not hold a short management transaction instead.
        let gate = service.cleanup.hold(Phase::Committing);
        drop(execution);
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        let reads = service.reads.available_permits();
        let mut edit = Box::pin(current.submit_assessment(&native, input));
        ensure!(futures::poll!(&mut edit).is_pending());
        ensure!(service.reads.available_permits() + 1 == reads);
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(4)).await;
        tokio::time::resume();
        drop(edit);
        // This actual ACP callback must adopt the existing5s deadline. Giving
        // the new waiter another5s would commit the expired Human row below.
        let mut callback = Box::pin(current.prompt(&native, "ACP settles old editor first"));
        ensure!(futures::poll!(&mut callback).is_pending());
        tokio::time::pause();
        tokio::time::advance(Duration::from_millis(1100)).await;
        tokio::time::resume();
        ensure!(service.cleanup.disposition() == Disposition::Pending);
        service.cleanup.release_gate();
        tokio::time::timeout(TIMEOUT, &mut callback).await??;
        drained(&host.ready).await?;
        ensure!(
            count(&host.ready.fixture, "acp_assessment_revisions").await? == 0,
            "a new callback renewed the old Human job"
        );
        ensure!(service.current().is_ok() && service.reads.available_permits() == reads);
        Ok(())
    }
    .await;
    host.finish(result).await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn queued_manual_edit_rechecks_current_revoke_suspend_and_tenant_scope() -> Result<()> {
    for change in ["revoke", "suspend", "scope"] {
        let mut host = Host::new(false, usize::MAX).await?;
        let result = async {
            let native = host.open().await?;
            let current = host.current()?;
            current
                .prompt(&native, "queued editor current authority")
                .await?;
            current.checkpoint(&native).await?;
            drained(&host.ready).await?;
            let input = submission(
                &current.review_checkpoint(&native, None).await?,
                "queued-current",
            );
            let service = &host.ready.fixture.service;
            let execution = service
                .admit_execution(
                    service
                        .submission(&host.ready.headers()?)
                        .map_err(failure)?,
                    host.ready.subject.resource_id.as_str(),
                    &ApiProtocol::ChatCompletions,
                )
                .await
                .map_err(failure)?;
            let gate = service.cleanup.hold(Phase::Committing);
            drop(execution);
            tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
            let commands = service.commands.capacity();
            let safety = service.safety.capacity();
            // Installation revokes and suspension use the reserved safety
            // queue; a scope update remains in the ordinary FIFO. Both must
            // be admitted before the Human operation while the actor is held.
            let safety_mutation = usize::from(matches!(change, "revoke" | "suspend"));
            let ordinary_mutation = 1 - safety_mutation;
            let mut mutation = Box::pin(change_authority(
                &host.ready.fixture,
                &host.ready.credential,
                &host.ready.subject,
                change,
            ));
            ensure!(futures::poll!(&mut mutation).is_pending());
            ensure!(
                service.commands.capacity() + ordinary_mutation == commands
                    && service.safety.capacity() + safety_mutation == safety,
                "{change}: real mutation did not enqueue in its designated queue"
            );
            let mut edit = Box::pin(current.submit_assessment(&native, input));
            ensure!(futures::poll!(&mut edit).is_pending());
            ensure!(
                service.commands.capacity() + ordinary_mutation + 1 == commands
                    && service.safety.capacity() + safety_mutation == safety,
                "{change}: manual operation did not enqueue after mutation"
            );
            service.cleanup.release_gate();
            tokio::time::timeout(TIMEOUT, &mut mutation).await??;
            ensure!(tokio::time::timeout(TIMEOUT, &mut edit).await?.is_err());
            drained(&host.ready).await?;
            ensure!(count(&host.ready.fixture, "acp_assessment_revisions").await? == 0);
            ensure!(
                service.current().is_ok()
                    && current.review_checkpoint(&native, None).await.is_err()
            );
            Ok(())
        }
        .await;
        host.finish(result).await?;
    }
    Ok(())
}

#[tokio::test]
async fn checked_capture_full_history_refuses_paid_acceptance_before_counter_or_provider()
-> Result<()> {
    use crate::tenancy::capture::{
        human::{Input, Reply as HumanReply},
        queue::{Reply, Request},
    };
    let provider = Provider::start(false).await?;
    let judge = Judge::new(false).await?;
    let ready = Ready::new(&provider.url, &judge.url, true, LearningMode::Observe, 2).await?;
    let (binding, port) = ready.port_pair().await?;
    let result = async {
        // Actual checked association/recorded events and actor Freeze. There is
        // no successful public checkpoint here: that facade also invokes paid
        // evaluation, so using a failed facade call as a live controller would
        // be false. All labels below still use the real Human actor consumer.
        let outcome = binding
            .enqueue(Request::Freeze(native().into()))
            .await
            .map_err(failure)?
            .await?;
        ensure!(outcome.disposition == Disposition::Committed);
        let summary = match outcome.result.map_err(failure)? {
            Reply::Frozen(Some(summary)) => summary,
            _ => anyhow::bail!("actual checked checkpoint absent"),
        };
        let mut view = match port.human(native(), Input::Review(None)).await? {
            HumanReply::Review(view) => view,
            _ => anyhow::bail!("actual Human review absent"),
        };
        for index in 0..128 {
            let input = submission(&view, &format!("before-paid-{index:03}"));
            match port.human(native(), Input::Submit(input)).await? {
                HumanReply::Submitted(_) => {}
                _ => anyhow::bail!("actual Human submit reply"),
            }
            view = match port.human(native(), Input::Review(None)).await? {
                HumanReply::Review(view) => view,
                _ => anyhow::bail!("actual Human review reply"),
            };
        }
        ensure!(judge.calls.load(Ordering::SeqCst) == 0);
        let before = bodies(&ready.fixture, "owned_judge_budget").await?;
        ensure!(before.is_empty());
        ensure!(
            crate::tenancy::learning::checkpoint(binding.clone(), native(), &summary)
                .await
                .is_err()
        );
        drained(&ready).await?;
        ensure!(
            bodies(&ready.fixture, "owned_judge_budget").await? == before
                && bodies(&ready.fixture, "owned_judge_job").await?.is_empty()
                && bodies(&ready.fixture, "owned_judge_pending")
                    .await?
                    .is_empty()
                && judge.calls.load(Ordering::SeqCst) == 0
                && provider.calls() == 0
        );
        let after = match port.human(native(), Input::Review(None)).await? {
            HumanReply::Review(view) => view,
            _ => anyhow::bail!("post-rejection Human review absent"),
        };
        ensure!(
            after.history.len() == 128
                && after.scoring.expected_revision == view.scoring.expected_revision
                && ready.fixture.service.current().is_ok()
        );
        Ok(())
    }
    .await;
    cleanup(&ready, &[port], judge, provider, result).await
}

fn exact_input(
    mut input: RubricSubmission,
    bytes: usize,
    principal: &str,
) -> Result<RubricSubmission> {
    // Normalized labels match the real stable author so original and normalized
    // encodings hit exactly the same unchanged512KiB boundary.
    input.evaluator_id = format!("owned-human/{principal}");
    input.evaluator_version = "owned-manual-rubric-v1".into();
    let base = serde_json::to_vec(&input)?.len();
    let mut remaining = bytes.checked_sub(base).context("input target too small")?;
    for field in input
        .evaluation
        .items
        .iter_mut()
        .flat_map(|item| [&mut item.explanation, &mut item.selection_reason])
    {
        let add = remaining.min(
            (64 * 1024usize)
                .checked_sub(field.len())
                .context("field already too large")?,
        );
        field.extend(std::iter::repeat_n('x', add));
        remaining -= add;
    }
    ensure!(
        remaining == 0 && serde_json::to_vec(&input)?.len() == bytes,
        "fixture could not reach exact input size"
    );
    Ok(input)
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn public_input_exact_512k_and_one_over_are_distinguished_before_sql() -> Result<()> {
    let mut host = Host::new(false, usize::MAX).await?;
    let result = async {
        let native = host.open().await?;
        let current = host.current()?;
        current.prompt(&native, "exact byte boundary").await?;
        current.checkpoint(&native).await?;
        drained(&host.ready).await?;
        let view = current.review_checkpoint(&native, None).await?;
        let exact = exact_input(
            submission(&view, "exact-byte-input"),
            512 * 1024,
            host.ready.fixture.principal.as_str(),
        )?;
        let over = exact_input(
            submission(&view, "exact-byte-input"),
            512 * 1024 + 1,
            host.ready.fixture.principal.as_str(),
        )?;
        let before = host.ready.fixture.service.cleanup.sql_counts_for_test();
        rejected(
            &current
                .submit_assessment(&native, over)
                .await
                .err()
                .context("one-over input accepted")?,
            ErrorCode::CapacityExceeded,
        )?;
        ensure!(host.ready.fixture.service.cleanup.sql_counts_for_test() == before);
        let accepted = current.submit_assessment(&native, exact.clone()).await?;
        ensure!(accepted.revision.selected_on_submission);
        ensure!(
            serde_json::to_value(current.submit_assessment(&native, exact).await?)?
                == serde_json::to_value(accepted)?
        );
        ensure!(
            current
                .review_checkpoint(&native, None)
                .await?
                .history
                .len()
                == 1
                && host.ready.fixture.service.current().is_ok()
        );
        Ok(())
    }
    .await;
    host.finish(result).await
}
