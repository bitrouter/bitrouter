use super::*;
use bitrouter_orchestrator::core::checkpoint::ToolStartFence;

pub(super) async fn full_result(
    harness: &Harness,
    command: &ToolExecute,
    status: ToolOutcome,
    key: &str,
) -> Result<ToolResult, Box<dyn std::error::Error>> {
    let limits = command.result_limits.ok_or("limits")?;
    let mut value = result(command);
    value.status = status;
    value.evidence =
        vec![evidence(harness, key, limits.artifact_bytes.ok_or("body bound")?).await?];
    let remaining = limits
        .payload_bytes
        .checked_sub(serde_json::to_vec(&value)?.len() as u64)
        .ok_or("result envelope")?;
    value
        .output
        .push_str(&"x".repeat(usize::try_from(remaining)?));
    limits.validate_result(&value)?;
    assert_eq!(
        serde_json::to_vec(&value)?.len() as u64,
        limits.payload_bytes
    );
    Ok(value)
}

#[tokio::test]
async fn repeated_archive_growth_rejects_optional_restore_but_preserves_cleanup() -> TestResult {
    let mut fixture = Harness::new(None, None);
    fixture.wait_for_approval = true;
    let mut harness = Arc::new(fixture);
    let (mut session, executor, settlements) = setup(
        vec![output(vec![call("read-a"), call("read-b")])],
        harness.clone(),
        false,
    )
    .await?;
    let mut update = signal_update(&session, Vec::new()).await;
    update.manifest.artifact_quota_bytes = 1024 * 1024;
    session.signals("quota", update).await?;
    let mut task = input();
    task.limits = Some(Limits {
        input_bytes: 4096,
        checkpoint_bytes: 128 * 1024,
        unacknowledged_bytes: 256 * 1024,
        outstanding_tools: 2,
        ..Limits::default()
    });
    let input_revision = session.head().await.state_revision;
    let accepted = session.start("input", input_revision, task.clone()).await?;
    session.drive().await?;
    let commands = harness.sent.lock().await.clone();
    assert_eq!(commands.len(), 2);
    for command in &commands {
        assert!(harness.store.lock().await.try_start_tool(ToolStartFence {
            invocation_id: command.invocation_id.clone(),
            attempt_id: command.attempt_id.clone(),
        }));
    }
    let original = session.snapshot().await;
    let original_steps = serde_json::to_value(&original.root_turn().ok_or("turn")?.steps)?;
    let original_cost = original.cost_work.clone();
    let mut history = vec![Vec::new(); commands.len()];
    let mut activity_history = Vec::new();
    let mut rejected = false;
    for round in 0..96 {
        session.disconnect().await;
        let replacement = recovery::harness_at(harness.store.lock().await.clone()).await;
        let before = replacement.store.lock().await.head.clone();
        let mut observations = Vec::new();
        for (index, command) in commands.iter().enumerate() {
            observations.push(
                full_recovery_observation(
                    &replacement,
                    command,
                    ToolStatus::Running,
                    &format!("repeat-{round}-{index}"),
                    false,
                )
                .await?,
            );
        }
        let mut request = recovery::request(&*replacement.store.lock().await, false)?;
        request.tools = observations.clone();
        let activity = request.active_time.clone().ok_or("activity")?;
        // Activity evidence is deterministic fixture input, not a measured
        // remote clock handoff. Every accepted observation is retained.
        match recovery::restore(request, replacement.clone(), Vec::new()).await {
            Ok((restored, restored_executor)) => {
                assert_eq!(restored_executor.calls.load(Ordering::SeqCst), 0);
                let state = restored.snapshot().await;
                for (index, call) in state
                    .root_turn()
                    .ok_or("turn")?
                    .invocations
                    .iter()
                    .enumerate()
                {
                    assert_eq!(call.prior_recovery_observations, history[index]);
                    assert_eq!(
                        call.recovery_observation.as_ref(),
                        Some(&observations[index])
                    );
                    history[index].push(observations[index].clone());
                }
                assert_eq!(
                    serde_json::to_value(&state.root_turn().ok_or("turn")?.steps)?,
                    original_steps
                );
                assert_eq!(state.cost_work, original_cost);
                assert_eq!(state.operations.get("input"), Some(&accepted));
                activity_history.push(activity);
                assert_eq!(
                    state.run.as_ref().ok_or("run")?.activity_reconciliations,
                    activity_history
                );
                session = restored;
                harness = replacement;
            }
            Err(error) => {
                let error = error.downcast_ref::<CoreError>().ok_or("core error")?;
                assert_eq!(
                    (error.code, error.commit_status),
                    (ErrorCode::LimitExceeded, CommitStatus::NotCommitted)
                );
                assert!(
                    error.message.contains("artifact quota"),
                    "{}",
                    error.message
                );
                assert_eq!(replacement.store.lock().await.head, before);
                assert!(replacement.sent.lock().await.is_empty());
                assert!(replacement.cancelled.lock().await.is_empty());
                // Only the rejected, never-referenced staging may be reclaimed.
                // Historical checkpoint roots remain pinned in this fixture;
                // this test does not claim a physical storage lease bound.
                let mut store = replacement.store.lock().await;
                for observed in &observations {
                    for reference in &observed.evidence {
                        store.artifacts.remove(&reference.artifact_id);
                        store.artifact_bytes.remove(&reference.artifact_id);
                    }
                }
                drop(store);
                harness = replacement;
                rejected = true;
                break;
            }
        }
    }
    assert!(
        rejected,
        "repeated archive growth never reached admission failure"
    );
    assert!(history[0].len() > 2);
    assert!(session.snapshot().await.recovery_archive.is_some());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(settlements.load(Ordering::SeqCst), 1);

    // First essential stopped/unknown observations use frozen allowances even
    // when another full optional Running restore has just been refused.
    for (phase, status) in [ToolStatus::Stopped, ToolStatus::EffectUnknown]
        .into_iter()
        .enumerate()
    {
        let replacement = recovery::harness_at(harness.store.lock().await.clone()).await;
        let mut observations = Vec::new();
        for (index, command) in commands.iter().enumerate() {
            observations.push(
                full_recovery_observation(
                    &replacement,
                    command,
                    status,
                    &format!("essential-{phase}-{index}"),
                    true,
                )
                .await?,
            );
        }
        let mut request = recovery::request(&*replacement.store.lock().await, false)?;
        request.tools = observations.clone();
        activity_history.push(request.active_time.clone().ok_or("activity")?);
        let restored = restore_losing_ack(request, replacement.clone(), phase == 1).await?;
        let state = restored.snapshot().await;
        assert_eq!(
            state.run.as_ref().ok_or("run")?.activity_reconciliations,
            activity_history
        );
        for (index, call) in state
            .root_turn()
            .ok_or("turn")?
            .invocations
            .iter()
            .enumerate()
        {
            assert_eq!(call.prior_recovery_observations, history[index]);
            assert_eq!(
                call.recovery_observation.as_ref(),
                Some(&observations[index])
            );
            assert!(call.result.is_none());
            history[index].push(observations[index].clone());
        }
        assert_eq!(state.cost_work, original_cost);
        assert_eq!(state.operations.get("input"), Some(&accepted));
        assert!(replacement.sent.lock().await.is_empty());
        if phase == 0 {
            restored.disconnect().await;
        }
        session = restored;
        harness = replacement;
    }
    let mut uncertain = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        let value = full_result(
            &harness,
            command,
            ToolOutcome::EffectUnknown,
            &format!("unknown-{index}"),
        )
        .await?;
        let operation = format!("unknown-{index}");
        let receipt = session.tool_result(&operation, value.clone()).await?;
        let head = session.head().await;
        assert_eq!(
            session.tool_result(&operation, value.clone()).await?,
            receipt
        );
        assert_eq!(session.head().await, head);
        uncertain.push(value);
    }
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::RecoveryRequired)
    );
    session.disconnect().await;
    let replacement = recovery::harness_at(harness.store.lock().await.clone()).await;
    let mut outcomes = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        outcomes.push(
            full_result(
                &replacement,
                command,
                ToolOutcome::Succeeded,
                &format!("definite-{index}"),
            )
            .await?,
        );
    }
    let mut request = recovery::request(&*replacement.store.lock().await, false)?;
    request.results = outcomes.clone();
    activity_history.push(request.active_time.clone().ok_or("activity")?);
    let (restored, restored_executor) =
        recovery::restore(request, replacement.clone(), Vec::new()).await?;
    let state = restored.snapshot().await;
    assert_eq!(
        state.run.as_ref().ok_or("run")?.activity_reconciliations,
        activity_history
    );
    for (index, call) in state
        .root_turn()
        .ok_or("turn")?
        .invocations
        .iter()
        .enumerate()
    {
        assert_eq!(call.result.as_ref(), Some(&outcomes[index]));
        assert_eq!(
            call.prior_uncertain_result.as_ref(),
            Some(&uncertain[index])
        );
        assert_eq!(call.recovery_observation.as_ref(), history[index].last());
        assert_eq!(
            call.prior_recovery_observations,
            history[index][..history[index].len() - 1]
        );
    }
    let revision = restored.head().await.state_revision;
    restored
        .cancel_run(
            "cleanup",
            revision,
            &state.run.as_ref().ok_or("run")?.run_id,
        )
        .await?;
    assert_eq!(
        restored.drive().await?.run.map(|run| run.status),
        Some(RunStatus::Cancelled)
    );
    let before = restored.head().await;
    assert_eq!(
        restored.start("input", input_revision, task).await?,
        accepted
    );
    assert_eq!(restored.head().await, before);
    restored.release("release", before.state_revision).await?;
    assert_eq!(restored.snapshot().await.releases.len(), 1);
    assert_eq!(restored_executor.calls.load(Ordering::SeqCst), 0);
    assert!(replacement.sent.lock().await.is_empty());
    eprintln!(
        "accepted {} full Running handoffs before logical archive rejection; full stopped, unknown and both outcome bodies retained",
        history[0].len() - 2
    );
    session.disconnect().await;
    Ok(())
}
