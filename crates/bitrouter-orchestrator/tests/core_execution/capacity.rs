use super::*;
use bitrouter_orchestrator::core::protocol::{ToolObservation, ToolStatus};
use bitrouter_orchestrator::core::session::ResourceConstraint;

#[path = "capacity/tree_limits.rs"]
mod tree_limits;

#[path = "capacity/tool_intents.rs"]
mod tool_intents;

#[path = "capacity/later_prompt.rs"]
mod later_prompt;

struct StopDuringArchiveRead {
    harness: Arc<Harness>,
    command: ToolExecute,
    stopped: Mutex<Option<std::time::Instant>>,
}

#[async_trait]
impl HarnessPort for StopDuringArchiveRead {
    async fn read_artifact(
        &self,
        reference: &ArtifactRef,
        offset: u64,
        max_bytes: u64,
    ) -> Result<Vec<u8>, CoreError> {
        let first = {
            let mut stopped = self.stopped.lock().await;
            if stopped.is_none() {
                *stopped = Some(std::time::Instant::now());
                true
            } else {
                false
            }
        };
        if first {
            tokio::time::sleep(Duration::from_millis(1100)).await;
        }
        self.harness
            .read_artifact(reference, offset, max_bytes)
            .await
    }

    async fn commit(&self, batch: CheckpointBatch) -> Result<CheckpointAck, CoreError> {
        self.harness.commit(batch).await
    }

    async fn send(&self, message: ServerMessage) -> Result<(), CoreError> {
        self.harness.send(message).await
    }

    async fn observe_restoration(
        &self,
        observer: bitrouter_orchestrator::core::session::restoration_activity::RestorationActivity,
    ) -> Result<(), CoreError> {
        let at = self.stopped.lock().await.ok_or_else(|| {
            CoreError::rejected(ErrorCode::RecoveryRequired, "fixture stop was not captured")
        })?;
        assert!(at >= observer.started_at().await?);
        observer
            .stopped(&self.command.invocation_id, &self.command.attempt_id, at)
            .await?;
        self.harness.observe_restoration(observer).await
    }

    async fn synchronize_restoration(
        &self,
        observer: bitrouter_orchestrator::core::session::restoration_activity::RestorationActivity,
    ) -> Result<(), CoreError> {
        self.harness.synchronize_restoration(observer).await
    }
}

async fn fill_optional_observations(
    session: &CoreSession,
    command: &ToolExecute,
    phase: ToolStatus,
) -> Result<CoreError, Box<dyn std::error::Error>> {
    for index in 0..512 {
        let before = session.head().await;
        let operation = format!("filler-{index:03}-{}", "x".repeat(100));
        let status = ToolObservation {
            invocation_id: command.invocation_id.clone(),
            attempt_id: command.attempt_id.clone(),
            status: phase,
            evidence: Vec::new(),
        };
        match session.tool_status(&operation, status).await {
            Ok(_) => {}
            Err(error) => {
                assert_eq!(
                    (error.code, error.commit_status),
                    (ErrorCode::LimitExceeded, CommitStatus::NotCommitted)
                );
                assert_eq!(
                    session.head().await.state_revision,
                    before.state_revision + 1
                );
                let state = session.snapshot().await;
                let run = state.run.as_ref().ok_or("run")?;
                assert_eq!(
                    run.resource_constraint,
                    Some(ResourceConstraint::CheckpointCapacity)
                );
                assert!(run.resource_error.is_some());
                assert!(run.active_ms < run.limits.active_seconds * 1000);
                assert!(state.root_queue.paused);
                assert!(session.operation(&operation).await.is_none());
                return Ok(error);
            }
        }
    }
    Err("fixture did not reach cleanup capacity".into())
}

async fn full_observation(
    harness: &Harness,
    command: &ToolExecute,
    status: ToolStatus,
    key: &str,
) -> Result<ToolObservation, Box<dyn std::error::Error>> {
    let mut value = ToolObservation {
        invocation_id: command.invocation_id.clone(),
        attempt_id: command.attempt_id.clone(),
        status,
        evidence: vec![ArtifactRef {
            artifact_id: key.into(),
            sha256: sha256(b"proof"),
            bytes: 5,
            media_type: String::new(),
        }],
    };
    let limits = command.result_limits.ok_or("limits")?;
    let used = serde_json::to_vec(&value)?.len();
    value.evidence[0].media_type = "x".repeat(limits.payload_bytes as usize - used);
    assert_eq!(
        serde_json::to_vec(&value)?.len() as u64,
        limits.payload_bytes
    );
    harness
        .store
        .lock()
        .await
        .put_artifact(value.evidence[0].clone(), b"proof")?;
    Ok(value)
}

