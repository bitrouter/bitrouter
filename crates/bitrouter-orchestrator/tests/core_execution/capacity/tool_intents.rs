//! A committed model result must survive rejection of its new tool contracts.

use super::*;
use bitrouter_ai::types::ToolResultOutput;
use bitrouter_orchestrator::core::accounting::work::{CostWorkKind, CostWorkState};
use bitrouter_orchestrator::core::checkpoint::ToolStartFence;

pub(super) fn failed_capacity(state: &SessionSnapshot) -> TestResult {
    let run = state.run.as_ref().ok_or("run")?;
    assert_eq!(
        run.resource_constraint,
        Some(ResourceConstraint::CheckpointCapacity)
    );
    let error = run.resource_error.as_ref().ok_or("capacity failure")?;
    assert_eq!(
        (error.code, error.commit_status),
        (ErrorCode::LimitExceeded, CommitStatus::Committed)
    );
    assert!(run.active_ms < run.limits.active_seconds * 1000);
    assert!(state.root_queue.paused);
    Ok(())
}

pub(super) async fn resolve_fault(
    session: &CoreSession,
    harness: &Harness,
    port: &reconnect::FaultPort,
    kind: &str,
    persisted: bool,
) -> TestResult {
    let proposal = port
        .proposals
        .lock()
        .await
        .last()
        .ok_or("proposal")?
        .clone();
    let payload = proposal.decode(&Limits::default())?;
    assert!(payload.events.iter().any(|event| event.kind == kind));
    assert_eq!(
        session.head().await.state_revision,
        payload.base_state_revision
    );
    assert_eq!(
        harness.store.lock().await.head.state_revision,
        payload.base_state_revision + u64::from(persisted)
    );
    let blocked = session
        .drive()
        .await
        .err()
        .ok_or("lost ACK reopened dispatch")?;
    assert_eq!(
        (blocked.code, blocked.commit_status),
        (ErrorCode::CheckpointUnavailable, CommitStatus::Unknown),
        "{blocked:?}"
    );
    reconnect::reconnect(session, harness).await?;
    let copies = port
        .proposals
        .lock()
        .await
        .iter()
        .filter(|batch| batch.identity.batch_id == proposal.identity.batch_id)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(copies.len(), if persisted { 1 } else { 2 });
    assert!(copies.iter().all(|batch| *batch == proposal));
    assert_eq!(
        harness
            .store
            .lock()
            .await
            .batches
            .iter()
            .filter(|batch| batch.identity.batch_id == proposal.identity.batch_id)
            .count(),
        1
    );
    Ok(())
}

pub(super) async fn occupy_root_and_spawn(
    session: &CoreSession,
    harness: &Harness,
    root: &str,
) -> Result<(String, Vec<ToolExecute>), Box<dyn std::error::Error>> {
    session.drive().await?;
    let commands = harness.sent.lock().await.clone();
    assert_eq!(commands.len(), 2);
    assert!(harness.store.lock().await.try_start_tool(ToolStartFence {
        invocation_id: commands[0].invocation_id.clone(),
        attempt_id: commands[0].attempt_id.clone(),
    }));
    for (operation, command, status) in [
        ("running", &commands[0], ToolStatus::Running),
        ("approval", &commands[1], ToolStatus::WaitingApproval),
    ] {
        session
            .tool_status(
                operation,
                ToolObservation {
                    invocation_id: command.invocation_id.clone(),
                    attempt_id: command.attempt_id.clone(),
                    status,
                    evidence: Vec::new(),
                },
            )
            .await?;
    }
    let child = session
        .collaborate(
            "spawn",
            session.head().await.state_revision,
            root,
            Action::Spawn {
                task: work("request four workspace tools"),
            },
        )
        .await?
        .assigned_ids["agent_id"]
        .clone();
    Ok((child, commands))
}

