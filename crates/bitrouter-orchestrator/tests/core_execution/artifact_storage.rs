use super::*;
use bitrouter_orchestrator::core::protocol::{ToolObservation, ToolStatus};

#[path = "artifact_storage/repeated_restore.rs"]
mod repeated_restore;
#[path = "artifact_storage/unknown_order.rs"]
mod unknown_order;

async fn evidence(
    harness: &Harness,
    id: &str,
    bytes: u64,
) -> Result<ArtifactRef, Box<dyn std::error::Error>> {
    let content = vec![b'x'; usize::try_from(bytes)?];
    let reference = ArtifactRef {
        artifact_id: id.into(),
        sha256: sha256(&content),
        bytes,
        media_type: "application/octet-stream".into(),
    };
    harness
        .store
        .lock()
        .await
        .put_artifact(reference.clone(), &content)?;
    Ok(reference)
}

fn observation(
    command: &ToolExecute,
    status: ToolStatus,
    reference: ArtifactRef,
) -> ToolObservation {
    ToolObservation {
        invocation_id: command.invocation_id.clone(),
        attempt_id: command.attempt_id.clone(),
        status,
        evidence: vec![reference],
    }
}

#[tokio::test]
async fn artifact_bodies_have_frozen_bounds_and_duplicate_references_count_once() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
    session.start("input", 1, input()).await?;
    session.drive().await?;
    let command = harness.sent.lock().await.first().ok_or("tool")?.clone();
    let limits = command.result_limits.ok_or("limits")?;
    let bound = limits.artifact_bytes.ok_or("artifact bound")?;
    assert!(bound > 0);
    let before = session.head().await;
    let mut value = result(&command);
    value.evidence = vec![evidence(&harness, "oversized", bound + 1).await?];
    let error = session
        .tool_result("oversized", value.clone())
        .await
        .err()
        .ok_or("unbounded body")?;
    assert_eq!(
        (error.code, error.commit_status),
        (ErrorCode::LimitExceeded, CommitStatus::NotCommitted)
    );
    assert_eq!(session.head().await, before);
    assert!(session.operation("oversized").await.is_none());
    let mut update = signal_update(&session, Vec::new()).await;
    update.manifest.artifact_quota_bytes *= 2;
    session.signals("larger-quota", update).await?;
    assert_eq!(
        session
            .tool_result("still-oversized", value.clone())
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::LimitExceeded)
    );
    value.evidence = vec![evidence(&harness, "bounded", bound).await?];
    value.evidence.push(value.evidence[0].clone());
    limits.validate_result(&value)?;
    let mut conflicting = value.clone();
    conflicting.evidence[1].sha256 = sha256(b"different");
    assert_eq!(
        limits
            .validate_result(&conflicting)
            .err()
            .map(|error| error.code),
        Some(ErrorCode::CheckpointConflict)
    );
    let mut overflow = value.clone();
    overflow.evidence[0].bytes = u64::MAX;
    overflow.evidence[1].artifact_id = "another".into();
    assert_eq!(
        limits
            .validate_result(&overflow)
            .err()
            .map(|error| error.code),
        Some(ErrorCode::LimitExceeded)
    );
    session.tool_result("bounded", value).await?;
    Ok(())
}

