use super::*;
use bitrouter_orchestrator::core::protocol::{ClientMessage, Command, ToolObservation, ToolStatus};

fn bounded_task() -> TaskInput {
    TaskInput {
        limits: Some(Limits {
            input_bytes: 4096,
            ..Limits::default()
        }),
        ..input()
    }
}

async fn pending() -> Result<(CoreSession, Arc<Harness>, ToolExecute), Box<dyn std::error::Error>> {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
    session.start("input", 1, bounded_task()).await?;
    session.drive().await?;
    let command = harness.sent.lock().await.first().ok_or("tool")?.clone();
    Ok((session, harness, command))
}

fn oversized_observation(command: &ToolExecute) -> ToolObservation {
    ToolObservation {
        invocation_id: command.invocation_id.clone(),
        attempt_id: command.attempt_id.clone(),
        status: ToolStatus::Stopped,
        evidence: vec![ArtifactRef {
            artifact_id: "evidence".into(),
            sha256: sha256(b"evidence"),
            bytes: 8,
            media_type: "x".repeat(4096),
        }],
    }
}

#[tokio::test]
async fn tool_payload_rejects_metadata_and_escaped_output_before_acceptance() -> TestResult {
    let (session, harness, command) = pending().await?;
    let limits = command.result_limits.ok_or("frozen limits")?;
    assert_eq!(limits.output_bytes, 8192);
    assert!(limits.payload_bytes < 4096);
    let head = session.head().await;
    let mut metadata = result(&command);
    metadata.workspace_revision = Some("x".repeat(4096));
    let mut evidence = result(&command);
    evidence.evidence = oversized_observation(&command).evidence;
    let mut escaped = result(&command);
    escaped.output = "\0".repeat(800);
    for (operation, rejected) in [
        ("metadata", metadata),
        ("evidence", evidence),
        ("escaped", escaped),
    ] {
        assert!(rejected.output.len() as u64 <= limits.output_bytes);
        let error = session
            .tool_result(operation, rejected)
            .await
            .err()
            .ok_or("must reject")?;
        assert_eq!(
            (error.code, error.commit_status),
            (ErrorCode::LimitExceeded, CommitStatus::NotCommitted)
        );
        assert_eq!(session.head().await, head);
        assert!(session.operation(operation).await.is_none());
        assert!(
            session
                .snapshot()
                .await
                .root_turn()
                .ok_or("turn")?
                .invocations[0]
                .result
                .is_none()
        );
    }
    assert_eq!(harness.sent.lock().await.len(), 1);
    // Rejection does not consume the operation ID or fabricate an outcome.
    let accepted = result(&command);
    let receipt = session.tool_result("metadata", accepted.clone()).await?;
    assert_eq!(session.tool_result("metadata", accepted).await?, receipt);
    Ok(())
}

#[tokio::test]
async fn tool_payload_reply_capacity_does_not_block_a_text_only_answer() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![text("done")])], harness.clone(), false).await?;
    let mut task = input();
    task.text = "x".into();
    task.acceptance_criteria.clear();
    task.limits = Some(Limits {
        input_bytes: 1024,
        ..Limits::default()
    });
    let bytes = serde_json::to_vec(&task)?.len() as u64;
    task.limits.as_mut().ok_or("limits")?.input_bytes = bytes + 8;
    session.start("input", 1, task).await?;
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert!(harness.sent.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn tool_payload_boundary_fits_the_maximum_control_envelope() -> TestResult {
    let (session, _, command) = pending().await?;
    let limits = command.result_limits.ok_or("limits")?;
    let mut value = result(&command);
    value.output = "\0\"\\é".into();
    value.workspace_revision = Some(String::new());
    let used = serde_json::to_vec(&value)?.len();
    value.workspace_revision = Some("x".repeat(limits.payload_bytes as usize - used));
    limits.validate_result(&value)?;
    assert_eq!(
        serde_json::to_vec(&value)?.len() as u64,
        limits.payload_bytes
    );
    let maximum_grant = OwnershipGrant {
        session_id: "s".repeat(128),
        execution_epoch: u64::MAX,
        ..grant()
    };
    let wire_limits = Limits {
        input_bytes: 4096,
        ..Limits::default()
    };
    let message = ClientMessage {
        version: 1,
        session_id: maximum_grant.session_id.clone(),
        execution_epoch: u64::MAX,
        operation_id: "o".repeat(128),
        expected_state_revision: Some(u64::MAX),
        command: Command::ToolResult(value.clone()),
    };
    message.validate(&maximum_grant, &wire_limits)?;
    assert_eq!(serde_json::to_vec(&message)?.len(), 4096);
    let mut observation = oversized_observation(&command);
    observation.evidence[0].media_type.clear();
    let used = serde_json::to_vec(&observation)?.len();
    observation.evidence[0].media_type = "x".repeat(limits.payload_bytes as usize - used);
    limits.validate_observation(&observation)?;
    let status = ClientMessage {
        command: Command::ToolStatus(observation),
        ..message
    };
    status.validate(&maximum_grant, &wire_limits)?;
    assert_eq!(serde_json::to_vec(&status)?.len(), 4096);
    let mut over = value.clone();
    over.workspace_revision
        .as_mut()
        .ok_or("revision")?
        .push('x');
    assert_eq!(
        session
            .tool_result("over", over)
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::LimitExceeded)
    );
    session.tool_result("boundary", value).await?;
    Ok(())
}

