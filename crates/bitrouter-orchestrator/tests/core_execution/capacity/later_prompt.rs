//! Accepted tool history must survive rejection of a later model attempt.

use super::*;
use bitrouter_orchestrator::core::accounting::work::{CostWorkKind, CostWorkState};
use bitrouter_orchestrator::core::checkpoint::ToolStartFence;
use bitrouter_orchestrator::core::protocol::OperationReceipt;
use bitrouter_sdk::language_model::types::ToolResultOutput;

#[path = "later_prompt/expanded.rs"]
mod expanded;

struct AcceptedReply {
    result: ToolResult,
    receipt: OperationReceipt,
}

fn history_result(
    command: &ToolExecute,
    index: usize,
) -> Result<ToolResult, Box<dyn std::error::Error>> {
    let mut value = result(command);
    value.output = format!("retained tool reply {index}: ");
    let limits = command.result_limits.ok_or("limits")?;
    let remaining = (limits.payload_bytes as usize)
        .checked_sub(serde_json::to_vec(&value)?.len())
        .ok_or("reply envelope exceeds its bound")?;
    value.output.push_str(&"x".repeat(remaining));
    assert!(value.output.len() >= 1024);
    assert!(value.output.len() as u64 <= limits.output_bytes);
    assert_eq!(
        serde_json::to_vec(&value)?.len() as u64,
        limits.payload_bytes
    );
    Ok(value)
}

fn task_with_capacity(existing_tools: bool, sufficient: bool) -> TaskInput {
    let mut task = input();
    let checkpoint_bytes = if sufficient {
        1024 * 1024
    } else if existing_tools {
        512 * 1024
    } else {
        256 * 1024
    };
    task.limits = Some(Limits {
        input_bytes: 8192,
        checkpoint_bytes,
        unacknowledged_bytes: checkpoint_bytes * 2,
        ..Limits::default()
    });
    task
}

fn scripted_outputs(existing_tools: bool) -> Vec<MockResponse> {
    let mut outputs = Vec::new();
    if existing_tools {
        outputs.push(output(vec![call("running"), call("approval")]));
    }
    outputs.extend((0..3).map(|index| output(vec![call(&format!("call-{index}"))])));
    outputs.push(output(vec![text(
        "continued with the complete retained history",
    )]));
    outputs
}

async fn accept_history(
    session: &CoreSession,
    harness: &Harness,
    actor: &str,
) -> Result<Vec<AcceptedReply>, Box<dyn std::error::Error>> {
    let mut replies = Vec::new();
    for index in 0..3 {
        let state = session.drive().await?;
        assert!(state.run.as_ref().ok_or("run")?.resource_error.is_none());
        let command = harness.sent.lock().await.last().ok_or("tool")?.clone();
        assert_eq!(command.agent_id, actor);
        {
            let mut store = harness.store.lock().await;
            let identity = ToolStartFence {
                invocation_id: command.invocation_id.clone(),
                attempt_id: command.attempt_id.clone(),
            };
            if !store.started_tools.contains(&identity) {
                assert!(store.try_start_tool(identity));
            }
        }
        let result = history_result(&command, index)?;
        let receipt = session
            .tool_result(&format!("reply-{index}"), result.clone())
            .await?;
        replies.push(AcceptedReply { result, receipt });
    }
    Ok(replies)
}

fn paired_reply(state: &SessionSnapshot, actor: &str, reply: &AcceptedReply) -> TestResult {
    let agent = &state.agents[actor];
    let call = agent
        .turn
        .as_ref()
        .ok_or("turn")?
        .invocations
        .iter()
        .find(|call| call.dispatch.invocation_id == reply.result.invocation_id)
        .ok_or("retained invocation")?;
    assert_eq!(call.result.as_ref(), Some(&reply.result));
    assert!(call.consumed);
    let expected = if reply.result.status == ToolOutcome::Succeeded {
        ToolResultOutput::Text {
            value: reply.result.output.clone(),
        }
    } else {
        ToolResultOutput::ErrorText {
            value: format!("NotExecuted: {}", reply.result.output),
        }
    };
    let content = || agent.history.iter().flat_map(|message| &message.content);
    assert_eq!(
        content()
            .filter(|part| matches!(part, Content::ToolCall { id, .. }
        if id == &call.provider_call_id))
            .count(),
        1
    );
    assert_eq!(content().filter(|part| matches!(part, Content::ToolResult { call_id, tool_name, output, .. }
        if call_id == &call.provider_call_id && tool_name.as_deref() == Some(call.dispatch.tool.as_str())
            && *output == expected)).count(), 1);
    assert_eq!(
        state.operations.get(&reply.receipt.operation_id),
        Some(&reply.receipt)
    );
    Ok(())
}