#[tokio::test]
async fn artifact_quota_reserves_essential_evidence_through_unknown_effect_recovery() -> TestResult
{
    for tool_count in [1, 2] {
        let mut fixture = Harness::new(None, None);
        fixture.wait_for_approval = true;
        let harness = Arc::new(fixture);
        let (session, executor, _) = setup(
            vec![output(
                (0..tool_count)
                    .map(|index| call(&format!("read-{index}")))
                    .collect(),
            )],
            harness.clone(),
            false,
        )
        .await?;
        let mut task = input();
        task.limits = Some(Limits {
            input_bytes: 4096,
            outstanding_tools: tool_count,
            ..Limits::default()
        });
        session.start("input", 1, task).await?;
        session.drive().await?;
        let commands = harness.sent.lock().await.clone();
        assert_eq!(commands.len(), tool_count as usize);
        let command = commands.first().ok_or("tool")?;
        let bound = command
            .result_limits
            .and_then(|limits| limits.artifact_bytes)
            .ok_or("bound")?;
        let mut rejected = false;
        for index in 0..16 {
            let id = format!("optional-{index}");
            let reference = evidence(&harness, &id, bound).await?;
            match session
                .tool_status(
                    &id,
                    observation(command, ToolStatus::WaitingApproval, reference),
                )
                .await
            {
                Ok(_) => {}
                Err(error) => {
                    assert_eq!(
                        (error.code, error.commit_status),
                        (ErrorCode::LimitExceeded, CommitStatus::NotCommitted)
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
                    // The host may reclaim staging never referenced by an accepted
                    // checkpoint. This fixture models retained-object accounting;
                    // a physical storage lease is an independent host obligation.
                    let mut store = harness.store.lock().await;
                    store.artifacts.remove(&id);
                    store.artifact_bytes.remove(&id);
                    rejected = true;
                    break;
                }
            }
        }
        assert!(rejected);
        for (index, command) in commands.iter().enumerate() {
            for (kind, status) in [
                ("running", ToolStatus::Running),
                ("stopped", ToolStatus::Stopped),
                ("unknown-status", ToolStatus::EffectUnknown),
            ] {
                let id = format!("{kind}-{index}");
                let reference = evidence(&harness, &id, bound).await?;
                session
                    .tool_status(&id, observation(command, status, reference))
                    .await?;
            }
            let mut unknown = result(command);
            unknown.status = ToolOutcome::EffectUnknown;
            let id = format!("unknown-result-{index}");
            unknown.evidence = vec![evidence(&harness, &id, bound).await?];
            session.tool_result(&id, unknown).await?;
        }
        session.disconnect().await;
        let replacement = recovery::harness_at(harness.store.lock().await.clone()).await;
        let mut outcomes = Vec::new();
        for (index, command) in commands.iter().enumerate() {
            let mut definite = result(command);
            definite.evidence =
                vec![evidence(&replacement, &format!("definite-{index}"), bound).await?];
            outcomes.push(definite);
        }
        let mut request = recovery::request(&*replacement.store.lock().await, false)?;
        request.results = outcomes.clone();
        let (restored, restored_executor) =
            recovery::restore(request, replacement.clone(), Vec::new()).await?;
        let done = restored.drive().await?;
        assert_eq!(
            done.run.as_ref().map(|run| run.status),
            Some(RunStatus::Failed)
        );
        for (call, definite) in done
            .root_turn()
            .ok_or("turn")?
            .invocations
            .iter()
            .zip(&outcomes)
        {
            assert_eq!(call.result.as_ref(), Some(definite));
        }
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(restored_executor.calls.load(Ordering::SeqCst), 0);
        let retained = replacement
            .store
            .lock()
            .await
            .artifacts
            .values()
            .map(|reference| reference.bytes)
            .sum::<u64>();
        assert!(retained <= done.manifest.artifact_quota_bytes);
    }
    Ok(())
}

#[tokio::test]
async fn recovery_archive_reservation_precedes_tool_dispatch() -> TestResult {
    for input_bytes in [65536, 4096] {
        let harness = Arc::new(Harness::new(None, None));
        let (session, executor, _) =
            setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
        let mut update = signal_update(&session, Vec::new()).await;
        update.manifest.artifact_quota_bytes = 256 * 1024;
        session.signals("quota", update).await?;
        let mut task = input();
        task.limits = Some(Limits {
            input_bytes,
            ..Limits::default()
        });
        session
            .start("input", session.head().await.state_revision, task)
            .await?;
        let outcome = session.drive().await;
        let state = session.snapshot().await;
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        if input_bytes == 65536 {
            if let Err(error) = outcome {
                assert_eq!(error.code, ErrorCode::LimitExceeded);
            }
            assert!(state.run.as_ref().ok_or("run")?.resource_error.is_some());
            assert!(harness.sent.lock().await.is_empty());
            assert!(state.root_turn().ok_or("turn")?.invocations.is_empty());
        } else {
            outcome?;
            assert_eq!(harness.sent.lock().await.len(), 1);
            let call = &state.root_turn().ok_or("turn")?.invocations[0];
            assert!(
                call.recovery_archive_allowance.ok_or("reservation")?
                    > call.dispatch.result_limits.ok_or("limits")?.payload_bytes * 2
            );
        }
    }
    Ok(())
}

async fn full_recovery_observation(
    harness: &Harness,
    command: &ToolExecute,
    status: ToolStatus,
    key: &str,
    full_body: bool,
) -> Result<ToolObservation, Box<dyn std::error::Error>> {
    let limits = command.result_limits.ok_or("limits")?;
    let bytes = vec![
        b'x';
        if full_body {
            usize::try_from(limits.artifact_bytes.ok_or("body bound")?)?
        } else {
            0
        }
    ];
    let mut value = observation(
        command,
        status,
        ArtifactRef {
            artifact_id: key.into(),
            sha256: sha256(&bytes),
            bytes: bytes.len() as u64,
            media_type: String::new(),
        },
    );
    let remaining = limits.payload_bytes - serde_json::to_vec(&value)?.len() as u64;
    value.evidence[0].media_type = "x".repeat(usize::try_from(remaining)?);
    assert_eq!(
        serde_json::to_vec(&value)?.len() as u64,
        limits.payload_bytes
    );
    harness
        .store
        .lock()
        .await
        .put_artifact(value.evidence[0].clone(), &bytes)?;
    Ok(value)
}

async fn restore_losing_ack(
    request: bitrouter_orchestrator::core::protocol::Restore,
    harness: Arc<Harness>,
    committed: bool,
) -> Result<CoreSession, Box<dyn std::error::Error>> {
    let (app, executor) = recovery::application(Vec::new())?;
    let caps = recovery::capabilities(&request.binding.grant.core_instance_id);
    let port = Arc::new(reconnect::FaultPort::new(
        harness.clone(),
        "session.restored",
        committed,
    ));
    let retained = Arc::new(Mutex::new(None));
    let registered = retained.clone();
    let error = CoreSession::restore_registered(
        request,
        &caps,
        app,
        CallerContext::local(),
        http::HeaderMap::new(),
        port.clone(),
        move |session| async move {
            *registered.lock().await = Some(session);
            Ok(())
        },
    )
    .await
    .err()
    .ok_or("restore ACK was not lost")?;
    assert_eq!(error.commit_status, CommitStatus::Unknown);
    let session = retained.lock().await.take().ok_or("registered session")?;
    let original = port
        .proposals
        .lock()
        .await
        .last()
        .cloned()
        .ok_or("proposal")?;
    reconnect::reconnect(&session, &harness).await?;
    let retransmissions = port
        .proposals
        .lock()
        .await
        .iter()
        .filter(|batch| batch.identity.batch_id == original.identity.batch_id)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(retransmissions.len(), if committed { 1 } else { 2 });
    assert!(retransmissions.iter().all(|batch| batch == &original));
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(session)
}

#[tokio::test]
async fn recovery_archive_growth_survives_artifact_saturation_and_full_observations() -> TestResult
{
    let mut fixture = Harness::new(None, None);
    fixture.wait_for_approval = true;
    let mut harness = Arc::new(fixture);
    let (mut session, executor, _) = setup(
        vec![output(vec![call("read-a"), call("read-b")])],
        harness.clone(),
        false,
    )
    .await?;
    let mut task = input();
    task.limits = Some(Limits {
        input_bytes: 4096,
        checkpoint_bytes: 128 * 1024,
        unacknowledged_bytes: 256 * 1024,
        outstanding_tools: 2,
        ..Limits::default()
    });
    session.start("input", 1, task).await?;
    session.drive().await?;
    let commands = harness.sent.lock().await.clone();
    assert_eq!(commands.len(), 2);
    // First establish a real archived history with no artifact-body pressure.
    // Repeated evidence uses fresh capacity; it cannot consume the reserved
    // first stopped/unknown observations tested below.
    let mut initial_observations = 0;
    for index in 0..32 {
        session.disconnect().await;
        let replacement = recovery::harness_at(harness.store.lock().await.clone()).await;
        let mut observations = Vec::new();
        for (tool, command) in commands.iter().enumerate() {
            observations.push(
                full_recovery_observation(
                    &replacement,
                    command,
                    ToolStatus::Running,
                    &format!("seed-{index}-{tool}"),
                    false,
                )
                .await?,
            );
        }
        let mut request = recovery::request(&*replacement.store.lock().await, false)?;
        request.tools = observations;
        let (restored, restored_executor) =
            recovery::restore(request, replacement.clone(), Vec::new()).await?;
        assert_eq!(restored_executor.calls.load(Ordering::SeqCst), 0);
        initial_observations += 1;
        session = restored;
        harness = replacement;
        if session.snapshot().await.recovery_archive.is_some() {
            break;
        }
    }
    assert!(session.snapshot().await.recovery_archive.is_some());
    let command = &commands[0];
    let bound = command
        .result_limits
        .and_then(|limits| limits.artifact_bytes)
        .ok_or("body bound")?;
    for (round, bytes) in [bound, (bound / 16).max(1)].into_iter().enumerate() {
        let mut saturated = false;
        for index in 0..256 {
            let key = format!("filler-{round}-{index}");
            let reference = evidence(&harness, &key, bytes).await?;
            match session
                .tool_status(&key, observation(command, ToolStatus::Running, reference))
                .await
            {
                Ok(_) => {}
                Err(error) => {
                    assert_eq!(error.code, ErrorCode::LimitExceeded);
                    assert!(
                        error.message.contains("artifact quota"),
                        "{}",
                        error.message
                    );
                    assert!(session.operation(&key).await.is_none());
                    let mut store = harness.store.lock().await;
                    store.artifacts.remove(&key);
                    store.artifact_bytes.remove(&key);
                    saturated = true;
                    break;
                }
            }
        }
        assert!(saturated);
    }
    for (phase, status) in [ToolStatus::Stopped, ToolStatus::EffectUnknown]
        .into_iter()
        .enumerate()
    {
        session.disconnect().await;
        let replacement = recovery::harness_at(harness.store.lock().await.clone()).await;
        let mut observations = Vec::new();
        for (index, command) in commands.iter().enumerate() {
            observations.push(
                full_recovery_observation(
                    &replacement,
                    command,
                    status,
                    &format!("recovery-{phase}-{index}"),
                    true,
                )
                .await?,
            );
        }
        let mut request = recovery::request(&*replacement.store.lock().await, false)?;
        request.tools = observations.clone();
        let restored = restore_losing_ack(request, replacement.clone(), phase == 1).await?;
        for (call, observed) in restored
            .snapshot()
            .await
            .root_turn()
            .ok_or("turn")?
            .invocations
            .iter()
            .zip(&observations)
        {
            assert_eq!(call.recovery_observation.as_ref(), Some(observed));
            assert_eq!(
                call.prior_recovery_observations.len(),
                initial_observations + phase
            );
        }
        session = restored;
        harness = replacement;
    }
    session.disconnect().await;
    let replacement = recovery::harness_at(harness.store.lock().await.clone()).await;
    let mut request = recovery::request(&*replacement.store.lock().await, false)?;
    request.results = commands.iter().map(result).collect();
    let (restored, restored_executor) =
        recovery::restore(request, replacement.clone(), Vec::new()).await?;
    assert_eq!(
        restored.drive().await?.run.map(|run| run.status),
        Some(RunStatus::Failed)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(restored_executor.calls.load(Ordering::SeqCst), 0);
    assert!(replacement.sent.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn recovery_archive_reservation_validates_restore_and_preserves_legacy() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
    session.start("input", 1, input()).await?;
    session.drive().await?;
    let command = harness.sent.lock().await.first().ok_or("tool")?.clone();
    session.disconnect().await;
    for forged in [false, true] {
        let mut store = harness.store.lock().await.clone();
        tool_payloads::rewrite_last(&mut store, |payload| {
            let call = &mut payload.checkpoint.state["agents"][&command.agent_id]["turn"]["invocations"]
                [0];
            if forged {
                call["recovery_archive_allowance"] = json!(0);
            } else {
                call.as_object_mut()
                    .map(|value| value.remove("recovery_archive_allowance"));
            }
        })?;
        let replacement = recovery::harness_at(store).await;
        let mut request = recovery::request(&*replacement.store.lock().await, false)?;
        request.results.push(result(&command));
        let head = replacement.store.lock().await.head.clone();
        let restored = recovery::restore(request, replacement.clone(), Vec::new()).await;
        if forged {
            let error = restored.err().ok_or("forged reservation accepted")?;
            assert_eq!(
                error.downcast_ref::<CoreError>().map(|error| error.code),
                Some(ErrorCode::CheckpointConflict)
            );
            assert_eq!(replacement.store.lock().await.head, head);
        } else {
            let (restored, executor) = restored?;
            let state = restored.snapshot().await;
            let call = &state.root_turn().ok_or("turn")?.invocations[0];
            assert!(call.recovery_archive_allowance.is_none());
            assert_eq!(call.result.as_ref(), Some(&result(&command)));
            assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        }
        assert!(replacement.sent.lock().await.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn artifact_legacy_result_is_not_narrowed_to_the_new_allocation() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
    session.start("input", 1, input()).await?;
    session.drive().await?;
    let command = harness.sent.lock().await.first().ok_or("tool")?.clone();
    let limits = command.result_limits.ok_or("limits")?;
    let mut value = result(&command);
    value.evidence = vec![
        evidence(
            &harness,
            "legacy-body",
            limits.artifact_bytes.ok_or("bound")? + 1,
        )
        .await?,
    ];
    session.disconnect().await;
    let mut store = harness.store.lock().await.clone();
    tool_payloads::rewrite_last(&mut store, |payload| {
        payload.checkpoint.state["agents"][&command.agent_id]["turn"]["invocations"][0]["dispatch"]
            ["result_limits"]
            .as_object_mut()
            .map(|limits| limits.remove("artifact_bytes"));
    })?;
    let replacement = recovery::harness_at(store).await;
    let mut request = recovery::request(&*replacement.store.lock().await, false)?;
    request.results.push(value.clone());
    let (restored, _) = recovery::restore(request, replacement, Vec::new()).await?;
    let state = restored.snapshot().await;
    let call = &state.root_turn().ok_or("turn")?.invocations[0];
    assert_eq!(call.result.as_ref(), Some(&value));
    assert_eq!(
        call.dispatch.result_limits.ok_or("limits")?.artifact_bytes,
        None
    );
    Ok(())
}

#[tokio::test]
async fn artifact_legacy_terminal_child_with_default_policy_survives_a_new_root() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(
        (0..8).map(|_| output(vec![text("done")])).collect(),
        harness.clone(),
        false,
    )
    .await?;
    let root = session.start("input", 1, input()).await?.assigned_ids["agent_id"].clone();
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
    let command = harness.sent.lock().await.first().ok_or("tool")?.clone();
    assert_eq!(command.agent_id, child);
    let value = result(&command);
    session.tool_result("result", value.clone()).await?;
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::Completed)
    );
    session
        .start("next", session.head().await.state_revision, input())
        .await?;
    session.disconnect().await;
    let mut store = harness.store.lock().await.clone();
    tool_payloads::rewrite_last(&mut store, |payload| {
        payload.checkpoint.state["agents"][&child]["turn"]["invocations"][0]
            ["dispatch"]["result_limits"].as_object_mut().map(|limits| limits.remove("artifact_bytes"));
    })?;
    let replacement = recovery::harness_at(store).await;
    let request = recovery::request(&*replacement.store.lock().await, false)?;
    let (restored, _) = recovery::restore(request, replacement, Vec::new()).await?;
    let snapshot = restored.snapshot().await;
    let turn = snapshot.agents[&child].turn.as_ref().ok_or("child")?;
    assert!(turn.input.limits.is_none());
    assert_eq!(turn.invocations[0].result.as_ref(), Some(&value));
    assert_eq!(
        turn.invocations[0]
            .dispatch
            .result_limits
            .ok_or("limits")?
            .artifact_bytes,
        None
    );
    Ok(())
}
