use super::*;
use bitrouter_orchestrator::core::checkpoint::ToolStartFence;
use bitrouter_orchestrator::core::protocol::{ToolObservation, ToolStatus};

fn observation(command: &ToolExecute, status: ToolStatus) -> ToolObservation {
    ToolObservation {
        invocation_id: command.invocation_id.clone(),
        attempt_id: command.attempt_id.clone(),
        status,
        evidence: Vec::new(),
    }
}

#[tokio::test]
async fn tool_status_running_time_survives_a_held_reconnect_ack() -> TestResult {
    let harness = Arc::new(Harness::new(None, Some("session.reconnected")));
    let (session, _, _) = setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
    session.start("input", 1, input()).await?;
    session.drive().await?;
    let command = harness.sent.lock().await[0].clone();
    session
        .tool_status("running", observation(&command, ToolStatus::Running))
        .await?;
    session.disconnect().await;
    let before = session.snapshot().await.run.ok_or("run")?.active_ms;
    let head = harness.store.lock().await.head.clone();
    let reconnecting = tokio::spawn({
        let session = session.clone();
        async move { session.reconnect(&grant(), &head).await }
    });
    tokio::time::timeout(Duration::from_secs(30), harness.seen.acquire())
        .await??
        .forget();
    tokio::time::sleep(Duration::from_millis(125)).await;
    harness.resume.add_permits(1);
    reconnecting.await??;
    session
        .tool_status("stopped", observation(&command, ToolStatus::Stopped))
        .await?;
    let after = session.snapshot().await.run.ok_or("run")?.active_ms;
    assert!(
        after >= before + 125,
        "running time during ACK wait was lost"
    );
    Ok(())
}