fn retained_model_outcomes(before: &SessionSnapshot, after: &SessionSnapshot) -> TestResult {
    let run_id = &before.run.as_ref().ok_or("run")?.run_id;
    for (id, agent) in &before.agents {
        let Some(turn) = &agent.turn else { continue };
        let saved = after.agents[id].turn.as_ref().ok_or("retained turn")?;
        for original in &turn.steps {
            let step = saved
                .steps
                .iter()
                .find(|step| step.step_id == original.step_id)
                .ok_or("retained step")?;
            assert_eq!(
                serde_json::to_value(&step.attempts)?,
                serde_json::to_value(&original.attempts)?
            );
            for attempt in &original.attempts {
                let receipt = attempt.receipt.as_ref().ok_or("complete model receipt")?;
                let usage = receipt
                    .report
                    .result
                    .as_ref()
                    .ok_or("model result")?
                    .usage
                    .as_ref()
                    .ok_or("usage")?;
                assert_eq!((usage.prompt_tokens, usage.completion_tokens), (7, 3));
                let cost = &before.cost_work[run_id].work[&attempt.attempt_id];
                assert_eq!(
                    (cost.kind, cost.state),
                    (
                        CostWorkKind::ProviderAttempt,
                        CostWorkState::OutcomeRecorded
                    )
                );
                assert_eq!(&after.cost_work[run_id].work[&attempt.attempt_id], cost);
            }
        }
    }
    Ok(())
}

async fn drain_original_tools(
    session: &CoreSession,
    harness: &Harness,
    commands: &[ToolExecute],
) -> Result<Vec<AcceptedReply>, Box<dyn std::error::Error>> {
    tokio::time::timeout(Duration::from_secs(5), harness.cancel_seen.acquire_many(2))
        .await??
        .forget();
    assert_eq!(harness.cancelled.lock().await.len(), commands.len());
    {
        let mut store = harness.store.lock().await;
        for command in &commands[1..] {
            assert!(store.tool_start_fences.contains(&ToolStartFence {
                invocation_id: command.invocation_id.clone(),
                attempt_id: command.attempt_id.clone(),
            }));
        }
        assert!(!store.try_start_tool(ToolStartFence {
            invocation_id: commands[1].invocation_id.clone(),
            attempt_id: commands[1].attempt_id.clone(),
        }));
    }
    session
        .tool_status(
            "stopped",
            full_observation(harness, &commands[0], ToolStatus::Stopped, "stopped-proof").await?,
        )
        .await?;
    let mut replies = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        let result = full_result(
            harness,
            command,
            if index == 0 {
                ToolOutcome::Succeeded
            } else {
                ToolOutcome::NotExecuted
            },
            &format!("old-proof-{index}"),
        )
        .await?;
        let receipt = session
            .tool_result(&format!("old-result-{index}"), result.clone())
            .await?;
        replies.push(AcceptedReply { result, receipt });
    }
    Ok(replies)
}

