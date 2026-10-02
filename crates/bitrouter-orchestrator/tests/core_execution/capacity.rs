use super::*;
use bitrouter_orchestrator::core::protocol::{ToolObservation, ToolStatus};

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
                assert_eq!(session.head().await, before);
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
                Some(RunStatus::Cancelled)
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
    assert!(run.active_ms < 600_000 && run.resource_error.is_none());
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
        Some(RunStatus::Cancelled)
    );
    assert!(replacement.sent.lock().await.is_empty());
    Ok(())
}