#[tokio::test]
async fn tool_status_has_a_durable_receipt_but_cannot_substitute_for_result() -> TestResult {
    let mut fixture = Harness::new(None, Some("tool.status"));
    fixture.wait_for_approval = true;
    let harness = Arc::new(fixture);
    let (session, executor, _) = setup(
        vec![output(vec![call("read")]), output(vec![text("done")])],
        harness.clone(),
        false,
    )
    .await?;
    session.start("input", 1, input()).await?;
    session.drive().await?;
    let command = harness.sent.lock().await[0].clone();
    let waiting = observation(&command, ToolStatus::WaitingApproval);
    let before = session.head().await;
    let pending = tokio::spawn({
        let session = session.clone();
        let waiting = waiting.clone();
        async move { session.tool_status("waiting", waiting).await }
    });
    tokio::time::timeout(Duration::from_secs(30), harness.seen.acquire())
        .await??
        .forget();
    assert_eq!(session.head().await, before);
    assert!(
        session
            .snapshot()
            .await
            .root_turn()
            .ok_or("turn")?
            .invocations[0]
            .tool_observations
            .is_empty()
    );
    harness.hold_enabled.store(false, Ordering::SeqCst);
    harness.resume.add_permits(1);
    let receipt = pending.await??;
    assert_eq!(
        session.tool_status("waiting", waiting.clone()).await?,
        receipt
    );
    assert!(harness.store.lock().await.try_start_tool(ToolStartFence {
        invocation_id: command.invocation_id.clone(),
        attempt_id: command.attempt_id.clone(),
    }));
    session
        .tool_status("running", observation(&command, ToolStatus::Running))
        .await?;
    let regression = session
        .tool_status("not-started", observation(&command, ToolStatus::NotStarted))
        .await;
    assert_eq!(
        regression.err().map(|e| e.code),
        Some(ErrorCode::OperationConflict)
    );
    session
        .tool_status("stopped", observation(&command, ToolStatus::Stopped))
        .await?;
    // Exact operation replay cannot regress the current facts. A fresh
    // operation reports current state and cannot return to approval waiting.
    assert_eq!(
        session.tool_status("waiting", waiting.clone()).await?,
        receipt
    );
    assert_eq!(
        session
            .tool_status("old-wait", waiting)
            .await
            .err()
            .map(|e| e.code),
        Some(ErrorCode::OperationConflict)
    );
    let pending = session.drive().await?;
    assert_eq!(
        pending.run.as_ref().map(|run| run.status),
        Some(RunStatus::Waiting)
    );
    assert!(
        pending.root_turn().ok_or("turn")?.invocations[0]
            .result
            .is_none()
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    session.tool_result("result", result(&command)).await?;
    assert_eq!(
        session.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(session.operation("waiting").await, Some(receipt));
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn tool_status_rejects_unknown_attempt_conflicting_operation_and_invalid_evidence()
-> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
    session.start("input", 1, input()).await?;
    let missing = ToolObservation {
        invocation_id: "missing".into(),
        attempt_id: "missing".into(),
        status: ToolStatus::Running,
        evidence: Vec::new(),
    };
    assert_eq!(
        session
            .tool_status("missing", missing)
            .await
            .err()
            .map(|e| e.code),
        Some(ErrorCode::InvalidToolResult)
    );
    session.drive().await?;
    let command = harness.sent.lock().await[0].clone();
    let before = session.head().await;
    let mut wrong = observation(&command, ToolStatus::Running);
    wrong.attempt_id = "foreign".into();
    assert_eq!(
        session
            .tool_status("wrong-attempt", wrong)
            .await
            .err()
            .map(|e| e.code),
        Some(ErrorCode::InvalidToolResult)
    );
    let mut invalid = observation(&command, ToolStatus::Running);
    invalid.evidence.push(ArtifactRef {
        artifact_id: "evidence".into(),
        sha256: "wrong".into(),
        bytes: 1,
        media_type: "text/plain".into(),
    });
    assert_eq!(
        session
            .tool_status("invalid", invalid.clone())
            .await
            .err()
            .map(|e| e.code),
        Some(ErrorCode::CheckpointConflict)
    );
    invalid.evidence[0].media_type = "x".repeat(70_000);
    assert_eq!(
        session
            .tool_status("oversize", invalid)
            .await
            .err()
            .map(|e| e.code),
        Some(ErrorCode::LimitExceeded)
    );
    assert_eq!(session.head().await, before);
    let running = observation(&command, ToolStatus::Running);
    session.tool_status("running", running.clone()).await?;
    let mut conflict = running;
    conflict.evidence.push(ArtifactRef {
        artifact_id: "evidence".into(),
        sha256: sha256(b"x"),
        bytes: 1,
        media_type: "text/plain".into(),
    });
    harness
        .store
        .lock()
        .await
        .put_artifact(conflict.evidence[0].clone(), b"x")?;
    session
        .tool_status("new-running-evidence", conflict.clone())
        .await?;
    assert_eq!(
        session
            .snapshot()
            .await
            .root_turn()
            .ok_or("turn")?
            .invocations[0]
            .tool_observations
            .len(),
        2
    );
    assert_eq!(
        session
            .tool_status("running", conflict)
            .await
            .err()
            .map(|e| e.code),
        Some(ErrorCode::OperationConflict)
    );
    session.tool_result("result", result(&command)).await?;
    assert_eq!(
        session
            .tool_status(
                "late-unknown",
                observation(&command, ToolStatus::EffectUnknown)
            )
            .await
            .err()
            .map(|e| e.code),
        Some(ErrorCode::OperationConflict)
    );
    Ok(())
}

#[tokio::test]
async fn tool_status_ack_loss_reconciles_without_replaying_execution() -> TestResult {
    for committed in [false, true] {
        let harness = Arc::new(Harness::new(None, None));
        let port = Arc::new(super::reconnect::FaultPort::new(
            harness.clone(),
            "tool.status",
            committed,
        ));
        let (session, executor, _) = setup(
            vec![output(vec![call("read")]), output(vec![text("done")])],
            port,
            false,
        )
        .await?;
        session.start("input", 1, input()).await?;
        session.drive().await?;
        let command = harness.sent.lock().await[0].clone();
        let running = observation(&command, ToolStatus::Running);
        assert_eq!(
            session
                .tool_status("running", running.clone())
                .await
                .err()
                .map(|e| e.commit_status),
            Some(CommitStatus::Unknown)
        );
        super::reconnect::reconnect(&session, &harness).await?;
        let receipt = session.tool_status("running", running).await?;
        assert_eq!(session.operation("running").await, Some(receipt));
        assert_eq!(harness.sent.lock().await.len(), 1);
        session.tool_result("result", result(&command)).await?;
        assert_eq!(
            session.drive().await?.run.as_ref().map(|run| run.status),
            Some(RunStatus::Completed)
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    }
    Ok(())
}

#[tokio::test]
async fn tool_status_unknown_effect_requires_explicit_restoration_even_after_live_result()
-> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) =
        setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
    session.start("input", 1, input()).await?;
    session.drive().await?;
    let command = harness.sent.lock().await[0].clone();
    let unknown = observation(&command, ToolStatus::EffectUnknown);
    let original = session.tool_status("uncertain", unknown.clone()).await?;
    assert_eq!(
        session.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::RecoveryRequired)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert!(session.snapshot().await.root_queue.paused);
    session
        .tool_status("stopped", observation(&command, ToolStatus::Stopped))
        .await?;
    session.tool_result("definite", result(&command)).await?;
    assert_eq!(
        session.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::RecoveryRequired)
    );
    session.disconnect().await;
    let replacement = super::recovery::harness_at(harness.store.lock().await.clone()).await;
    let mut request = super::recovery::request(&*replacement.store.lock().await, false)?;
    request
        .tools
        .push(observation(&command, ToolStatus::Stopped));
    request.results.push(result(&command));
    let (restored, executor) = super::recovery::restore(
        request,
        replacement.clone(),
        vec![output(vec![text("reconciled")])],
    )
    .await?;
    // Exact operation replay does not resurrect reconciled uncertainty. A new
    // uncertainty claim conflicts with the already confirmed definite outcome.
    assert_eq!(
        restored.tool_status("uncertain", unknown.clone()).await?,
        original
    );
    assert_eq!(
        restored
            .tool_status("old-uncertain", unknown)
            .await
            .err()
            .map(|e| e.code),
        Some(ErrorCode::OperationConflict)
    );
    let done = restored.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert!(
        done.root_turn().ok_or("turn")?.invocations[0]
            .tool_observations
            .contains_key("uncertain")
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert!(replacement.sent.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn tool_status_renewed_uncertainty_blocks_an_unfinished_restored_tool() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
    session.start("input", 1, input()).await?;
    session.drive().await?;
    let command = harness.sent.lock().await[0].clone();
    session
        .tool_status("running", observation(&command, ToolStatus::Running))
        .await?;
    let unknown = observation(&command, ToolStatus::EffectUnknown);
    let original = session.tool_status("uncertain", unknown.clone()).await?;
    session.disconnect().await;
    let replacement = super::recovery::harness_at(harness.store.lock().await.clone()).await;
    let mut request = super::recovery::request(&*replacement.store.lock().await, false)?;
    request
        .tools
        .push(observation(&command, ToolStatus::Running));
    let (restored, executor) =
        super::recovery::restore(request, replacement.clone(), Vec::new()).await?;
    assert_eq!(
        restored.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Waiting)
    );
    // An old operation remains an immutable receipt after reconciliation.
    assert_eq!(
        restored.tool_status("uncertain", unknown.clone()).await?,
        original
    );
    assert_eq!(
        restored.snapshot().await.run.as_ref().map(|run| run.status),
        Some(RunStatus::Waiting)
    );
    // A fresh operation can describe a new uncertainty with different evidence.
    let artifact = ArtifactRef {
        artifact_id: "renewed-uncertainty".into(),
        sha256: sha256(b"lost"),
        bytes: 4,
        media_type: "text/plain".into(),
    };
    replacement
        .store
        .lock()
        .await
        .put_artifact(artifact.clone(), b"lost")?;
    let mut renewed = unknown.clone();
    renewed.evidence.push(artifact);
    restored.tool_status("uncertain-again", renewed).await?;
    let blocked = restored.drive().await?;
    assert_eq!(
        blocked.run.as_ref().map(|run| run.status),
        Some(RunStatus::RecoveryRequired)
    );
    assert!(blocked.root_queue.paused);
    assert!(
        blocked.root_turn().ok_or("turn")?.invocations[0]
            .result
            .is_none()
    );
    assert_eq!(
        blocked.root_turn().ok_or("turn")?.invocations[0].tool_observations["uncertain"],
        unknown
    );
    assert_eq!(
        blocked.root_turn().ok_or("turn")?.invocations[0].tool_observations["uncertain-again"]
            .evidence
            .len(),
        1
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert!(replacement.sent.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn tool_status_evidence_is_a_restore_dependency_and_cannot_be_retargeted() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
    session.start("input", 1, input()).await?;
    session.drive().await?;
    let command = harness.sent.lock().await[0].clone();
    let artifact = ArtifactRef {
        artifact_id: "observation-evidence".into(),
        sha256: sha256(b"proof"),
        bytes: 5,
        media_type: "text/plain".into(),
    };
    harness
        .store
        .lock()
        .await
        .put_artifact(artifact.clone(), b"proof")?;
    let mut running = observation(&command, ToolStatus::Running);
    running.evidence.push(artifact);
    session.tool_status("running", running).await?;
    session.disconnect().await;
    for tamper in [
        "missing-artifact",
        "attempt",
        "phase",
        "regression",
        "receipt",
        "revision",
        "operation",
    ] {
        let replacement = super::recovery::harness_at(harness.store.lock().await.clone()).await;
        let mut request = super::recovery::request(&*replacement.store.lock().await, false)?;
        request
            .tools
            .push(observation(&command, ToolStatus::Running));
        if tamper == "missing-artifact" {
            request.available_artifacts.clear();
        } else if tamper == "regression" {
            request.tools[0].status = ToolStatus::NotStarted;
        } else {
            let mut payload = request
                .binding
                .checkpoint
                .as_ref()
                .ok_or("checkpoint")?
                .decode(&Limits::default())?;
            let root = payload.checkpoint.state["agent_id"]
                .as_str()
                .ok_or("root")?
                .to_owned();
            let observed = &mut payload.checkpoint.state["agents"][&root]["turn"]["invocations"][0]
                ["tool_observations"]["running"];
            if tamper == "attempt" {
                observed["attempt_id"] = json!("foreign");
            } else if tamper == "phase" {
                observed["status"] = json!("stopped");
            } else if tamper == "receipt" {
                payload.checkpoint.state["operations"]["running"]["assigned_ids"]["attempt_id"] =
                    json!("foreign");
            } else if tamper == "revision" {
                payload.checkpoint.state["operations"]["running"]["state_revision"] =
                    json!(request.binding.durable_head.state_revision + 1);
            } else if let Some(operations) = payload.checkpoint.state["operations"].as_object_mut()
            {
                operations.remove("running");
            }
            let batch = CheckpointBatch::encode(&payload, &Limits::default())?;
            request.binding.durable_head.payload_sha256 = Some(batch.payload_sha256.clone());
            request.binding.checkpoint = Some(batch);
        }
        let head = replacement.store.lock().await.head.clone();
        assert!(
            super::recovery::restore(request, replacement.clone(), Vec::new())
                .await
                .is_err()
        );
        assert_eq!(replacement.store.lock().await.head, head);
    }
    Ok(())
}

#[tokio::test]
async fn tool_status_restore_can_reset_only_an_unstarted_approval() -> TestResult {
    for restored_approval in [false, true] {
        let mut fixture = Harness::new(None, None);
        fixture.wait_for_approval = true;
        let harness = Arc::new(fixture);
        let (session, _, _) =
            setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
        session.start("input", 1, input()).await?;
        session.drive().await?;
        let command = harness.sent.lock().await[0].clone();
        session
            .tool_status(
                "approval",
                observation(&command, ToolStatus::WaitingApproval),
            )
            .await?;
        session.disconnect().await;
        let mut source = harness;
        if restored_approval {
            let replacement = super::recovery::harness_at(source.store.lock().await.clone()).await;
            let mut request = super::recovery::request(&*replacement.store.lock().await, false)?;
            request
                .tools
                .push(observation(&command, ToolStatus::WaitingApproval));
            let (restored, _) =
                super::recovery::restore(request, replacement.clone(), Vec::new()).await?;
            restored.disconnect().await;
            source = replacement;
        }
        let mut replacement = super::recovery::harness_at(source.store.lock().await.clone()).await;
        Arc::get_mut(&mut replacement)
            .ok_or("shared replacement")?
            .wait_for_approval = true;
        let mut request = super::recovery::request(&*replacement.store.lock().await, false)?;
        request
            .tools
            .push(observation(&command, ToolStatus::NotStarted));
        let (restored, executor) =
            super::recovery::restore(request, replacement.clone(), Vec::new()).await?;
        assert!(replacement.sent.lock().await.is_empty());
        restored.drive().await?;
        let commands = replacement.sent.lock().await.clone();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].invocation_id, command.invocation_id);
        assert!(commands[0].execution_epoch > command.execution_epoch);
        restored
            .tool_status(
                "new-not-started",
                observation(&commands[0], ToolStatus::NotStarted),
            )
            .await?;
        restored
            .tool_status(
                "new-approval",
                observation(&commands[0], ToolStatus::WaitingApproval),
            )
            .await?;
        // The restoration reset does not permit a subsequent live regression.
        assert_eq!(
            restored
                .tool_status(
                    "regressed",
                    observation(&commands[0], ToolStatus::NotStarted)
                )
                .await
                .err()
                .map(|e| e.code),
            Some(ErrorCode::OperationConflict)
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    }
    Ok(())
}

#[tokio::test]
async fn tool_status_late_child_observation_keeps_its_original_run() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(
        vec![
            output(vec![call("root-read")]),
            output(vec![call("child-read")]),
            output(vec![text("child done")]),
            output(vec![text("root done")]),
        ],
        harness.clone(),
        false,
    )
    .await?;
    let first = session.start("first", 1, input()).await?;
    session.drive().await?;
    let root_command = harness.sent.lock().await[0].clone();
    let child = session
        .collaborate(
            "spawn",
            session.head().await.state_revision,
            &first.assigned_ids["agent_id"],
            Action::Spawn {
                task: work("child read"),
            },
        )
        .await?;
    session.drive().await?;
    let child_command = harness.sent.lock().await[1].clone();
    assert_eq!(child_command.agent_id, child.assigned_ids["agent_id"]);
    session
        .tool_result("child-result", result(&child_command))
        .await?;
    session.drive().await?;
    session
        .tool_result("root-result", result(&root_command))
        .await?;
    assert_eq!(
        session.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    let second = session
        .start("second", session.head().await.state_revision, input())
        .await?;
    session
        .tool_status(
            "late-child-stop",
            observation(&child_command, ToolStatus::Stopped),
        )
        .await?;
    let state = session.snapshot().await;
    assert_eq!(
        state.run.as_ref().map(|run| &run.run_id),
        second.assigned_ids.get("run_id")
    );
    let store = harness.store.lock().await;
    let payload = store
        .batches
        .last()
        .ok_or("checkpoint")?
        .decode(&store.limits)?;
    let event = payload.events.last().ok_or("event")?;
    assert_eq!(event.kind, "tool.status");
    assert_eq!(event.run_id.as_ref(), first.assigned_ids.get("run_id"));
    assert_eq!(event.agent_id.as_ref(), child.assigned_ids.get("agent_id"));
    Ok(())
}

#[tokio::test]
async fn tool_status_live_result_can_reconcile_uncertainty_from_an_earlier_restore() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
    session.start("input", 1, input()).await?;
    session.drive().await?;
    let command = harness.sent.lock().await[0].clone();
    session.disconnect().await;
    let replacement = super::recovery::harness_at(harness.store.lock().await.clone()).await;
    let mut request = super::recovery::request(&*replacement.store.lock().await, false)?;
    request
        .tools
        .push(observation(&command, ToolStatus::EffectUnknown));
    let (restored, _) = super::recovery::restore(request, replacement.clone(), Vec::new()).await?;
    restored
        .tool_result("definite-result", result(&command))
        .await?;
    assert_eq!(
        restored.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::RecoveryRequired)
    );
    restored.disconnect().await;
    let final_harness = super::recovery::harness_at(replacement.store.lock().await.clone()).await;
    let mut request = super::recovery::request(&*final_harness.store.lock().await, false)?;
    request
        .tools
        .push(observation(&command, ToolStatus::Stopped));
    request.results.push(result(&command));
    let (restored, executor) = super::recovery::restore(
        request,
        final_harness.clone(),
        vec![output(vec![text("done")])],
    )
    .await?;
    assert_eq!(
        restored.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert!(final_harness.sent.lock().await.is_empty());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn tool_status_repeated_restoration_cannot_erase_started_or_stopped_evidence() -> TestResult {
    for irreversible in [ToolStatus::Running, ToolStatus::Stopped] {
        let harness = Arc::new(Harness::new(None, None));
        let (session, _, _) =
            setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
        session.start("input", 1, input()).await?;
        session.drive().await?;
        let command = harness.sent.lock().await[0].clone();
        session.disconnect().await;
        let mut source = harness;
        for status in [irreversible, ToolStatus::EffectUnknown] {
            let replacement = super::recovery::harness_at(source.store.lock().await.clone()).await;
            let mut request = super::recovery::request(&*replacement.store.lock().await, false)?;
            request.tools.push(observation(&command, status));
            let (restored, executor) =
                super::recovery::restore(request, replacement.clone(), Vec::new()).await?;
            assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
            assert!(replacement.sent.lock().await.is_empty());
            if status == ToolStatus::EffectUnknown {
                let before = restored.snapshot().await.run.ok_or("run")?.active_ms;
                tokio::time::sleep(Duration::from_millis(35)).await;
                restored
                    .tool_status("observation-tick", observation(&command, status))
                    .await?;
                let after = restored.snapshot().await.run.ok_or("run")?.active_ms;
                if irreversible == ToolStatus::Running {
                    assert!(after >= before + 35, "restore lost known running time");
                } else {
                    assert_eq!(after, before, "restore charged a known stopped tool");
                }
            }
            restored.disconnect().await;
            source = replacement;
        }
        let replacement = super::recovery::harness_at(source.store.lock().await.clone()).await;
        let mut request = super::recovery::request(&*replacement.store.lock().await, false)?;
        request
            .tools
            .push(observation(&command, ToolStatus::NotStarted));
        let head = replacement.store.lock().await.head.clone();
        let restored = super::recovery::restore(request, replacement.clone(), Vec::new()).await;
        assert_eq!(
            restored
                .err()
                .and_then(|error| error
                    .downcast::<bitrouter_orchestrator::core::protocol::CoreError>()
                    .ok())
                .map(|error| error.code),
            Some(ErrorCode::OperationConflict)
        );
        assert_eq!(replacement.store.lock().await.head, head);
        assert!(replacement.sent.lock().await.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn tool_status_running_intervals_share_one_clock_and_approval_is_idle() -> TestResult {
    let mut fixture = Harness::new(None, None);
    fixture.wait_for_approval = true;
    let harness = Arc::new(fixture);
    let (session, _, _) = setup(
        vec![output(vec![call("one"), call("two")])],
        harness.clone(),
        false,
    )
    .await?;
    session.start("input", 1, input()).await?;
    session.drive().await?;
    let commands = harness.sent.lock().await.clone();
    assert_eq!(commands.len(), 2);
    for (index, command) in commands.iter().enumerate() {
        session
            .tool_status(
                &format!("approval-{index}"),
                observation(command, ToolStatus::WaitingApproval),
            )
            .await?;
    }
    let before = session.snapshot().await.run.ok_or("run")?.active_ms;
    tokio::time::sleep(Duration::from_millis(35)).await;
    session
        .tool_status(
            "approval-repeat",
            observation(&commands[0], ToolStatus::WaitingApproval),
        )
        .await?;
    assert_eq!(session.snapshot().await.run.ok_or("run")?.active_ms, before);
    let started = std::time::Instant::now();
    for (index, command) in commands.iter().enumerate() {
        assert!(harness.store.lock().await.try_start_tool(ToolStartFence {
            invocation_id: command.invocation_id.clone(),
            attempt_id: command.attempt_id.clone(),
        }));
        session
            .tool_status(
                &format!("running-{index}"),
                observation(command, ToolStatus::Running),
            )
            .await?;
    }
    tokio::time::sleep(Duration::from_millis(125)).await;
    for (index, command) in commands.iter().enumerate() {
        session
            .tool_status(
                &format!("stopped-{index}"),
                observation(command, ToolStatus::Stopped),
            )
            .await?;
    }
    let actual_interval = started.elapsed().as_millis() as u64;
    let observed = session.snapshot().await.run.ok_or("run")?.active_ms - before;
    assert!(observed >= 125, "running tools did not accrue active time");
    assert!(
        observed <= actual_interval + 10,
        "overlapping tools were double counted"
    );
    Ok(())
}