async fn rejected_later_attempt(
    existing_tools: bool,
    kind: &'static str,
    persisted: bool,
) -> TestResult {
    let mut fixture = Harness::new(None, None);
    fixture.wait_for_approval = existing_tools;
    let harness = Arc::new(fixture);
    let port = Arc::new(reconnect::FaultPort::new(harness.clone(), kind, persisted));
    let (session, executor, settlements) =
        setup(scripted_outputs(existing_tools), port.clone(), false).await?;
    let task = task_with_capacity(existing_tools, false);
    let initial_revision = session.head().await.state_revision;
    let accepted = session
        .start("input", initial_revision, task.clone())
        .await?;
    let root = &accepted.assigned_ids["agent_id"];
    let (actor, old_commands) = if existing_tools {
        tool_intents::occupy_root_and_spawn(&session, &harness, root).await?
    } else {
        (root.clone(), Vec::new())
    };
    let replies = accept_history(&session, &harness, &actor).await?;
    let before = session.snapshot().await;
    let expected_calls = 3 + usize::from(existing_tools);
    assert_eq!(executor.calls.load(Ordering::SeqCst), expected_calls);
    assert_eq!(
        before.agents[&actor]
            .turn
            .as_ref()
            .ok_or("turn")?
            .steps
            .len(),
        3
    );
    let outcome = session.drive().await;
    assert_eq!(executor.calls.load(Ordering::SeqCst), expected_calls);
    // The SDK settles the rejected request once even though no provider
    // attempt or executor call was admitted. Replaying must not settle it again.
    assert_eq!(settlements.load(Ordering::SeqCst), expected_calls + 1);
    assert_eq!(harness.sent.lock().await.len(), old_commands.len() + 3);
    if kind == "run.capacity_reached" || !existing_tools {
        let error = outcome.err().ok_or("missing failure ACK loss")?;
        assert_eq!(
            (error.code, error.commit_status),
            (ErrorCode::CheckpointUnavailable, CommitStatus::Unknown),
            "{error:?}"
        );
        if kind == "run.capacity_reached" {
            assert!(harness.cancelled.lock().await.is_empty());
        }
        tool_intents::resolve_fault(&session, &harness, &port, kind, persisted).await?;
    } else if let Err(error) = outcome {
        assert_eq!(
            (error.code, error.commit_status),
            (ErrorCode::LimitExceeded, CommitStatus::NotCommitted)
        );
    }
    let rejected = session.snapshot().await;
    tool_intents::failed_capacity(&rejected)?;
    assert!(
        rejected
            .run
            .as_ref()
            .ok_or("run")?
            .resource_error
            .as_ref()
            .ok_or("capacity error")?
            .message
            .contains("checkpoint")
    );
    let turn = rejected.agents[&actor].turn.as_ref().ok_or("turn")?;
    assert_eq!((turn.steps.len(), turn.invocations.len()), (4, 3));
    let preparing = turn.steps.last().ok_or("later step")?;
    assert!(preparing.plan.is_some());
    assert!(preparing.attempts.is_empty());
    for reply in &replies {
        paired_reply(&rejected, &actor, reply)?;
        assert!(preparing.input_history.iter().flat_map(|message| &message.content).any(|part|
            matches!(part, Content::ToolResult { output: ToolResultOutput::Text { value }, .. }
                if value == &reply.result.output)));
    }
    retained_model_outcomes(&before, &rejected)?;
    session.drive().await?;
    let old_replies = if existing_tools {
        let replies = drain_original_tools(&session, &harness, &old_commands).await?;
        let cleanup = session.drive().await;
        if kind == "run.failed" {
            let error = cleanup.err().ok_or("missing terminal ACK loss")?;
            assert_eq!(
                (error.code, error.commit_status),
                (ErrorCode::CheckpointUnavailable, CommitStatus::Unknown)
            );
            tool_intents::resolve_fault(&session, &harness, &port, kind, persisted).await?;
        } else {
            cleanup?;
        }
        replies
    } else {
        Vec::new()
    };
    let done = session.drive().await?;
    tool_intents::failed_capacity(&done)?;
    let run = done.run.as_ref().ok_or("run")?;
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.model_attempts as usize, expected_calls);
    retained_model_outcomes(&before, &done)?;
    for reply in &replies {
        paired_reply(&done, &actor, reply)?;
    }
    for reply in &old_replies {
        paired_reply(&done, root, reply)?;
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
    assert!(session.pending_provider_evidence().await.reports.is_empty());
    let head = session.head().await;
    assert_eq!(
        session.start("input", initial_revision, task).await?,
        accepted
    );
    for reply in replies.iter().chain(&old_replies) {
        assert_eq!(
            session
                .tool_result(&reply.receipt.operation_id, reply.result.clone())
                .await?,
            reply.receipt
        );
    }
    session.drive().await?;
    assert_eq!(session.head().await, head);
    assert_eq!(executor.calls.load(Ordering::SeqCst), expected_calls);
    // The SDK settles the rejected request once even though no provider
    // attempt or executor call was admitted. Replaying must not settle it again.
    assert_eq!(settlements.load(Ordering::SeqCst), expected_calls + 1);
    assert_eq!(harness.sent.lock().await.len(), old_commands.len() + 3);
    session.release("release", head.state_revision).await?;
    Ok(())
}

