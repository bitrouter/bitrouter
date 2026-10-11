use super::*;
use bitrouter_orchestrator::core::accounting::work::CostWorkKind;

#[derive(Clone, Copy, Debug)]
enum Pressure {
    Artifacts,
    Checkpoint,
}

async fn result_before_observation(pressure: Pressure, persisted: bool) -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let port = Arc::new(reconnect::FaultPort::new(
        harness.clone(),
        "tool.status",
        persisted,
    ));
    port.set_enabled(false);
    let (session, executor, settlements) =
        setup(vec![output(vec![call("read")])], port.clone(), false).await?;
    if matches!(pressure, Pressure::Artifacts) {
        let mut update = signal_update(&session, Vec::new()).await;
        update.manifest.artifact_quota_bytes = 1024 * 1024;
        session.signals("quota", update).await?;
    }
    let mut task = input();
    task.limits = Some(Limits {
        input_bytes: 4096,
        checkpoint_bytes: if matches!(pressure, Pressure::Artifacts) {
            256 * 1024
        } else {
            128 * 1024
        },
        unacknowledged_bytes: 512 * 1024,
        outstanding_tools: 1,
        ..Limits::default()
    });
    let input_revision = session.head().await.state_revision;
    let accepted = session.start("input", input_revision, task.clone()).await?;
    session.drive().await?;
    let command = harness.sent.lock().await.first().ok_or("tool")?.clone();
    let original = session.snapshot().await;
    let uncertain = repeated_restore::full_result(
        &harness,
        &command,
        ToolOutcome::EffectUnknown,
        "unknown-result",
    )
    .await?;
    let result_receipt = session
        .tool_result("unknown-result", uncertain.clone())
        .await?;
    let stopped =
        full_recovery_observation(&harness, &command, ToolStatus::Stopped, "first-stop", true)
            .await?;
    let stopped_receipt = session.tool_status("first-stop", stopped.clone()).await?;
    let mut admitted = Vec::new();
    let mut rejected = false;
    for index in 0..96 {
        let id = format!("optional-stop-{index}");
        let value = full_recovery_observation(
            &harness,
            &command,
            ToolStatus::Stopped,
            &id,
            matches!(pressure, Pressure::Artifacts),
        )
        .await?;
        match session.tool_status(&id, value.clone()).await {
            Ok(receipt) => admitted.push((value, receipt)),
            Err(error) => {
                assert_eq!(
                    (error.code, error.commit_status),
                    (ErrorCode::LimitExceeded, CommitStatus::NotCommitted)
                );
                assert!(
                    error.message.contains(match pressure {
                        Pressure::Artifacts => "artifact quota",
                        Pressure::Checkpoint => "checkpoint",
                    }),
                    "{error:?}"
                );
                assert!(session.operation(&id).await.is_none());
                assert!(
                    session
                        .snapshot()
                        .await
                        .run
                        .ok_or("run")?
                        .resource_error
                        .is_some()
                );
                // The rejected object was never referenced by an accepted head.
                let mut store = harness.store.lock().await;
                store.artifacts.remove(&id);
                store.artifact_bytes.remove(&id);
                rejected = true;
                break;
            }
        }
    }
    assert!(rejected);
    assert!(!admitted.is_empty());
    let value = full_recovery_observation(
        &harness,
        &command,
        ToolStatus::EffectUnknown,
        "first-unknown",
        true,
    )
    .await?;
    let before = session.head().await;
    port.set_enabled(true);
    let error = session
        .tool_status("first-unknown", value.clone())
        .await
        .err()
        .ok_or("missing ACK fault")?;
    assert_eq!(
        (error.code, error.commit_status),
        (ErrorCode::CheckpointUnavailable, CommitStatus::Unknown),
        "{pressure:?}/{persisted}: {error:?}"
    );
    assert_eq!(session.head().await, before);
    assert!(session.operation("first-unknown").await.is_none());
    let pending = port.proposals.lock().await.last().ok_or("pending")?.clone();
    assert_eq!(
        harness.store.lock().await.head.state_revision,
        before.state_revision + u64::from(persisted)
    );
    reconnect::reconnect(&session, &harness).await?;
    let receipt = session
        .operation("first-unknown")
        .await
        .ok_or("missing status receipt")?;
    let head = session.head().await;
    assert_eq!(
        session.tool_status("first-unknown", value.clone()).await?,
        receipt
    );
    assert_eq!(
        session
            .tool_result("unknown-result", uncertain.clone())
            .await?,
        result_receipt
    );
    assert_eq!(
        session.tool_status("first-stop", stopped.clone()).await?,
        stopped_receipt
    );
    for (observed, accepted) in &admitted {
        assert_eq!(
            session
                .tool_status(&accepted.operation_id, observed.clone())
                .await?,
            *accepted
        );
    }
    assert_eq!(session.head().await, head);
    let replayed = port
        .proposals
        .lock()
        .await
        .iter()
        .filter(|batch| batch.identity.batch_id == pending.identity.batch_id)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(replayed.len(), if persisted { 1 } else { 2 });
    assert!(replayed.iter().all(|batch| batch == &pending));
    assert_eq!(
        harness
            .store
            .lock()
            .await
            .batches
            .iter()
            .filter(|batch| batch.identity.batch_id == pending.identity.batch_id)
            .count(),
        1
    );
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::RecoveryRequired)
    );
    session.disconnect().await;
    let replacement = recovery::harness_at(harness.store.lock().await.clone()).await;
    let definite = repeated_restore::full_result(
        &replacement,
        &command,
        ToolOutcome::Succeeded,
        "definite-result",
    )
    .await?;
    let mut request = recovery::request(&*replacement.store.lock().await, false)?;
    request.results.push(definite.clone());
    let (restored, restored_executor) =
        recovery::restore(request, replacement.clone(), Vec::new()).await?;
    let done = restored.drive().await?;
    assert_eq!(done.run.as_ref().ok_or("run")?.status, RunStatus::Failed);
    let call = done
        .root_turn()
        .ok_or("turn")?
        .invocations
        .first()
        .ok_or("call")?;
    assert_eq!(call.result.as_ref(), Some(&definite));
    assert_eq!(call.prior_uncertain_result.as_ref(), Some(&uncertain));
    assert_eq!(call.tool_observations.get("first-unknown"), Some(&value));
    assert_eq!(call.tool_observations.get("first-stop"), Some(&stopped));
    for (observed, receipt) in &admitted {
        assert_eq!(
            call.tool_observations.get(&receipt.operation_id),
            Some(observed)
        );
        assert_eq!(done.operations.get(&receipt.operation_id), Some(receipt));
    }
    for (run, ledger) in &original.cost_work {
        for (id, work) in &ledger.work {
            if work.kind == CostWorkKind::ProviderAttempt {
                assert_eq!(done.cost_work[run].work.get(id), Some(work));
            }
        }
    }
    assert_eq!(
        restored.start("input", input_revision, task).await?,
        accepted
    );
    restored
        .release("release", restored.head().await.state_revision)
        .await?;
    assert_eq!(restored.snapshot().await.releases.len(), 1);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(settlements.load(Ordering::SeqCst), 1);
    assert_eq!(restored_executor.calls.load(Ordering::SeqCst), 0);
    assert_eq!(harness.sent.lock().await.len(), 1);
    assert!(replacement.sent.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn unknown_result_keeps_first_unknown_observation_body_reserved() -> TestResult {
    for persisted in [false, true] {
        result_before_observation(Pressure::Artifacts, persisted).await?;
    }
    Ok(())
}

#[tokio::test]
async fn unknown_result_keeps_first_unknown_observation_checkpoint_reserved() -> TestResult {
    for persisted in [false, true] {
        result_before_observation(Pressure::Checkpoint, persisted).await?;
    }
    Ok(())
}
