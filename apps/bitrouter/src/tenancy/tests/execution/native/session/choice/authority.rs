use super::*;

#[tokio::test]
async fn owned_choice_real_credential_wait_rechecks_authority_and_still_acks_cleanup() -> Result<()>
{
    for change in ["revoke", "suspend", "scope"] {
        let generation = Provider::start(false).await?;
        let mut choice = wire::Endpoint::start(false, false).await?;
        let directory = tempfile::tempdir()?;
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let bound = fixture::sqlite(&generation, &choice, directory.path(), 2).await?;
        let mut session = fixture::open(
            &bound,
            &generation,
            &workspace,
            Arc::new(HostSpies::default()),
        )
        .await?;
        let core = session.core().clone();
        let gate = bound
            .fixture
            .service
            .hold_receipt_credential_read_for_test()?;
        let release = gate.release.clone().drop_guard();
        let checked = tokio::time::timeout(Duration::from_secs(25), async {
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
            tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
            let child = gate
                .execution
                .get()
                .context("actual child credential read")?
                .clone();
            let saved = fixture::journal(&bound).await?;
            let links = saved["owned_choices"]
                .as_object()
                .context("durable Choice intent")?;
            ensure!(links.len() == 1);
            let (decision_id, link) = links.iter().next().context("owned Choice link")?;
            ensure!(link["child_execution"] == child.as_str() && link["outcome_digest"].is_null());
            let parent = link["parent_execution"]
                .as_str()
                .context("parent execution")?
                .to_owned();
            ensure!(
                parent != child.as_str()
                    && choice.calls() == 0
                    && generation.calls() == 0
                    && !running.is_finished()
            );
            let snapshot = core.snapshot().await;
            ensure!(
                snapshot
                    .context_store
                    .decisions
                    .get(decision_id)
                    .is_some_and(|entry| entry.outcome.is_none())
            );
            change_authority(&bound.fixture, &bound.key, &bound.route, change).await?;
            gate.release.cancel();
            let (session, outcome) = tokio::time::timeout(TIMEOUT, running).await??;
            if let Err(error) = outcome {
                ensure!(matches!(
                    error.code,
                    bitrouter_ai::core::protocol::ErrorCode::UnauthorizedScope
                        | bitrouter_ai::core::protocol::ErrorCode::CheckpointUnavailable
                ));
            }
            session.close().await?;
            let snapshot = core.snapshot().await;
            let failure = snapshot
                .context_store
                .decisions
                .get(decision_id)
                .context("retained decision")?
                .outcome
                .as_ref()
                .context("cleanup outcome ACK")?
                .as_ref()
                .err()
                .context("revoked Choice succeeded")?;
            ensure!(!failure.may_have_run && failure.usage.is_none());
            let saved = fixture::journal(&bound).await?;
            ensure!(saved["owned_choices"][decision_id]["child_execution"] == child.as_str());
            ensure!(
                saved["owned_choices"][decision_id]["outcome_digest"].is_string()
                    && saved["released"] == true
            );
            tokio::time::timeout(TIMEOUT, bound.fixture.service.admission.wait_drained()).await?;
            let child_receipt = receipt(&bound, child.as_str()).await?;
            let parent_receipt = receipt(&bound, &parent).await?;
            ensure!(child_receipt.attempts.is_empty() && parent_receipt.attempts.is_empty());
            let ids = page(&bound).await?;
            ensure!(
                ids.execution_ids.len() == 2
                    && ids.multiple_executions
                    && ids.execution_ids.contains(&child_receipt.execution_id)
                    && ids.execution_ids.contains(&parent_receipt.execution_id)
            );
            ensure!(
                choice.calls() == 0
                    && generation.calls() == 0
                    && bound.fixture.service.host_slots_for_test() == 1
            );
            ensure!(bound.fixture.service.current().is_ok());
            Ok(())
        })
        .await
        .context("owned Choice credential boundary deadline")
        .and_then(|result| result);
        drop(release);
        let choice_closed = choice.finish().await;
        finish(&bound.fixture, generation, checked).await?;
        choice_closed?;
    }
    Ok(())
}