#[tokio::test]
async fn later_model_admission_retains_accepted_results_across_failure_ack_loss() -> TestResult {
    for kind in ["run.capacity_reached", "run.failed"] {
        for persisted in [false, true] {
            rejected_later_attempt(false, kind, persisted)
                .await
                .map_err(|error| format!("{kind}, persisted={persisted}: {error}"))?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn later_model_admission_failure_drains_existing_full_tool_replies() -> TestResult {
    for kind in ["run.capacity_reached", "run.failed"] {
        for persisted in [false, true] {
            rejected_later_attempt(true, kind, persisted)
                .await
                .map_err(|error| format!("{kind}, persisted={persisted}: {error}"))?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn the_same_retained_history_admits_a_model_with_more_checkpoint_capacity() -> TestResult {
    for existing_tools in [false, true] {
        let mut fixture = Harness::new(None, None);
        fixture.wait_for_approval = existing_tools;
        let harness = Arc::new(fixture);
        let (session, executor, settlements) =
            setup(scripted_outputs(existing_tools), harness.clone(), false).await?;
        let accepted = session
            .start(
                "input",
                session.head().await.state_revision,
                task_with_capacity(existing_tools, true),
            )
            .await?;
        let root = &accepted.assigned_ids["agent_id"];
        let (actor, old_commands) = if existing_tools {
            tool_intents::occupy_root_and_spawn(&session, &harness, root).await?
        } else {
            (root.clone(), Vec::new())
        };
        let replies = accept_history(&session, &harness, &actor).await?;
        let done = session.drive().await?;
        assert!(done.run.as_ref().ok_or("run")?.resource_error.is_none());
        let turn = done.agents[&actor].turn.as_ref().ok_or("turn")?;
        assert_eq!(
            turn.final_answer.as_deref(),
            Some("continued with the complete retained history")
        );
        assert_eq!(turn.steps.len(), 4);
        for reply in &replies {
            paired_reply(&done, &actor, reply)?;
        }
        let prompts = executor.prompts.lock().await;
        let last = prompts.last().ok_or("accepted next model prompt")?;
        for reply in &replies {
            assert!(last.messages.iter().flat_map(|message| &message.content).any(|part|
                matches!(part, Content::ToolResult { output: ToolResultOutput::Text { value }, .. }
                    if value == &reply.result.output)));
        }
        drop(prompts);
        let expected_calls = 4 + usize::from(existing_tools);
        assert_eq!(executor.calls.load(Ordering::SeqCst), expected_calls);
        assert_eq!(settlements.load(Ordering::SeqCst), expected_calls);
        if existing_tools {
            session
                .cancel_run(
                    "cancel",
                    session.head().await.state_revision,
                    &accepted.assigned_ids["run_id"],
                )
                .await?;
            session.drive().await?;
            let old_replies = drain_original_tools(&session, &harness, &old_commands).await?;
            let cancelled = session.drive().await?;
            assert_eq!(
                cancelled.run.as_ref().ok_or("run")?.status,
                RunStatus::Cancelled
            );
            for reply in &old_replies {
                paired_reply(&cancelled, root, reply)?;
            }
        } else {
            assert_eq!(done.run.as_ref().ok_or("run")?.status, RunStatus::Completed);
        }
        assert_eq!(executor.calls.load(Ordering::SeqCst), expected_calls);
        session
            .release("release", session.head().await.state_revision)
            .await?;
    }
    Ok(())
}