#[tokio::test]
async fn tool_payload_lifecycle_uses_the_same_frozen_bound() -> TestResult {
    let (session, _, command) = pending().await?;
    let before = session.head().await;
    assert_eq!(
        session
            .tool_status("oversize", oversized_observation(&command))
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::LimitExceeded)
    );
    assert_eq!(session.head().await, before);
    let mut observed = oversized_observation(&command);
    observed.evidence.clear();
    session.tool_status("oversize", observed).await?;
    assert!(
        session
            .snapshot()
            .await
            .root_turn()
            .ok_or("turn")?
            .invocations[0]
            .result
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn tool_payload_limits_survive_manifest_changes_and_process_restoration() -> TestResult {
    let (session, harness, command) = pending().await?;
    let mut update = signal_update(&session, Vec::new()).await;
    update.manifest.max_tool_output_bytes = 1;
    session.signals("smaller-manifest", update).await?;
    session.disconnect().await;
    let replacement = recovery::harness_at(harness.store.lock().await.clone()).await;
    let mut request = recovery::request(&*replacement.store.lock().await, false)?;
    request.tools.push(ToolObservation {
        invocation_id: command.invocation_id.clone(),
        attempt_id: command.attempt_id.clone(),
        status: ToolStatus::Stopped,
        evidence: Vec::new(),
    });
    let accepted = result(&command);
    assert!(accepted.output.len() > 1);
    request.results.push(accepted.clone());
    let (restored, _) = recovery::restore(request, replacement.clone(), Vec::new()).await?;
    let state = restored.snapshot().await;
    let call = &state.root_turn().ok_or("turn")?.invocations[0];
    assert_eq!(call.dispatch.result_limits, command.result_limits);
    assert_eq!(call.result.as_ref(), Some(&accepted));
    assert!(replacement.sent.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn tool_payload_restore_rejects_oversized_result_and_observation_before_commit() -> TestResult
{
    let (session, harness, command) = pending().await?;
    session.disconnect().await;
    for observation in [false, true] {
        let replacement = recovery::harness_at(harness.store.lock().await.clone()).await;
        let mut request = recovery::request(&*replacement.store.lock().await, false)?;
        if observation {
            request.tools.push(oversized_observation(&command));
        } else {
            let mut value = result(&command);
            value.workspace_revision = Some("x".repeat(4096));
            request.results.push(value);
        }
        let before = replacement.store.lock().await.head.clone();
        let error = match recovery::restore(request, replacement.clone(), Vec::new()).await {
            Ok(_) => return Err("oversized restore was accepted".into()),
            Err(error) => error,
        };
        let error = error.downcast_ref::<CoreError>().ok_or("core error")?;
        assert_eq!(
            (error.code, error.commit_status),
            (ErrorCode::LimitExceeded, CommitStatus::NotCommitted)
        );
        assert_eq!(replacement.store.lock().await.head, before);
        assert!(replacement.sent.lock().await.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn tool_payload_verification_advertises_its_own_frozen_limits() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![text("done")])], harness.clone(), false).await?;
    let mut task = bounded_task();
    task.verification = Some(Verification {
        tool: "read".into(),
        arguments: json!({"path":"result"}),
    });
    session.start("input", 1, task).await?;
    session.drive().await?;
    let command = harness
        .sent
        .lock()
        .await
        .first()
        .ok_or("verification")?
        .clone();
    assert!(command.verification);
    let limits = command.result_limits.ok_or("limits")?;
    assert!(limits.payload_bytes < 4096);
    let mut oversized = result(&command);
    oversized.workspace_revision = Some("x".repeat(4096));
    assert_eq!(
        session
            .tool_result("large", oversized)
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::LimitExceeded)
    );
    session.tool_result("result", result(&command)).await?;
    assert_eq!(
        session.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    Ok(())
}

pub(super) fn rewrite_last(
    store: &mut DurableHarness,
    edit: impl FnOnce(&mut bitrouter_orchestrator::core::checkpoint::CheckpointPayload),
) -> TestResult {
    let batch = store.batches.last_mut().ok_or("checkpoint")?;
    let mut payload = batch.decode(&store.limits)?;
    edit(&mut payload);
    *batch = CheckpointBatch::encode(&payload, &store.limits)?;
    store.head.payload_sha256 = Some(batch.payload_sha256.clone());
    store
        .acknowledgements
        .get_mut(&batch.identity.batch_id)
        .ok_or("ack")?
        .payload_sha256 = batch.payload_sha256.clone();
    Ok(())
}

#[tokio::test]
async fn tool_payload_legacy_intent_is_upgraded_before_restored_dispatch() -> TestResult {
    let (session, harness, command) = pending().await?;
    session.disconnect().await;
    let mut store = recovery::prefix(&*harness.store.lock().await, "model.output.applied")?;
    store.started_tools.clear();
    rewrite_last(&mut store, |payload| {
        let state = &mut payload.checkpoint.state;
        state["agents"][&command.agent_id]["turn"]["invocations"][0]["dispatch"]["result_limits"] =
            serde_json::Value::Null;
    })?;
    let replacement = recovery::harness_at(store).await;
    let mut request = recovery::request(&*replacement.store.lock().await, false)?;
    request.tools.push(ToolObservation {
        invocation_id: command.invocation_id.clone(),
        attempt_id: command.attempt_id.clone(),
        status: ToolStatus::NotStarted,
        evidence: Vec::new(),
    });
    let (restored, _) = recovery::restore(request, replacement.clone(), Vec::new()).await?;
    restored.drive().await?;
    let sent = replacement
        .sent
        .lock()
        .await
        .first()
        .ok_or("dispatch")?
        .clone();
    let mut legacy_limits = command.result_limits.ok_or("limits")?;
    legacy_limits.artifact_bytes = None;
    assert_eq!(sent.result_limits, Some(legacy_limits));
    assert_eq!(sent.invocation_id, command.invocation_id);
    assert_eq!(
        restored
            .snapshot()
            .await
            .root_turn()
            .ok_or("turn")?
            .invocations[0]
            .dispatch,
        sent
    );
    Ok(())
}

#[tokio::test]
async fn tool_payload_legacy_child_retains_limits_across_root_replacement() -> TestResult {
    for replace_before_restore in [false, true] {
        let harness = Arc::new(Harness::new(None, None));
        let (session, executor, _) = setup(
            (0..8).map(|_| output(vec![text("done")])).collect(),
            harness.clone(),
            false,
        )
        .await?;
        let root = session
            .start("input", 1, bounded_task())
            .await?
            .assigned_ids["agent_id"]
            .clone();
        let child = session
            .collaborate(
                "spawn",
                session.head().await.state_revision,
                &root,
                Action::Spawn {
                    task: work("read a file"),
                },
            )
            .await?
            .assigned_ids["agent_id"]
            .clone();
        executor
            .agent_once
            .lock()
            .await
            .insert(child.clone(), vec![call("child-read")]);
        session.drive().await?;
        let command = harness
            .sent
            .lock()
            .await
            .first()
            .ok_or("child tool")?
            .clone();
        assert_eq!(command.agent_id, child);
        session.tool_result("result", result(&command)).await?;
        assert_eq!(
            session.drive().await?.run.map(|run| run.status),
            Some(RunStatus::Completed)
        );
        if replace_before_restore {
            session
                .start("next", session.head().await.state_revision, input())
                .await?;
        }
        session.disconnect().await;
        let mut store = harness.store.lock().await.clone();
        rewrite_last(&mut store, |payload| {
            let state = &mut payload.checkpoint.state;
            state["agents"][&child]["turn"]["invocations"][0]["dispatch"]["result_limits"] =
                serde_json::Value::Null;
        })?;
        if replace_before_restore {
            let mut missing = store.clone();
            rewrite_last(&mut missing, |payload| {
                let state = &mut payload.checkpoint.state;
                state["agents"][&child]["turn"]["input"]["limits"] = serde_json::Value::Null;
            })?;
            let unavailable = recovery::harness_at(missing).await;
            let request = recovery::request(&*unavailable.store.lock().await, false)?;
            let error = recovery::restore(request, unavailable, Vec::new())
                .await
                .err()
                .ok_or("missing legacy policy accepted")?;
            assert_eq!(
                error.downcast_ref::<CoreError>().ok_or("core error")?.code,
                ErrorCode::RecoveryRequired
            );
        }
        let replacement = recovery::harness_at(store).await;
        let request = recovery::request(&*replacement.store.lock().await, false)?;
        let (restored, _) = recovery::restore(request, replacement, Vec::new()).await?;
        if !replace_before_restore {
            restored
                .start("next", restored.head().await.state_revision, input())
                .await?;
        }
        let snapshot = restored.snapshot().await;
        let frozen = snapshot.agents[&child]
            .turn
            .as_ref()
            .ok_or("child")?
            .invocations[0]
            .dispatch
            .result_limits;
        let mut legacy_limits = command.result_limits.ok_or("limits")?;
        legacy_limits.artifact_bytes = None;
        assert_eq!(frozen, Some(legacy_limits));
        let before = restored.head().await;
        assert_eq!(
            restored
                .tool_status("large-late", oversized_observation(&command))
                .await
                .err()
                .map(|error| error.code),
            Some(ErrorCode::LimitExceeded)
        );
        assert_eq!(restored.head().await, before);
        let mut stopped = oversized_observation(&command);
        stopped.evidence.clear();
        restored.tool_status("small-late", stopped).await?;
    }
    Ok(())
}

#[tokio::test]
async fn tool_payload_restore_checks_frozen_limits_and_legacy_evidence() -> TestResult {
    let (session, harness, command) = pending().await?;
    session.tool_result("result", result(&command)).await?;
    session.disconnect().await;
    let mut legacy_result = result(&command);
    legacy_result.output = "\0".repeat(800);
    let legacy_digest = sha256(&serde_json::to_vec(&legacy_result)?);
    for case in ["too_small", "too_large", "legacy_large"] {
        let mut store = harness.store.lock().await.clone();
        rewrite_last(&mut store, |payload| {
            let state = &mut payload.checkpoint.state;
            let call = &mut state["agents"][&command.agent_id]["turn"]["invocations"][0];
            if case == "legacy_large" {
                call["dispatch"]["result_limits"] = serde_json::Value::Null;
                call["result"] = json!(legacy_result);
                state["operations"]["result"]["request_sha256"] = json!(legacy_digest);
                payload.events[0].payload = json!(legacy_result);
            } else {
                call["result"] = serde_json::Value::Null;
                call["dispatch"]["result_limits"]["payload_bytes"] =
                    json!(if case == "too_small" { 1 } else { 65536 });
            }
        })?;
        let replacement = recovery::harness_at(store).await;
        let request = recovery::request(&*replacement.store.lock().await, false)?;
        let before = replacement.store.lock().await.head.clone();
        let error = recovery::restore(request, replacement.clone(), Vec::new())
            .await
            .err()
            .ok_or("invalid snapshot accepted")?;
        let error = error.downcast_ref::<CoreError>().ok_or("core error")?;
        assert_eq!(
            error.code,
            if case == "legacy_large" {
                ErrorCode::LimitExceeded
            } else {
                ErrorCode::CheckpointConflict
            }
        );
        assert_eq!(error.commit_status, CommitStatus::NotCommitted);
        assert_eq!(replacement.store.lock().await.head, before);
        assert!(replacement.sent.lock().await.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn tool_payload_core_denials_fit_tiny_output_limits_and_restore_again() -> TestResult {
    for workspace_changed in [false, true] {
        let harness = Arc::new(Harness::new(None, None));
        let (session, _, _) =
            setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
        let mut update = signal_update(&session, Vec::new()).await;
        update.manifest.max_tool_output_bytes = 1;
        session.signals("small-output", update).await?;
        session
            .start("input", session.head().await.state_revision, bounded_task())
            .await?;
        session.drive().await?;
        let command = harness.sent.lock().await.first().ok_or("tool")?.clone();
        session.disconnect().await;
        let mut store = recovery::prefix(&*harness.store.lock().await, "model.output.applied")?;
        store.started_tools.clear();
        let replacement = recovery::harness_at(store).await;
        let mut request = recovery::request(&*replacement.store.lock().await, false)?;
        if workspace_changed {
            request.binding.manifest.workspace_revision = Some("changed".into());
        } else {
            request.binding.manifest.permission_revision += 1;
        }
        request.tools.push(ToolObservation {
            invocation_id: command.invocation_id.clone(),
            attempt_id: command.attempt_id.clone(),
            status: ToolStatus::NotStarted,
            evidence: Vec::new(),
        });
        let (restored, _) = recovery::restore(
            request,
            replacement.clone(),
            vec![output(vec![text("done")])],
        )
        .await?;
        let done = restored.drive().await?;
        assert_eq!(
            done.run.as_ref().map(|run| run.status),
            Some(RunStatus::Completed)
        );
        let outcome = done.root_turn().ok_or("turn")?.invocations[0]
            .result
            .as_ref()
            .ok_or("denied")?;
        assert_eq!(outcome.status, ToolOutcome::Denied);
        assert!(outcome.output.is_empty());
        command
            .result_limits
            .ok_or("limits")?
            .validate_result(outcome)?;
        assert!(replacement.sent.lock().await.is_empty());
        restored.disconnect().await;
        let next = recovery::harness_at(replacement.store.lock().await.clone()).await;
        let request = recovery::request(&*next.store.lock().await, false)?;
        recovery::restore(request, next, Vec::new()).await?;
    }
    Ok(())
}