#[tokio::test]
async fn owned_choice_final_send_rejection_keeps_known_zero_after_claim() -> Result<()> {
    use crate::storage::checkpoint::snapshot::context::inspection::InspectionExt;
    let generation = Provider::start(false).await?;
    let mut choice = wire::Endpoint::start(false, false).await?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let bound = fixture::sqlite(&generation, &choice, directory.path(), 2).await?;
    let gate = bound.fixture.service.hold_choice_final_gate_for_test()?;
    let release = gate.release.clone().drop_guard();
    let mut session = None;
    let checked = tokio::time::timeout(Duration::from_secs(25), async {
        session = Some(
            fixture::open(
                &bound,
                &generation,
                &workspace,
                Arc::new(HostSpies::default()),
            )
            .await?,
        );
        let running_session = session.as_mut().context("real session absent")?;
        let core = running_session.core().clone();
        let run = running_session.run(
            fixture::task(),
            Arc::new(Approval),
            CancellationToken::new(),
        );
        tokio::pin!(run);
        // Poll the real facade while observing the one-use Core claim. The gate
        // is after claimed=true and before the existing tenancy admission work.
        tokio::select! {
            result = &mut run => anyhow::bail!("Choice completed before final gate: {result:?}"),
            () = gate.entered.notified() => {}
        }
        let child = gate
            .execution
            .get()
            .context("actual claimed child")?
            .clone();
        ensure!(choice.calls() == 0 && generation.calls() == 0);
        let before = fixture::journal(&bound).await?;
        let (decision, link) = before["owned_choices"]
            .as_object()
            .context("Choice intent")?
            .iter()
            .next()
            .context("Choice intent absent")?;
        ensure!(link["child_execution"] == child.as_str() && link["outcome_digest"].is_null());
        // Real actor mutation, not a forged permission or synthetic send error.
        change_authority(&bound.fixture, &bound.key, &bound.route, "revoke").await?;
        gate.release.cancel();
        let _outcome = run.await;
        let state = core.snapshot().await;
        let failed = state
            .context_store
            .decisions
            .get(decision)
            .context("Choice outcome missing")?
            .outcome
            .as_ref()
            .context("Choice outcome unacknowledged")?
            .as_ref()
            .err()
            .context("revoked final gate succeeded")?;
        ensure!(
            !failed.may_have_run && failed.usage.is_none()
                && failed.kind == crate::gateway::model::decision::types::DecisionFailure::Transport,
            "final authority rejection must precede arming, without cancellation hiding the boundary"
        );
        let view = bitrouter_ai::harness::inspection::Inspection::capture(&state)
            .context("public inspection absent")?;
        ensure!(
            view.decisions.attempts == 1
                && view.decisions.failures == 1
                && view.decisions.pending == 0
        );
        ensure!(
            view.decisions.input_tokens == Some(0)
                && view.decisions.output_tokens == Some(0)
                && view.decisions.estimated_micro_usd == Some(0)
        );
        let after = fixture::journal(&bound).await?;
        ensure!(after["owned_choices"][decision]["outcome_digest"].is_string());
        let child_receipt = receipt(&bound, child.as_str()).await?;
        ensure!(
            child_receipt.attempts.is_empty() && choice.calls() == 0 && generation.calls() == 0
        );
        Ok(())
    })
    .await
    .context("Choice final gate test deadline")
    .and_then(|result| result);
    drop(release);
    let closed = match session.take() {
        Some(session) => session.close().await.map_err(anyhow::Error::from),
        None => Ok(()),
    };
    let choice_closed = choice.finish().await;
    finish(
        &bound.fixture,
        generation,
        checked.and(closed).and(choice_closed),
    )
    .await
}

#[tokio::test]
async fn owned_choice_transport_failure_after_arm_remains_unknown() -> Result<()> {
    use crate::storage::checkpoint::snapshot::context::inspection::InspectionExt;
    let generation = Provider::start(false).await?;
    let mut choice = wire::Endpoint::transport_failure().await?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let bound = fixture::sqlite(&generation, &choice, directory.path(), 2).await?;
    let mut session = None;
    let checked = tokio::time::timeout(Duration::from_secs(25), async {
        session = Some(
            fixture::open(
                &bound,
                &generation,
                &workspace,
                Arc::new(HostSpies::default()),
            )
            .await?,
        );
        let session = session.as_mut().context("real session absent")?;
        let _outcome = session
            .run(
                fixture::task(),
                Arc::new(Approval),
                CancellationToken::new(),
            )
            .await;
        let sent = choice.receive().await?;
        ensure!(sent.path == "/v1/decisions" && choice.calls() == 1);
        let state = session.core().snapshot().await;
        ensure!(state.context_store.decisions.len() == 1);
        let (decision, saved) = state
            .context_store
            .decisions
            .iter()
            .next()
            .context("Choice outcome missing")?;
        let failed = saved
            .outcome
            .as_ref()
            .context("Choice outcome unacknowledged")?
            .as_ref()
            .err()
            .context("broken socket produced Choice success")?;
        ensure!(failed.may_have_run && failed.usage.is_none());
        let view = bitrouter_ai::harness::inspection::Inspection::capture(&state)
            .context("public inspection absent")?;
        ensure!(
            view.decisions.input_tokens.is_none()
                && view.decisions.output_tokens.is_none()
                && view.decisions.estimated_micro_usd.is_none()
        );
        let journal = fixture::journal(&bound).await?;
        let link = &journal["owned_choices"][decision];
        ensure!(link["outcome_digest"].is_string());
        let child = link["child_execution"]
            .as_str()
            .context("child receipt link")?;
        let child_receipt = receipt(&bound, child).await?;
        ensure!(child_receipt.attempts.len() == 1 && !child_receipt.usage.complete);
        ensure!(child_receipt.attempts[0].outcome == Outcome::Unknown);
        Ok(())
    })
    .await
    .context("Choice transport control deadline")
    .and_then(|result| result);
    let closed = match session.take() {
        Some(session) => session.close().await.map_err(anyhow::Error::from),
        None => Ok(()),
    };
    let choice_closed = choice.finish().await;
    finish(
        &bound.fixture,
        generation,
        checked.and(closed).and(choice_closed),
    )
    .await
}