async fn full_result(
    harness: &Harness,
    command: &ToolExecute,
    status: ToolOutcome,
    key: &str,
) -> Result<ToolResult, Box<dyn std::error::Error>> {
    let mut value = result(command);
    value.status = status;
    value.output = "\0\"é".repeat(100);
    value.workspace_revision = Some(String::new());
    value.evidence = vec![ArtifactRef {
        artifact_id: key.into(),
        sha256: sha256(b"proof"),
        bytes: 5,
        media_type: "text/plain".into(),
    }];
    let limits = command.result_limits.ok_or("limits")?;
    let remaining = limits.payload_bytes as usize - serde_json::to_vec(&value)?.len();
    value.workspace_revision = Some(format!(
        "{}{}",
        "\"".repeat(remaining / 2),
        "x".repeat(remaining % 2)
    ));
    assert_eq!(
        serde_json::to_vec(&value)?.len() as u64,
        limits.payload_bytes
    );
    harness
        .store
        .lock()
        .await
        .put_artifact(value.evidence[0].clone(), b"proof")?;
    Ok(value)
}

#[tokio::test]
async fn cleanup_capacity_survives_saturation_full_tool_outcomes_and_release() -> TestResult {
    for (checkpoint, wire) in [(96 * 1024, 192 * 1024), (192 * 1024, 192 * 1024)] {
        for unknown in [false, true] {
            let mut fixture = Harness::new(None, None);
            fixture.wait_for_approval = true;
            let harness = Arc::new(fixture);
            let (session, executor, _) =
                setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
            let mut task = input();
            task.limits = Some(Limits {
                input_bytes: 4096,
                checkpoint_bytes: checkpoint,
                unacknowledged_bytes: wire,
                ..Limits::default()
            });
            let receipt = session.start("input", 1, task).await?;
            session.drive().await?;
            let command = harness.sent.lock().await.first().ok_or("tool")?.clone();
            let error =
                fill_optional_observations(&session, &command, ToolStatus::WaitingApproval).await?;
            assert_eq!(error.message.contains("wire"), checkpoint == wire);
            session
                .tool_status(
                    "running",
                    full_observation(&harness, &command, ToolStatus::Running, "running-proof")
                        .await?,
                )
                .await?;
            session
                .tool_status(
                    "stopped",
                    full_observation(&harness, &command, ToolStatus::Stopped, "stopped-proof")
                        .await?,
                )
                .await?;
            session
                .cancel_run(
                    &"c".repeat(128),
                    session.head().await.state_revision,
                    &receipt.assigned_ids["run_id"],
                )
                .await?;
            let outcome = if unknown {
                ToolOutcome::EffectUnknown
            } else {
                ToolOutcome::Succeeded
            };
            let value = full_result(&harness, &command, outcome, "result-proof").await?;
            session.tool_result(&"r".repeat(128), value.clone()).await?;
            let (session, harness, definite) = if unknown {
                session.disconnect().await;
                let replacement = recovery::harness_at(harness.store.lock().await.clone()).await;
                let definite = full_result(
                    &replacement,
                    &command,
                    ToolOutcome::Succeeded,
                    "definite-proof",
                )
                .await?;
                let mut request = recovery::request(&*replacement.store.lock().await, false)?;
                request.results.push(definite.clone());
                let (restored, _) =
                    recovery::restore(request, replacement.clone(), Vec::new()).await?;
                (restored, replacement, definite)
            } else {
                (session, harness, value)
            };
            let done = tokio::time::timeout(Duration::from_secs(15), session.drive()).await??;
            assert_eq!(
                done.run.as_ref().map(|run| run.status),
                Some(RunStatus::Failed)
            );
            let retained = &done.root_turn().ok_or("turn")?.invocations[0];
            assert_eq!(retained.result.as_ref(), Some(&definite));
            assert!(retained.consumed);
            assert_eq!(retained.prior_uncertain_result.is_some(), unknown);
            assert!(done.agents[&done.agent_id].history.iter().flat_map(|message| &message.content).any(|content| matches!(content, Content::ToolResult { output: bitrouter_sdk::language_model::types::ToolResultOutput::Text { value }, .. } if value == &definite.output)));
            assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
            session
                .release(&"z".repeat(128), session.head().await.state_revision)
                .await?;
            for batch in &harness.store.lock().await.batches {
                assert!(batch.wire_bytes()? <= wire);
                let payload = batch.decode(&Limits::default())?;
                assert!(serde_json::to_vec(&payload)?.len() as u64 <= checkpoint);
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn saturated_tree_preserves_waits_pairing_and_child_delivery_after_ack_loss() -> TestResult {
    use bitrouter_orchestrator::core::session::AgentStatus;

    for committed in [false, true] {
        let harness = Arc::new(Harness::new(None, None));
        let port = Arc::new(reconnect::FaultPort::new(
            harness.clone(),
            "agent.result.delivered",
            committed,
        ));
        let (session, executor, _) = setup(Vec::new(), port.clone(), false).await?;
        let mut task = input();
        task.limits = Some(Limits {
            input_bytes: 4096,
            mailbox_messages: 4,
            queued_runs: 2,
            // Admit the additional normal-wait source obligation before the
            // same test deliberately exhausts remaining optional capacity.
            checkpoint_bytes: 640 * 1024,
            unacknowledged_bytes: 1280 * 1024,
            ..Limits::default()
        });
        let accepted = session.start("input", 1, task).await?;
        let root = accepted.assigned_ids["agent_id"].clone();
        let mut agents = vec![root.clone()];
        for (index, parent) in [0, 0, 1].into_iter().enumerate() {
            let child = session
                .collaborate(
                    &format!("spawn-{index}"),
                    session.head().await.state_revision,
                    &agents[parent],
                    Action::Spawn {
                        task: work("settle owned workspace evidence"),
                    },
                )
                .await?
                .assigned_ids["agent_id"]
                .clone();
            agents.push(child);
        }
        for (index, agent) in agents.iter().enumerate() {
            executor
                .agent_once
                .lock()
                .await
                .insert(agent.clone(), vec![call(&format!("read-{index}"))]);
        }
        session.drive().await?;
        assert_eq!(harness.sent.lock().await.len(), agents.len());
        assert_eq!(executor.calls.load(Ordering::SeqCst), agents.len());
        let first_root = harness
            .sent
            .lock()
            .await
            .iter()
            .find(|command| command.agent_id == root)
            .ok_or("root tool")?
            .clone();
        executor.agent_once.lock().await.insert(
            root.clone(),
            vec![
                call("root-second-read"),
                core_call(
                    "wait_agent",
                    json!({"agent_ids":agents[1..],"timeout_ms":600_000}),
                    "model-wait",
                ),
            ],
        );
        session
            .tool_result("root-first", result(&first_root))
            .await?;
        session.drive().await?;
        let commands = harness
            .sent
            .lock()
            .await
            .iter()
            .filter(|command| command.invocation_id != first_root.invocation_id)
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(commands.len(), agents.len());

        // Several independent wait operations retain the same graph view. A
        // queued follow-up changes their observed target without executing it.
        for index in 0..8 {
            session
                .collaborate(
                    &format!("wait-{index}-{}", "w".repeat(100)),
                    session.head().await.state_revision,
                    &root,
                    Action::Wait {
                        agent_ids: agents[1..].to_vec(),
                        timeout_ms: 600_000,
                    },
                )
                .await?;
        }
        for index in 0..2 {
            let mut followup = work("must be cancelled without model dispatch");
            followup.fresh_context = false;
            session
                .collaborate(
                    &format!("queued-{index}"),
                    session.head().await.state_revision,
                    &root,
                    Action::Followup {
                        agent_id: agents[1].clone(),
                        task: followup,
                    },
                )
                .await?;
        }
        for (index, agent) in agents.iter().enumerate() {
            for slot in 0..4 {
                session
                    .collaborate(
                        &format!("mail-{index}-{slot}"),
                        session.head().await.state_revision,
                        &root,
                        Action::Message {
                            agent_id: agent.clone(),
                            text: format!("retained mail {index}/{slot}"),
                        },
                    )
                    .await?;
            }
        }
        let before = session.snapshot().await;
        assert_eq!(before.waits.len(), 8);
        assert!(before.waits.values().all(|wait| wait.result.is_none()));
        assert_eq!(before.agents[&agents[1]].queue.len(), 2);
        assert!(before.agents.values().all(|agent| agent.mailbox.len() == 4));
        let model_wait = &before.root_turn().ok_or("root turn")?.core_calls[0];
        assert!(model_wait.wait.is_some());
        assert!(model_wait.result.is_none());
        fill_optional_observations(&session, &commands[0], ToolStatus::Running).await?;

        // Every already dispatched tool may still report its full contracted
        // payload after the independent capacity-failure checkpoint commits.
        let mut outcomes = Vec::new();
        for (index, command) in commands.iter().enumerate() {
            let outcome = full_result(
                &harness,
                command,
                ToolOutcome::Succeeded,
                &format!("tree-proof-{index}"),
            )
            .await?;
            session
                .tool_result(&format!("result-{index}"), outcome.clone())
                .await?;
            outcomes.push(outcome);
        }
        let error = session
            .drive()
            .await
            .err()
            .ok_or("missing delivery ACK loss")?;
        assert_eq!(error.commit_status, CommitStatus::Unknown);
        let original = port
            .proposals
            .lock()
            .await
            .last()
            .ok_or("delivery proposal")?
            .clone();
        let payload = original.decode(&Limits::default())?;
        assert_eq!(payload.events[0].kind, "agent.result.delivered");
        reconnect::reconnect(&session, &harness).await?;
        let done = tokio::time::timeout(Duration::from_secs(30), session.drive()).await??;
        assert_eq!(
            done.run.as_ref().map(|run| run.status),
            Some(RunStatus::Failed)
        );
        assert!(done.agents.values().all(|agent| {
            agent.queue.is_empty()
                && agent.turn.as_ref().is_some_and(|turn| {
                    turn.status == AgentStatus::Interrupted
                        && (agent.parent_id.is_none() || turn.notified)
                        && turn.invocations.iter().all(|call| call.consumed)
                })
        }));
        assert!(done.waits.values().all(|wait| {
            wait.result.as_ref().is_some_and(|result| {
                result["agents"]
                    .as_array()
                    .is_some_and(|agents| agents.len() == 3)
            })
        }));
        for (id, wait) in &before.waits {
            assert_eq!(
                serde_json::to_value(&done.waits[id].state)?,
                serde_json::to_value(&wait.state)?
            );
            assert!(session.operation(id).await.is_some());
        }
        for index in 0..2 {
            assert!(
                session
                    .operation(&format!("queued-{index}"))
                    .await
                    .is_some()
            );
        }
        let model_wait = &done.root_turn().ok_or("root turn")?.core_calls[0];
        assert!(model_wait.consumed);
        assert_eq!(
            model_wait.result,
            Some(json!({"ok":false,"reason":"agent interrupted"}))
        );
        let paired_waits = done.agents[&root]
            .history
            .iter()
            .flat_map(|message| &message.content)
            .filter(|part| matches!(part, Content::ToolResult { call_id, .. } if call_id == "model-wait"))
            .count();
        assert_eq!(paired_waits, 1);
        for (command, outcome) in commands.iter().zip(&outcomes) {
            let agent = &done.agents[&command.agent_id];
            assert_eq!(
                agent
                    .turn
                    .as_ref()
                    .ok_or("turn")?
                    .invocations
                    .iter()
                    .find(|call| call.dispatch.invocation_id == command.invocation_id)
                    .ok_or("invocation")?
                    .result
                    .as_ref(),
                Some(outcome)
            );
            assert!(agent.history.iter().flat_map(|message| &message.content).any(|part| {
                matches!(part, Content::ToolResult { output: bitrouter_sdk::language_model::types::ToolResultOutput::Text { value }, .. } if value == &outcome.output)
            }));
            assert!(before.agents[&command.agent_id].mailbox.iter().all(|mail| {
                agent.mailbox.iter().any(|retained| {
                    retained.message_id == mail.message_id && retained.content == mail.content
                })
            }));
        }
        for child_id in &agents[1..] {
            let child = &done.agents[child_id];
            let turn = child.turn.as_ref().ok_or("child turn")?;
            let notifications = done.agents[&turn.assigned_by]
                .mailbox
                .iter()
                .filter(|mail| mail.kind == "agent_result" && mail.sender_id == *child_id)
                .collect::<Vec<_>>();
            assert_eq!(notifications.len(), 1);
            assert_eq!(notifications[0].context_sources, child.context_sources);
        }
        assert_eq!(
            port.proposals
                .lock()
                .await
                .iter()
                .filter(|batch| **batch == original)
                .count(),
            if committed { 1 } else { 2 }
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), agents.len() + 1);
        assert_eq!(harness.sent.lock().await.len(), commands.len() + 1);
        session
            .release("release", session.head().await.state_revision)
            .await?;
        for batch in &harness.store.lock().await.batches {
            assert!(batch.wire_bytes()? <= 1280 * 1024);
            assert!(serde_json::to_vec(&batch.decode(&Limits::default())?)?.len() <= 640 * 1024);
        }
    }
    Ok(())
}

#[tokio::test]
async fn cleanup_capacity_preserves_running_recovery_at_saturation() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
    let mut task = input();
    task.limits = Some(Limits {
        input_bytes: 4096,
        checkpoint_bytes: 96 * 1024,
        unacknowledged_bytes: 192 * 1024,
        ..Limits::default()
    });
    session.start("input", 1, task).await?;
    session.drive().await?;
    let command = harness.sent.lock().await.first().ok_or("tool")?.clone();
    fill_optional_observations(&session, &command, ToolStatus::Running).await?;
    let mut restored = session;
    let mut replacement = harness;
    let mut observations = Vec::new();
    for index in 0..16 {
        restored.disconnect().await;
        let retained_store = replacement.store.lock().await.clone();
        replacement = recovery::harness_at(retained_store).await;
        let observation = full_observation(
            &replacement,
            &command,
            ToolStatus::Running,
            &format!("restored-running-{index:02}"),
        )
        .await?;
        let store = replacement.store.lock().await;
        let mut request = recovery::request(&store, false)?;
        // Availability describes direct roots. Historical evidence is retained
        // through the archive dependency closure, not flattened into Restore.
        request.available_artifacts = store
            .batches
            .last()
            .ok_or("checkpoint")?
            .decode(&store.limits)?
            .checkpoint
            .artifact_refs;
        request
            .available_artifacts
            .extend(observation.evidence.iter().cloned());
        request.tools.push(observation.clone());
        drop(store);
        let (next, executor) = recovery::restore(request, replacement.clone(), Vec::new()).await?;
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        assert!(replacement.sent.lock().await.is_empty());
        observations.push(observation);
        restored = next;
    }
    let state = restored.snapshot().await;
    assert!(state.recovery_archive.is_some());
    assert_eq!(
        state
            .run
            .as_ref()
            .ok_or("run")?
            .activity_reconciliations
            .len(),
        16
    );
    assert_eq!(
        state.root_turn().ok_or("turn")?.invocations[0]
            .recovery_observation
            .as_ref(),
        observations.last()
    );
    assert_eq!(
        state.root_turn().ok_or("turn")?.invocations[0].prior_recovery_observations,
        observations[..15]
    );
    let archive = state.recovery_archive.as_ref().ok_or("archive")?;
    for missing in [archive.artifact_id.as_str(), "restored-running-00"] {
        let mut store = replacement.store.lock().await.clone();
        store.artifact_bytes.remove(missing);
        let faulty = recovery::harness_at(store).await;
        let mut request = recovery::request(&*faulty.store.lock().await, false)?;
        request
            .tools
            .push(observations.last().ok_or("observation")?.clone());
        let head = faulty.store.lock().await.head.clone();
        let error = recovery::restore(request, faulty.clone(), Vec::new())
            .await
            .err()
            .ok_or("missing artifact was accepted")?;
        assert_eq!(
            error
                .downcast_ref::<CoreError>()
                .map(|error| (error.code, error.commit_status)),
            Some((ErrorCode::ArtifactUnavailable, CommitStatus::NotCommitted))
        );
        assert_eq!(faulty.store.lock().await.head, head);
        assert!(faulty.sent.lock().await.is_empty());
    }
    let fenced = recovery::harness_at(replacement.store.lock().await.clone()).await;
    let mut request = recovery::request(&*fenced.store.lock().await, false)?;
    request.tools.push(ToolObservation {
        invocation_id: command.invocation_id.clone(),
        attempt_id: command.attempt_id.clone(),
        status: ToolStatus::NotStarted,
        evidence: Vec::new(),
    });
    let head = fenced.store.lock().await.head.clone();
    let error = recovery::restore(request, fenced.clone(), Vec::new())
        .await
        .err()
        .ok_or("archived running fact was erased")?;
    assert_eq!(
        error.downcast_ref::<CoreError>().map(|error| error.code),
        Some(ErrorCode::OperationConflict)
    );
    assert_eq!(fenced.store.lock().await.head, head);
    assert!(fenced.sent.lock().await.is_empty());
    for committed in [false, true] {
        let faulty = recovery::harness_at(replacement.store.lock().await.clone()).await;
        let before = faulty.store.lock().await.head.clone();
        let mut request = recovery::request(&*faulty.store.lock().await, false)?;
        request
            .tools
            .push(observations.last().ok_or("observation")?.clone());
        let port = Arc::new(reconnect::FaultPort::new(
            faulty.clone(),
            "session.restored",
            committed,
        ));
        let error = recovery::restore(request, port, Vec::new())
            .await
            .err()
            .ok_or("restore ACK was not lost")?;
        assert_eq!(
            error
                .downcast_ref::<CoreError>()
                .map(|error| error.commit_status),
            Some(CommitStatus::Unknown)
        );
        assert_eq!(
            faulty.store.lock().await.head.state_revision,
            before.state_revision + u64::from(committed)
        );
        let resumed = recovery::harness_at(faulty.store.lock().await.clone()).await;
        let mut request = recovery::request(&*resumed.store.lock().await, false)?;
        request
            .tools
            .push(observations.last().ok_or("observation")?.clone());
        let (session, executor) = recovery::restore(request, resumed.clone(), Vec::new()).await?;
        assert_eq!(
            session
                .snapshot()
                .await
                .run
                .as_ref()
                .ok_or("run")?
                .activity_reconciliations
                .len(),
            17 + usize::from(committed)
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        assert!(resumed.sent.lock().await.is_empty());
        session.disconnect().await;
    }
    let stopped = recovery::harness_at(replacement.store.lock().await.clone()).await;
    let mut request = recovery::request(&*stopped.store.lock().await, false)?;
    request
        .tools
        .push(observations.last().ok_or("observation")?.clone());
    request.active_time.as_mut().ok_or("clock")?.active_ms = 599_500;
    let stop_port = Arc::new(StopDuringArchiveRead {
        harness: stopped,
        command: command.clone(),
        stopped: Mutex::new(None),
    });
    let (with_stop, _) = recovery::restore(request, stop_port, Vec::new()).await?;
    with_stop
        .signals(
            "clock-after-archive",
            signal_update(&with_stop, Vec::new()).await,
        )
        .await?;
    let stopped_state = with_stop.snapshot().await;
    let run = stopped_state.run.as_ref().ok_or("run")?;
    assert!(run.active_ms < 600_000);
    assert_eq!(
        run.resource_constraint,
        Some(ResourceConstraint::CheckpointCapacity)
    );
    assert!(
        stopped_state.root_turn().ok_or("turn")?.invocations[0]
            .tool_observations
            .values()
            .any(|observation| observation.status == ToolStatus::Stopped)
    );
    with_stop.disconnect().await;
    restored
        .cancel_run(
            "cancel",
            restored.head().await.state_revision,
            &state.run.as_ref().ok_or("run")?.run_id,
        )
        .await?;
    restored
        .tool_result(
            "settled",
            full_result(
                &replacement,
                &command,
                ToolOutcome::Succeeded,
                "result-proof",
            )
            .await?,
        )
        .await?;
    assert_eq!(
        restored.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Failed)
    );
    assert!(replacement.sent.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn capacity_failure_resolves_signal_fence_after_ack_or_reconnect() -> TestResult {
    use bitrouter_orchestrator::core::checkpoint::ToolStartFence;
    for lost_ack in [None, Some(false), Some(true)] {
        let mut fixture = Harness::new(None, None);
        fixture.wait_for_approval = true;
        let harness = Arc::new(fixture);
        let fault = lost_ack.map(|committed| {
            Arc::new(reconnect::FaultPort::new(
                harness.clone(),
                "run.capacity_reached",
                committed,
            ))
        });
        let port: Arc<dyn HarnessPort> = match &fault {
            Some(port) => port.clone(),
            None => harness.clone(),
        };
        let (session, executor, _) = setup(
            vec![output(vec![call("running"), call("approval")])],
            port,
            false,
        )
        .await?;
        let mut task = input();
        task.limits = Some(Limits {
            input_bytes: 2048,
            checkpoint_bytes: 96 * 1024,
            unacknowledged_bytes: 192 * 1024,
            ..Limits::default()
        });
        session.start("input", 1, task).await?;
        session.drive().await?;
        let commands = harness.sent.lock().await.clone();
        assert_eq!(commands.len(), 2);
        let fences = commands
            .iter()
            .map(|command| ToolStartFence {
                invocation_id: command.invocation_id.clone(),
                attempt_id: command.attempt_id.clone(),
            })
            .collect::<Vec<_>>();
        assert!(harness.store.lock().await.try_start_tool(fences[0].clone()));
        session
            .tool_status(
                "running",
                ToolObservation {
                    invocation_id: commands[0].invocation_id.clone(),
                    attempt_id: commands[0].attempt_id.clone(),
                    status: ToolStatus::Running,
                    evidence: Vec::new(),
                },
            )
            .await?;
        let before = session.head().await;
        let original_manifest = session.snapshot().await.manifest;
        let mut update = signal_update(&session, Vec::new()).await;
        update
            .facts
            .insert("large_inventory".into(), json!("x".repeat(60 * 1024)));
        update.manifest.permission_revision += 1;
        let error = session
            .signals("oversized-signal", update)
            .await
            .err()
            .ok_or("capacity admitted")?;
        if let Some(committed) = lost_ack {
            assert_eq!(
                (error.code, error.commit_status),
                (ErrorCode::CheckpointUnavailable, CommitStatus::Unknown)
            );
            assert_eq!(session.head().await, before);
            assert_eq!(
                harness.store.lock().await.head.state_revision,
                before.state_revision + u64::from(committed)
            );
            assert!(session.drive().await.is_err());
            assert!(harness.cancelled.lock().await.is_empty());
            reconnect::reconnect(&session, &harness).await?;
            let proposals = fault.as_ref().ok_or("fault")?.proposals.lock().await;
            let failures = proposals
                .iter()
                .filter(|batch| {
                    batch.decode(&Limits::default()).is_ok_and(|payload| {
                        payload
                            .events
                            .iter()
                            .any(|event| event.kind == "run.capacity_reached")
                    })
                })
                .collect::<Vec<_>>();
            assert_eq!(failures.len(), if committed { 1 } else { 2 });
            assert!(failures.iter().all(|batch| *batch == failures[0]));
        } else {
            assert_eq!(
                (error.code, error.commit_status),
                (ErrorCode::LimitExceeded, CommitStatus::NotCommitted)
            );
            assert_eq!(
                session.head().await.state_revision,
                before.state_revision + 1
            );
        }
        let state = session.snapshot().await;
        assert_eq!(state.manifest, original_manifest);
        assert!(state.signals.facts.is_empty());
        assert_eq!(state.signals.revision, 0);
        assert!(session.operation("oversized-signal").await.is_none());
        let run = state.run.as_ref().ok_or("run")?;
        assert_eq!(
            run.resource_constraint,
            Some(ResourceConstraint::CheckpointCapacity)
        );
        assert!(run.active_ms < run.limits.active_seconds * 1000);
        assert_eq!(
            harness
                .committed_kinds()
                .await?
                .iter()
                .filter(|kind| *kind == "run.capacity_reached")
                .count(),
            1
        );
        {
            let mut store = harness.store.lock().await;
            assert!(
                fences
                    .iter()
                    .all(|fence| store.tool_start_fences.contains(fence))
            );
            assert!(!store.try_start_tool(fences[1].clone()));
        }
        // Cleanup must remain dispatchable even though the rejected signal has
        // no operation receipt, including both uncertain-commit resolutions.
        session.drive().await?;
        tokio::time::timeout(Duration::from_secs(5), harness.cancel_seen.acquire_many(2))
            .await??
            .forget();
        assert_eq!(harness.cancelled.lock().await.len(), 2);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        session.tool_result("actual", result(&commands[0])).await?;
        let mut prevented = result(&commands[1]);
        prevented.status = ToolOutcome::NotExecuted;
        prevented.output.clear();
        session.tool_result("prevented", prevented).await?;
        let done = session.drive().await?;
        assert_eq!(
            done.run.as_ref().map(|run| run.status),
            Some(RunStatus::Failed)
        );
        assert!(
            done.root_turn()
                .ok_or("turn")?
                .invocations
                .iter()
                .all(|call| call.consumed)
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    }
    Ok(())
}

#[tokio::test]
async fn capacity_failure_history_cannot_be_erased_or_reclassified() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
    let mut task = input();
    task.limits = Some(Limits {
        input_bytes: 4096,
        checkpoint_bytes: 96 * 1024,
        unacknowledged_bytes: 192 * 1024,
        ..Limits::default()
    });
    session.start("input", 1, task).await?;
    session.drive().await?;
    let mut update = signal_update(&session, Vec::new()).await;
    update
        .facts
        .insert("large_inventory".into(), json!("x".repeat(60 * 1024)));
    let error = session
        .signals("too-large", update)
        .await
        .err()
        .ok_or("capacity admitted")?;
    assert_eq!(
        (error.code, error.commit_status),
        (ErrorCode::LimitExceeded, CommitStatus::NotCommitted)
    );
    let state = session.snapshot().await;
    let run_id = state.run.as_ref().ok_or("run")?.run_id.clone();
    session
        .cancel_run("cancel", session.head().await.state_revision, &run_id)
        .await?;
    session.disconnect().await;
    let mut store = harness.store.lock().await.clone();
    // Keep only the failure anchor and following checkpoint. No unrelated
    // history needs to fit into this authenticated restore envelope.
    let failure = store.batches.len().checked_sub(2).ok_or("failure anchor")?;
    store.batches.drain(..failure);
    let anchor = store.batches[0].decode(&store.limits)?;
    let last = store.batches[1].decode(&store.limits)?;
    assert_eq!(anchor.events[0].kind, "run.capacity_reached");
    for alteration in [
        "erase",
        "rewrite",
        "cause",
        "event",
        "repeat",
        "uncancel",
        "runnable",
        "across-none",
        "across-run",
    ] {
        let mut altered = store.clone();
        let mut first = anchor.clone();
        let mut final_payload = last.clone();
        let run = &mut final_payload.checkpoint.state["run"];
        match alteration {
            "erase" | "across-none" | "across-run" => {
                run["resource_error"] = serde_json::Value::Null;
                run.as_object_mut()
                    .ok_or("run object")?
                    .remove("resource_constraint");
            }
            "rewrite" => run["resource_error"]["message"] = json!("changed"),
            "cause" => run["resource_constraint"] = json!("active_time"),
            "event" => first.events[0].payload["message"] = json!("changed"),
            "repeat" => {
                final_payload.events[0] = bitrouter_orchestrator::core::checkpoint::DurableEvent {
                    event_seq: final_payload.events[0].event_seq,
                    ..first.events[0].clone()
                }
            }
            "uncancel" => {
                final_payload.checkpoint.state["agents"][&state.agent_id]["turn"]["cancellation_requested"] =
                    json!(false)
            }
            "runnable" => {
                final_payload.checkpoint.state["agents"][&state.agent_id]["turn"]["status"] =
                    json!("runnable")
            }
            _ => return Err("unknown fixture".into()),
        }
        altered.batches = vec![CheckpointBatch::encode(&first, &altered.limits)?];
        if matches!(alteration, "across-none" | "across-run") {
            let mut middle = last.clone();
            middle.identity.batch_id = "intermediate".into();
            if alteration == "across-none" {
                middle.checkpoint.state["run"] = serde_json::Value::Null;
            } else {
                middle.checkpoint.state["run"]["run_id"] = json!("another-run");
                middle.checkpoint.state["run"]["resource_error"] = serde_json::Value::Null;
                middle.checkpoint.state["run"]
                    .as_object_mut()
                    .ok_or("run object")?
                    .remove("resource_constraint");
            }
            altered
                .batches
                .push(CheckpointBatch::encode(&middle, &altered.limits)?);
            final_payload.base_state_revision += 1;
            final_payload.base_event_seq += 1;
            final_payload.checkpoint.state_revision += 1;
            final_payload.events[0].event_seq += 1;
        }
        let batch = CheckpointBatch::encode(&final_payload, &altered.limits)?;
        let mut ack = altered
            .acknowledgements
            .get(&batch.identity.batch_id)
            .ok_or("ack")?
            .clone();
        ack.payload_sha256 = batch.payload_sha256.clone();
        ack.state_revision = final_payload.checkpoint.state_revision;
        ack.through_event_seq = final_payload.events.last().ok_or("event")?.event_seq;
        altered.head = ack.head();
        altered
            .acknowledgements
            .insert(batch.identity.batch_id.clone(), ack);
        altered.batches.push(batch);
        let replacement = recovery::harness_at(altered).await;
        let before = replacement.store.lock().await.head.clone();
        let request = recovery::request(&*replacement.store.lock().await, true)?;
        let error = recovery::restore(request, replacement.clone(), Vec::new())
            .await
            .err()
            .ok_or("tampering accepted")?;
        let error = error.downcast_ref::<CoreError>().ok_or("core error")?;
        assert_eq!(
            error.code,
            ErrorCode::CheckpointConflict,
            "{alteration}: {error}"
        );
        assert!(
            error.message.contains("resource failure"),
            "{alteration}: {error}"
        );
        assert_eq!(replacement.store.lock().await.head, before);
    }
    // A checkpoint-only anchor still restores an already accepted capacity
    // failure when the originating event predates the supplied journal.
    let replacement = recovery::harness_at(store).await;
    let request = recovery::request(&*replacement.store.lock().await, false)?;
    let (restored, executor) = recovery::restore(request, replacement, Vec::new()).await?;
    assert_eq!(
        restored
            .snapshot()
            .await
            .run
            .as_ref()
            .and_then(|run| run.resource_constraint),
        Some(ResourceConstraint::CheckpointCapacity)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn capacity_failure_keeps_unrelated_blocks_and_recovers_abandoned_request() -> TestResult {
    for abandon in [false, true] {
        let harness = Arc::new(Harness::new(None, Some("run.capacity_reached")));
        let (session, executor, _) =
            setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
        let mut task = input();
        task.limits = Some(Limits {
            input_bytes: 4096,
            checkpoint_bytes: 96 * 1024,
            unacknowledged_bytes: 192 * 1024,
            ..Limits::default()
        });
        session.start("input", 1, task).await?;
        session.drive().await?;
        let command = harness.sent.lock().await.first().ok_or("tool")?.clone();
        let mut large = signal_update(&session, Vec::new()).await;
        large
            .facts
            .insert("inventory".into(), json!("x".repeat(60 * 1024)));
        let update = {
            let session = session.clone();
            tokio::spawn(async move { session.signals("too-large", large).await })
        };
        tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
            .await??
            .forget();
        let unrelated = signal_update(&session, Vec::new()).await;
        {
            // Poll a distinct control while the first holds input serialization.
            // It installs its own provisional block but has no proposed batch.
            let pending = session.signals("unrelated", unrelated.clone());
            tokio::pin!(pending);
            tokio::select! {
                biased;
                _ = &mut pending => return Err("unrelated control bypassed input serialization".into()),
                _ = tokio::task::yield_now() => {}
            }
        }
        harness.hold_enabled.store(false, Ordering::SeqCst);
        if abandon {
            update.abort();
            assert!(update.await.is_err_and(|error| error.is_cancelled()));
            reconnect::reconnect(&session, &harness).await?;
        } else {
            harness.resume.add_permits(1);
            let error = update.await?.err().ok_or("capacity admitted")?;
            assert_eq!(error.commit_status, CommitStatus::NotCommitted);
        }
        assert!(session.operation("too-large").await.is_none());
        let error = session
            .drive()
            .await
            .err()
            .ok_or("unrelated block cleared")?;
        assert_eq!(error.code, ErrorCode::CheckpointUnavailable);
        assert!(harness.cancelled.lock().await.is_empty());
        session.signals("unrelated", unrelated).await?;
        session.drive().await?;
        tokio::time::timeout(Duration::from_secs(5), harness.cancel_seen.acquire())
            .await??
            .forget();
        session.tool_result("actual", result(&command)).await?;
        assert_eq!(
            session.drive().await?.run.as_ref().map(|run| run.status),
            Some(RunStatus::Failed)
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            harness
                .committed_kinds()
                .await?
                .iter()
                .filter(|kind| *kind == "run.capacity_reached")
                .count(),
            1
        );
    }
    Ok(())
}