async fn rejected_tool_batch(
    existing_tools: bool,
    kind: &'static str,
    persisted: bool,
) -> TestResult {
    let mut fixture = Harness::new(None, None);
    fixture.wait_for_approval = true;
    let harness = Arc::new(fixture);
    let port = Arc::new(reconnect::FaultPort::new(harness.clone(), kind, persisted));
    let calls = if existing_tools { 4 } else { 1 };
    let rejected_content = (0..calls)
        .map(|index| call(&format!("rejected-{index}")))
        .collect::<Vec<_>>();
    let mut outputs = Vec::new();
    if existing_tools {
        outputs.push(output(vec![call("running"), call("approval")]));
    }
    outputs.push(output(rejected_content.clone()));
    let (session, executor, settlements) = setup(outputs, port.clone(), false).await?;
    let mut storage = signal_update(&session, Vec::new()).await;
    storage.manifest.artifact_quota_bytes = if existing_tools {
        3 * 1024 * 1024
    } else {
        256 * 1024
    };
    session.signals("storage", storage).await?;
    let initial_revision = session.head().await.state_revision;
    let task = input();
    let accepted = session
        .start("input", initial_revision, task.clone())
        .await?;
    let root = accepted.assigned_ids["agent_id"].clone();
    let (actor, commands) = if existing_tools {
        occupy_root_and_spawn(&session, &harness, &root).await?
    } else {
        (root.clone(), Vec::new())
    };
    // Six total workspace calls in the larger case fit the default count of
    // eight. The independent artifact quota rejects the new reply/archive
    // contracts only after the complete model outcome has been acknowledged.
    let outcome = session.drive().await;
    let expected_calls = if existing_tools { 2 } else { 1 };
    assert_eq!(executor.calls.load(Ordering::SeqCst), expected_calls);
    assert_eq!(settlements.load(Ordering::SeqCst), expected_calls);
    assert_eq!(harness.sent.lock().await.len(), commands.len());
    let outcome_batch = port
        .proposals
        .lock()
        .await
        .iter()
        .find(|batch| {
            batch.decode(&Limits::default()).is_ok_and(|payload| {
                payload.events.iter().any(|event| {
                    event.kind == "model.attempt.outcome"
                        && event.agent_id.as_deref() == Some(actor.as_str())
                })
            })
        })
        .cloned()
        .ok_or("acknowledged provider outcome")?;
    assert!(harness.store.lock().await.batches.contains(&outcome_batch));
    let original: SessionSnapshot =
        serde_json::from_value(outcome_batch.decode(&Limits::default())?.checkpoint.state)?;
    let original_step = original.agents[&actor]
        .turn
        .as_ref()
        .ok_or("turn")?
        .steps
        .last()
        .ok_or("step")?;
    let attempt = original_step.attempts.last().ok_or("attempt")?;
    let receipt = attempt.receipt.as_ref().ok_or("complete receipt")?;
    assert_eq!(
        receipt
            .report
            .result
            .as_ref()
            .ok_or("complete output")?
            .content,
        rejected_content
    );
    assert_eq!(
        receipt
            .report
            .result
            .as_ref()
            .ok_or("complete output")?
            .usage
            .as_ref()
            .ok_or("usage")?
            .prompt_tokens,
        7
    );
    let run_id = &accepted.assigned_ids["run_id"];
    let original_cost = &original.cost_work[run_id].work[&attempt.attempt_id];
    assert_eq!(
        (original_cost.kind, original_cost.state),
        (
            CostWorkKind::ProviderAttempt,
            CostWorkState::OutcomeRecorded
        )
    );
    if kind == "run.capacity_reached" || !existing_tools {
        let error = outcome.err().ok_or("missing failure ACK loss")?;
        assert_eq!(
            (error.code, error.commit_status),
            (ErrorCode::CheckpointUnavailable, CommitStatus::Unknown),
            "{kind}/{persisted}: {error:?}"
        );
        if kind == "run.capacity_reached" {
            assert!(harness.cancelled.lock().await.is_empty());
        }
        resolve_fault(&session, &harness, &port, kind, persisted).await?;
    } else if let Err(error) = outcome {
        assert_eq!(
            (error.code, error.commit_status),
            (ErrorCode::LimitExceeded, CommitStatus::NotCommitted)
        );
    }
    failed_capacity(&session.snapshot().await)?;
    session.drive().await?;
    let rejected = session.snapshot().await;
    assert!(
        rejected.agents[&actor]
            .turn
            .as_ref()
            .ok_or("turn")?
            .invocations
            .is_empty()
    );
    assert!(
        !rejected.agents[&actor]
            .history
            .iter()
            .flat_map(|message| &message.content)
            .any(
                |part| matches!(part, Content::ToolCall { id, .. } if id.starts_with("rejected-"))
            )
    );
    let mut results = Vec::new();
    if existing_tools {
        tokio::time::timeout(Duration::from_secs(5), harness.cancel_seen.acquire_many(2))
            .await??
            .forget();
        assert_eq!(harness.cancelled.lock().await.len(), commands.len());
        {
            let mut store = harness.store.lock().await;
            for command in &commands {
                let fence = ToolStartFence {
                    invocation_id: command.invocation_id.clone(),
                    attempt_id: command.attempt_id.clone(),
                };
                assert!(store.tool_start_fences.contains(&fence));
            }
            assert!(!store.try_start_tool(ToolStartFence {
                invocation_id: commands[1].invocation_id.clone(),
                attempt_id: commands[1].attempt_id.clone(),
            }));
        }
        session
            .tool_status(
                "stopped",
                full_observation(&harness, &commands[0], ToolStatus::Stopped, "stopped-proof")
                    .await?,
            )
            .await?;
        for (index, command) in commands.iter().enumerate() {
            let result = full_result(
                &harness,
                command,
                if index == 0 {
                    ToolOutcome::Succeeded
                } else {
                    ToolOutcome::NotExecuted
                },
                &format!("result-proof-{index}"),
            )
            .await?;
            session
                .tool_result(&format!("result-{index}"), result.clone())
                .await?;
            results.push(result);
        }
        let cleanup = session.drive().await;
        if kind == "run.failed" {
            let error = cleanup.err().ok_or("missing terminal ACK loss")?;
            assert_eq!(
                (error.code, error.commit_status),
                (ErrorCode::CheckpointUnavailable, CommitStatus::Unknown)
            );
            resolve_fault(&session, &harness, &port, kind, persisted).await?;
        } else {
            cleanup?;
        }
    }
    let done = session.drive().await?;
    failed_capacity(&done)?;
    assert_eq!(done.run.as_ref().ok_or("run")?.status, RunStatus::Failed);
    assert_eq!(
        done.run.as_ref().ok_or("run")?.model_attempts as usize,
        expected_calls
    );
    let saved = done.agents[&actor]
        .turn
        .as_ref()
        .ok_or("turn")?
        .steps
        .last()
        .ok_or("step")?;
    assert_eq!(saved.step_id, original_step.step_id);
    assert_eq!(
        serde_json::to_value(&saved.attempts[0].receipt)?,
        serde_json::to_value(Some(receipt))?
    );
    assert_eq!(
        &done.cost_work[run_id].work[&attempt.attempt_id],
        original_cost
    );
    for (command, result) in commands.iter().zip(&results) {
        let call = done.agents[&root]
            .turn
            .as_ref()
            .ok_or("root turn")?
            .invocations
            .iter()
            .find(|call| call.dispatch.invocation_id == command.invocation_id)
            .ok_or("old tool")?;
        assert_eq!(call.result.as_ref(), Some(result));
        assert!(call.consumed);
        let expected = if result.status == ToolOutcome::Succeeded {
            ToolResultOutput::Text {
                value: result.output.clone(),
            }
        } else {
            ToolResultOutput::ErrorText {
                value: format!("NotExecuted: {}", result.output),
            }
        };
        assert_eq!(done.agents[&root].history.iter().flat_map(|message| &message.content)
            .filter(|part| matches!(part, Content::ToolResult { call_id, tool_name, output, .. }
                if call_id == &call.provider_call_id && tool_name.as_deref() == Some(command.tool.as_str()) && *output == expected)).count(), 1);
    }
    let kinds = harness.committed_kinds().await?;
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| *kind == "run.capacity_reached")
            .count(),
        1
    );
    assert_eq!(kinds.iter().filter(|kind| *kind == "run.failed").count(), 1);
    assert_eq!(executor.calls.load(Ordering::SeqCst), expected_calls);
    assert_eq!(settlements.load(Ordering::SeqCst), expected_calls);
    assert_eq!(harness.sent.lock().await.len(), commands.len());
    assert!(session.pending_provider_evidence().await.reports.is_empty());
    let head = session.head().await;
    assert_eq!(
        session.start("input", initial_revision, task).await?,
        accepted
    );
    session.drive().await?;
    assert_eq!(session.head().await, head);
    session.release("release", head.state_revision).await?;
    Ok(())
}

