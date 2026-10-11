use super::*;

#[tokio::test]
async fn explicit_restore_confirms_identical_live_result_after_stopped_or_missing_evidence()
-> TestResult {
    for stopped in [false, true] {
        for cancelled in [false, true] {
            let harness = Arc::new(Harness::new(None, None));
            let (session, original_executor, settlements) =
                setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
            let accepted = session.start("input", 1, input()).await?;
            session.drive().await?;
            let command = harness.sent.lock().await.first().ok_or("command")?.clone();
            if cancelled {
                session
                    .cancel_run(
                        "cancel",
                        session.head().await.state_revision,
                        &accepted.assigned_ids["run_id"],
                    )
                    .await?;
            }
            session.disconnect().await;
            let replacement = harness_at(harness.store.lock().await.clone()).await;
            let mut restore_input = request(&*replacement.store.lock().await, false)?;
            if stopped {
                restore_input
                    .tools
                    .push(observation(&command, ToolStatus::Stopped));
            }
            let (restored, executor) =
                restore(restore_input, replacement.clone(), Vec::new()).await?;
            assert_eq!(
                restored.drive().await?.run.map(|run| run.status),
                Some(RunStatus::RecoveryRequired)
            );
            let outcome = result(&command);
            let receipt = restored.tool_result("known-live", outcome.clone()).await?;
            assert_eq!(
                restored.drive().await?.run.map(|run| run.status),
                Some(RunStatus::RecoveryRequired)
            );
            assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
            assert!(replacement.sent.lock().await.is_empty());
            restored.disconnect().await;

            // Merely loading a checkpoint containing the live result does not
            // supply explicit confirmation of the previously uncertain effect.
            let empty_harness = harness_at(replacement.store.lock().await.clone()).await;
            let empty_input = request(&*empty_harness.store.lock().await, false)?;
            let (empty, empty_executor) =
                restore(empty_input, empty_harness.clone(), Vec::new()).await?;
            assert_eq!(
                empty.drive().await?.run.map(|run| run.status),
                Some(RunStatus::RecoveryRequired)
            );
            assert_eq!(empty_executor.calls.load(Ordering::SeqCst), 0);
            assert!(empty_harness.sent.lock().await.is_empty());
            empty.disconnect().await;

            let confirmed_harness = harness_at(empty_harness.store.lock().await.clone()).await;
            let mut confirmed_input = request(&*confirmed_harness.store.lock().await, false)?;
            confirmed_input.results.push(outcome.clone());
            let (confirmed, confirmed_executor) = restore(
                confirmed_input,
                confirmed_harness.clone(),
                vec![output(vec![text("finished")])],
            )
            .await?;
            let done = confirmed.drive().await?;
            assert_eq!(
                done.run.as_ref().ok_or("run")?.status,
                if cancelled {
                    RunStatus::Cancelled
                } else {
                    RunStatus::Completed
                },
                "stopped={stopped}, cancelled={cancelled}"
            );
            let call = done
                .root_turn()
                .ok_or("turn")?
                .invocations
                .first()
                .ok_or("call")?;
            assert_eq!(call.result.as_ref(), Some(&outcome));
            assert!(call.prior_uncertain_result.is_none());
            assert_eq!(
                call.recovery_observation
                    .as_ref()
                    .map(|observation| observation.status),
                stopped.then_some(ToolStatus::Stopped)
            );
            let head = confirmed.head().await;
            assert_eq!(confirmed.tool_result("known-live", outcome).await?, receipt);
            assert_eq!(confirmed.start("input", 1, input()).await?, accepted);
            assert_eq!(confirmed.head().await, head);
            confirmed.release("release", head.state_revision).await?;
            assert_eq!(
                confirmed_executor.calls.load(Ordering::SeqCst),
                usize::from(!cancelled)
            );
            assert_eq!(original_executor.calls.load(Ordering::SeqCst), 1);
            assert_eq!(settlements.load(Ordering::SeqCst), 1);
            assert!(confirmed_harness.sent.lock().await.is_empty());
        }
    }
    Ok(())
}

#[tokio::test]
async fn explicit_known_result_does_not_resolve_another_unknown_or_missing_tool() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, original_executor, _) = setup(
        vec![output(vec![call("first"), call("second")])],
        harness.clone(),
        false,
    )
    .await?;
    let accepted = session.start("input", 1, input()).await?;
    session.drive().await?;
    let commands = harness.sent.lock().await.clone();
    assert_eq!(commands.len(), 2);
    session
        .cancel_run(
            "cancel",
            session.head().await.state_revision,
            &accepted.assigned_ids["run_id"],
        )
        .await?;
    session.disconnect().await;
    let replacement = harness_at(harness.store.lock().await.clone()).await;
    let restore_input = request(&*replacement.store.lock().await, false)?;
    let (restored, executor) = restore(restore_input, replacement.clone(), Vec::new()).await?;
    restored
        .tool_result("first-known", result(&commands[0]))
        .await?;
    restored.disconnect().await;
    let missing = replacement.store.lock().await.clone();
    for unknown_result in [false, true] {
        let phase_harness = harness_at(missing.clone()).await;
        let mut phase_input = request(&*phase_harness.store.lock().await, false)?;
        phase_input.results.push(result(&commands[0]));
        let (phase, phase_executor) =
            restore(phase_input, phase_harness.clone(), Vec::new()).await?;
        assert_eq!(
            phase.drive().await?.run.map(|run| run.status),
            Some(RunStatus::RecoveryRequired)
        );
        let mut uncertain = result(&commands[1]);
        uncertain.status = ToolOutcome::EffectUnknown;
        if unknown_result {
            phase
                .tool_result("second-unknown", uncertain.clone())
                .await?;
        }
        phase.disconnect().await;
        let retry_harness = harness_at(phase_harness.store.lock().await.clone()).await;
        let mut retry_input = request(&*retry_harness.store.lock().await, false)?;
        retry_input.results.push(result(&commands[0]));
        if unknown_result {
            // Identical unknown results, even beside a confirmed result, do
            // not establish an outcome for the unresolved invocation.
            retry_input.results.push(uncertain.clone());
        }
        let (retry, retry_executor) =
            restore(retry_input, retry_harness.clone(), Vec::new()).await?;
        let blocked = retry.drive().await?;
        assert_eq!(
            blocked.run.as_ref().ok_or("run")?.status,
            RunStatus::RecoveryRequired
        );
        assert_eq!(
            blocked.root_turn().ok_or("turn")?.invocations[1]
                .result
                .as_ref(),
            unknown_result.then_some(&uncertain)
        );
        retry.disconnect().await;
        let final_harness = harness_at(retry_harness.store.lock().await.clone()).await;
        let mut final_input = request(&*final_harness.store.lock().await, false)?;
        final_input.results = commands.iter().map(result).collect();
        let (final_session, final_executor) =
            restore(final_input, final_harness.clone(), Vec::new()).await?;
        let done = final_session.drive().await?;
        assert_eq!(done.run.as_ref().ok_or("run")?.status, RunStatus::Cancelled);
        assert_eq!(
            done.root_turn().ok_or("turn")?.invocations[1]
                .prior_uncertain_result
                .as_ref(),
            unknown_result.then_some(&uncertain)
        );
        final_session
            .release("release", final_session.head().await.state_revision)
            .await?;
        for executor in [&phase_executor, &retry_executor, &final_executor] {
            assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        }
        for harness in [&phase_harness, &retry_harness, &final_harness] {
            assert!(harness.sent.lock().await.is_empty());
        }
    }
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert_eq!(original_executor.calls.load(Ordering::SeqCst), 1);
    Ok(())
}