#[tokio::test]
async fn new_tool_batch_overflow_preserves_complete_output_across_failure_ack_loss() -> TestResult {
    for kind in ["run.capacity_reached", "run.failed"] {
        for persisted in [false, true] {
            rejected_tool_batch(false, kind, persisted)
                .await
                .map_err(|error| format!("{kind}, persisted={persisted}: {error}"))?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn new_tool_batch_overflow_drains_existing_full_reply_payloads() -> TestResult {
    for kind in ["run.capacity_reached", "run.failed"] {
        for persisted in [false, true] {
            rejected_tool_batch(true, kind, persisted)
                .await
                .map_err(|error| format!("{kind}, persisted={persisted}: {error}"))?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn the_same_pending_tool_batch_fits_with_sufficient_artifact_quota() -> TestResult {
    let mut fixture = Harness::new(None, None);
    fixture.wait_for_approval = true;
    let harness = Arc::new(fixture);
    let (session, executor, _) = setup(
        vec![
            output(vec![call("running"), call("approval")]),
            output(
                (0..4)
                    .map(|index| call(&format!("rejected-{index}")))
                    .collect(),
            ),
        ],
        harness.clone(),
        false,
    )
    .await?;
    let mut storage = signal_update(&session, Vec::new()).await;
    storage.manifest.artifact_quota_bytes = 32 * 1024 * 1024;
    session.signals("storage", storage).await?;
    let accepted = session
        .start("input", session.head().await.state_revision, input())
        .await?;
    let root = &accepted.assigned_ids["agent_id"];
    let (child, _) = occupy_root_and_spawn(&session, &harness, root).await?;
    let waiting = session.drive().await?;
    assert!(waiting.run.as_ref().ok_or("run")?.resource_error.is_none());
    assert_eq!(waiting.run.as_ref().ok_or("run")?.limits, Limits::default());
    assert_eq!(
        waiting.agents[&child]
            .turn
            .as_ref()
            .ok_or("child")?
            .invocations
            .len(),
        4
    );
    let commands = harness.sent.lock().await.clone();
    assert_eq!(commands.len(), 6);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    session
        .cancel_run(
            "cancel",
            session.head().await.state_revision,
            &accepted.assigned_ids["run_id"],
        )
        .await?;
    session.drive().await?;
    session
        .tool_status(
            "stopped",
            ToolObservation {
                invocation_id: commands[0].invocation_id.clone(),
                attempt_id: commands[0].attempt_id.clone(),
                status: ToolStatus::Stopped,
                evidence: Vec::new(),
            },
        )
        .await?;
    for (index, command) in commands.iter().enumerate() {
        let mut outcome = result(command);
        if index != 0 {
            outcome.status = ToolOutcome::NotExecuted;
            outcome.output.clear();
        }
        session
            .tool_result(&format!("result-{index}"), outcome)
            .await?;
    }
    assert_eq!(
        session.drive().await?.run.as_ref().ok_or("run")?.status,
        RunStatus::Cancelled
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    session
        .release("release", session.head().await.state_revision)
        .await?;
    Ok(())
}

#[tokio::test]
async fn disconnected_driver_without_pending_checkpoint_is_not_unknown() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(Vec::new(), harness, false).await?;
    session
        .start("input", session.head().await.state_revision, input())
        .await?;
    session.disconnect().await;
    let error = session
        .drive()
        .await
        .err()
        .ok_or("disconnected driver progressed")?;
    assert_eq!(
        (error.code, error.commit_status),
        (ErrorCode::CheckpointUnavailable, CommitStatus::NotCommitted)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn response_scheduler_preserves_pending_capacity_failure_uncertainty() -> TestResult {
    for persisted in [false, true] {
        let harness = Arc::new(Harness::new(None, None));
        let port = Arc::new(reconnect::FaultPort::new(
            harness.clone(),
            "run.capacity_reached",
            persisted,
        ));
        let (session, executor, _) =
            setup(vec![output(vec![call("read")])], port.clone(), false).await?;
        let mut storage = signal_update(&session, Vec::new()).await;
        storage.manifest.artifact_quota_bytes = 256 * 1024;
        session.signals("storage", storage).await?;
        let accepted = session
            .start_response("input", session.head().await.state_revision, input())
            .await?;
        let response_id = &accepted.assigned_ids["response_id"];
        for _ in 0..2 {
            let error = session
                .drive_response(response_id)
                .await
                .err()
                .ok_or("pending ACK accepted")?;
            assert_eq!(
                (error.code, error.commit_status),
                (ErrorCode::CheckpointUnavailable, CommitStatus::Unknown)
            );
        }
        resolve_fault(&session, &harness, &port, "run.capacity_reached", persisted).await?;
        let completed = session.drive_response(response_id).await?;
        assert_eq!(completed.run_status, Some(RunStatus::Failed));
        assert!(completed.completed_state_revision.is_some());
        assert!(completed.pending.is_empty() && completed.output.is_empty());
        let head = session.head().await;
        assert_eq!(session.drive_response(response_id).await?, completed);
        assert_eq!(session.head().await, head);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert!(harness.sent.lock().await.is_empty());
        session.release("release", head.state_revision).await?;
    }
    Ok(())
}
