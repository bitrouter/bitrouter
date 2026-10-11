use super::*;
use bitrouter_orchestrator::core::protocol::{ToolObservation, ToolStatus};

fn observed(command: &ToolExecute, status: ToolStatus) -> ToolObservation {
    ToolObservation {
        invocation_id: command.invocation_id.clone(),
        attempt_id: command.attempt_id.clone(),
        status,
        evidence: Vec::new(),
    }
}

async fn waiting() -> Result<(Arc<Harness>, Vec<ToolExecute>), Box<dyn std::error::Error>> {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(
        vec![output(vec![call("first-read"), call("second-read")])],
        harness.clone(),
        false,
    )
    .await?;
    session.start("input", 1, input()).await?;
    session.drive().await?;
    session.disconnect().await;
    let commands = harness.sent.lock().await.clone();
    assert_eq!(commands.len(), 2);
    Ok((harness, commands))
}

#[tokio::test]
async fn recovery_time_requires_fresh_run_and_head_evidence_before_any_commit() -> TestResult {
    let source = super::recovery::completed_store().await?;
    let source = super::recovery::prefix(&source, "input.accepted")?;
    let harness = super::recovery::harness_at(source).await;
    let mut request = super::recovery::request(&*harness.store.lock().await, false)?;
    request.active_time.as_mut().ok_or("clock")?.active_ms = 500;
    let (session, _) = super::recovery::restore(request, harness.clone(), Vec::new()).await?;
    session.disconnect().await;
    let source = harness.store.lock().await.clone();
    for case in ["missing", "run", "head", "epoch", "digest", "regression"] {
        let harness = super::recovery::harness_at(source.clone()).await;
        let mut request = super::recovery::request(&*harness.store.lock().await, true)?;
        let proof = request.active_time.as_mut().ok_or("clock")?;
        let expected = match case {
            "missing" => {
                request.active_time = None;
                ErrorCode::RecoveryRequired
            }
            "run" => {
                proof.run_id = "another-run".into();
                ErrorCode::UnauthorizedScope
            }
            "head" => {
                proof.durable_head.state_revision -= 1;
                ErrorCode::CheckpointConflict
            }
            "epoch" => {
                proof.durable_head.execution_epoch += 1;
                ErrorCode::CheckpointConflict
            }
            "digest" => {
                proof.durable_head.payload_sha256 = Some("0".repeat(64));
                ErrorCode::CheckpointConflict
            }
            _ => {
                proof.active_ms = 499;
                ErrorCode::OperationConflict
            }
        };
        let error = super::recovery::restore(request, harness.clone(), Vec::new())
            .await
            .err()
            .ok_or("invalid clock accepted")?;
        let error = error.downcast_ref::<CoreError>().ok_or("core error")?;
        assert_eq!(error.code, expected, "{case}");
        assert_eq!(error.commit_status, CommitStatus::NotCommitted, "{case}");
        assert_eq!(
            harness.store.lock().await.batches.len(),
            source.batches.len()
        );
        assert!(harness.sent.lock().await.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn recovery_time_keeps_cumulative_union_across_restores_and_resets_for_new_run() -> TestResult
{
    let (source, commands) = waiting().await?;
    let harness = super::recovery::harness_at(source.store.lock().await.clone()).await;
    let mut request = super::recovery::request(&*harness.store.lock().await, true)?;
    let before = request.active_time.as_ref().ok_or("clock")?.active_ms;
    // Two tools overlapped during a measured 2750 ms interval. The harness
    // attests their union, including any overlapping provider/preparation work.
    request.active_time.as_mut().ok_or("clock")?.active_ms += 2750;
    request.results = commands.iter().map(result).collect();
    let proof = request.active_time.clone().ok_or("clock")?;
    let (session, executor) =
        super::recovery::restore(request, harness.clone(), Vec::new()).await?;
    let state = session.snapshot().await;
    let run = state.run.as_ref().ok_or("run")?;
    assert_eq!(run.active_ms, before + 2750);
    assert_eq!(run.activity_reconciliations, vec![proof]);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert!(harness.sent.lock().await.is_empty());
    session.disconnect().await;

    let harness = super::recovery::harness_at(harness.store.lock().await.clone()).await;
    let request = super::recovery::request(&*harness.store.lock().await, true)?;
    let (session, _) =
        super::recovery::restore(request, harness.clone(), vec![output(vec![text("done")])])
            .await?;
    let state = session.snapshot().await;
    assert_eq!(state.run.as_ref().ok_or("run")?.active_ms, before + 2750);
    assert_eq!(
        state
            .run
            .as_ref()
            .ok_or("run")?
            .activity_reconciliations
            .len(),
        2
    );
    assert_eq!(
        session.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert!(harness.sent.lock().await.is_empty());
    session
        .start("next", session.head().await.state_revision, input())
        .await?;
    let state = session.snapshot().await;
    let run = state.run.as_ref().ok_or("run")?;
    assert_eq!(run.active_ms, 0);
    assert!(run.activity_reconciliations.is_empty());
    Ok(())
}

#[tokio::test]
async fn recovery_time_exhausted_downtime_blocks_replacement_work() -> TestResult {
    let source = super::recovery::completed_store().await?;
    for boundary in [
        "input.accepted",
        "model.attempt.outcome",
        "model.output.applied",
    ] {
        let source = super::recovery::prefix(&source, boundary)?;
        let harness = super::recovery::harness_at(source).await;
        let mut request = super::recovery::request(&*harness.store.lock().await, true)?;
        request.active_time.as_mut().ok_or("clock")?.active_ms = 600_000;
        if boundary == "model.output.applied" {
            let state: SessionSnapshot = serde_json::from_value(
                harness
                    .store
                    .lock()
                    .await
                    .batches
                    .last()
                    .ok_or("batch")?
                    .decode(&Limits::default())?
                    .checkpoint
                    .state,
            )?;
            request.tools = state
                .root_turn()
                .ok_or("turn")?
                .invocations
                .iter()
                .map(|call| observed(&call.dispatch, ToolStatus::NotStarted))
                .collect();
        }
        let (session, executor) =
            super::recovery::restore(request, harness.clone(), Vec::new()).await?;
        let state = tokio::time::timeout(Duration::from_secs(5), session.drive()).await??;
        let run = state.run.as_ref().ok_or("run")?;
        assert_eq!(run.status, RunStatus::Failed, "{boundary}");
        assert_eq!(
            run.resource_error.as_ref().map(|error| error.code),
            Some(ErrorCode::LimitExceeded),
            "{boundary}"
        );
        assert_eq!(run.active_ms, 600_000, "{boundary}");
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0, "{boundary}");
        assert!(harness.sent.lock().await.is_empty(), "{boundary}");
    }
    Ok(())
}

#[tokio::test]
async fn recovery_time_restore_ack_wait_counts_running_union_and_excludes_approval() -> TestResult {
    let (source, commands) = waiting().await?;
    for status in [ToolStatus::Running, ToolStatus::WaitingApproval] {
        let mut store = source.store.lock().await.clone();
        store.grant.execution_epoch += 1;
        store.grant.core_instance_id = "replacement".into();
        let harness = Arc::new(Harness::new(None, Some("session.restored")));
        *harness.store.lock().await = store;
        let mut request = super::recovery::request(&*harness.store.lock().await, false)?;
        request.active_time.as_mut().ok_or("clock")?.active_ms = 500;
        request.tools = commands
            .iter()
            .map(|command| observed(command, status))
            .collect();
        let target = harness.clone();
        let started = std::time::Instant::now();
        let restoring = tokio::spawn(async move {
            super::recovery::restore(request, target, Vec::new())
                .await
                .map_err(|error| error.to_string())
        });
        tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
            .await??
            .forget();
        assert!(!restoring.is_finished());
        assert!(harness.sent.lock().await.is_empty());
        tokio::time::sleep(Duration::from_millis(100)).await;
        harness.resume.add_permits(1);
        let (session, executor) = restoring.await??;
        session
            .signals(
                "clock-checkpoint",
                signal_update(&session, Vec::new()).await,
            )
            .await?;
        let active = session.snapshot().await.run.ok_or("run")?.active_ms;
        if status == ToolStatus::Running {
            assert!(
                active >= 600,
                "running tools lost the restore ACK interval: {active}"
            );
            assert!(
                active <= 500 + started.elapsed().as_millis() as u64 + 1,
                "overlapping tools were counted twice: {active}"
            );
        } else {
            assert_eq!(active, 500, "idle approval must not consume time");
        }
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        session.disconnect().await;
    }
    Ok(())
}

struct LostRestoreAck {
    harness: Arc<Harness>,
}

#[tokio::test]
async fn recovery_time_tools_stopping_during_restore_ack_do_not_charge_the_idle_tail() -> TestResult
{
    let (source, commands) = waiting().await?;
    let mut store = source.store.lock().await.clone();
    store.grant.execution_epoch += 1;
    store.grant.core_instance_id = "replacement".into();
    let harness = Arc::new(Harness::new(None, Some("session.restored")));
    *harness.store.lock().await = store;
    let mut request = super::recovery::request(&*harness.store.lock().await, true)?;
    request.active_time.as_mut().ok_or("clock")?.active_ms = 599_000;
    request.tools = commands
        .iter()
        .map(|command| observed(command, ToolStatus::Running))
        .collect();
    let target = harness.clone();
    let restoring = tokio::spawn(async move {
        super::recovery::restore(request, target, Vec::new())
            .await
            .map_err(|error| error.to_string())
    });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    let observer = harness
        .restoration_activity
        .lock()
        .await
        .clone()
        .ok_or("observer")?;
    assert_eq!(
        observer
            .stopped("unknown", "unknown", std::time::Instant::now())
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::InvalidToolResult)
    );
    let first = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let second = std::time::Instant::now();
    // Delivery order must not replace the true end of the union with the first
    // tool's earlier stop. Both reports arrive before the restore ACK.
    observer
        .stopped(&commands[1].invocation_id, &commands[1].attempt_id, second)
        .await?;
    observer
        .stopped(&commands[0].invocation_id, &commands[0].attempt_id, first)
        .await?;
    observer
        .stopped(&commands[0].invocation_id, &commands[0].attempt_id, first)
        .await?;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    harness.resume.add_permits(1);
    let (session, executor) = restoring.await??;
    session
        .signals("settled-clock", signal_update(&session, Vec::new()).await)
        .await?;
    let state = session.snapshot().await;
    let run = state.run.as_ref().ok_or("run")?;
    assert!(
        run.active_ms >= 599_020 && run.active_ms < 600_000,
        "idle restore ACK time was charged: {}",
        run.active_ms
    );
    assert!(run.resource_error.is_none());
    assert_eq!(run.status, RunStatus::Waiting);
    assert_eq!(
        state
            .root_turn()
            .ok_or("turn")?
            .invocations
            .iter()
            .filter(|call| call
                .tool_observations
                .values()
                .any(|observation| observation.status == ToolStatus::Stopped))
            .count(),
        2
    );
    assert!(
        !harness
            .committed_kinds()
            .await?
            .iter()
            .any(|kind| kind == "run.limit_reached")
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert!(harness.sent.lock().await.is_empty());
    assert_eq!(
        observer
            .stopped(&commands[0].invocation_id, &commands[0].attempt_id, first)
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::Busy)
    );
    session.disconnect().await;
    let replacement = super::recovery::harness_at(harness.store.lock().await.clone()).await;
    let mut request = super::recovery::request(&*replacement.store.lock().await, true)?;
    request.results = commands.iter().map(result).collect();
    let (restored, _) = super::recovery::restore(request, replacement, Vec::new()).await?;
    assert_eq!(
        restored.snapshot().await.run.ok_or("run")?.active_ms,
        run.active_ms
    );
    Ok(())
}

#[tokio::test]
async fn recovery_time_running_tools_require_an_installed_restoration_observer() -> TestResult {
    let (source, commands) = waiting().await?;
    let harness = super::recovery::harness_at(source.store.lock().await.clone()).await;
    let before = harness.store.lock().await.batches.len();
    let mut request = super::recovery::request(&*harness.store.lock().await, false)?;
    request.tools = commands
        .iter()
        .map(|command| observed(command, ToolStatus::Running))
        .collect();
    let app = Arc::new(App::builder().build()?);
    let caps = Capabilities {
        version: 1,
        core_instance_id: "replacement".into(),
        operations: Vec::new(),
        transports: vec!["in_process".into()],
        unsupported_features: Vec::new(),
        limits: Limits::default(),
        max_sessions: 16,
        max_host_model_attempts: 16,
    };
    let error = CoreSession::restore(
        request,
        &caps,
        app,
        CallerContext::local(),
        Arc::new(LostRestoreAck {
            harness: harness.clone(),
        }),
    )
    .await
    .err()
    .ok_or("running tools without observer accepted")?;
    assert_eq!(error.code, ErrorCode::RecoveryRequired);
    assert_eq!(error.commit_status, CommitStatus::NotCommitted);
    assert_eq!(harness.store.lock().await.batches.len(), before);
    Ok(())
}

#[async_trait]
impl HarnessPort for LostRestoreAck {
    async fn read_artifact(
        &self,
        reference: &ArtifactRef,
        offset: u64,
        max_bytes: u64,
    ) -> Result<Vec<u8>, CoreError> {
        self.harness
            .read_artifact(reference, offset, max_bytes)
            .await
    }

    async fn commit(&self, batch: CheckpointBatch) -> Result<CheckpointAck, CoreError> {
        let restored = batch
            .decode(&Limits::default())?
            .events
            .iter()
            .any(|event| event.kind == "session.restored");
        let ack = self.harness.commit(batch).await?;
        if restored {
            Err(CoreError::rejected(
                ErrorCode::CheckpointUnavailable,
                "restore ACK lost",
            ))
        } else {
            Ok(ack)
        }
    }

    async fn send(&self, message: ServerMessage) -> Result<(), CoreError> {
        self.harness.send(message).await
    }
}

#[tokio::test]
async fn recovery_time_adopts_a_committed_restore_after_ack_loss_without_double_charge()
-> TestResult {
    let source = super::recovery::completed_store().await?;
    let harness =
        super::recovery::harness_at(super::recovery::prefix(&source, "input.accepted")?).await;
    let mut request = super::recovery::request(&*harness.store.lock().await, false)?;
    request.active_time.as_mut().ok_or("clock")?.active_ms = 700;
    let app = Arc::new(App::builder().build()?);
    let caps = Capabilities {
        version: 1,
        core_instance_id: "replacement".into(),
        operations: Vec::new(),
        transports: vec!["in_process".into()],
        unsupported_features: Vec::new(),
        limits: Limits::default(),
        max_sessions: 16,
        max_host_model_attempts: 16,
    };
    let error = CoreSession::restore(
        request.clone(),
        &caps,
        app,
        CallerContext::local(),
        Arc::new(LostRestoreAck {
            harness: harness.clone(),
        }),
    )
    .await
    .err()
    .ok_or("lost ACK accepted")?;
    assert_eq!(error.commit_status, CommitStatus::Unknown);
    let stale = super::recovery::restore(request, harness.clone(), Vec::new()).await;
    assert!(stale.is_err(), "old restore head cannot be committed again");
    let replacement = super::recovery::harness_at(harness.store.lock().await.clone()).await;
    let fresh = super::recovery::request(&*replacement.store.lock().await, true)?;
    let (session, executor) =
        super::recovery::restore(fresh, replacement.clone(), Vec::new()).await?;
    let state = session.snapshot().await;
    let run = state.run.as_ref().ok_or("run")?;
    assert_eq!(run.active_ms, 700);
    assert_eq!(run.activity_reconciliations.len(), 2);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert!(replacement.sent.lock().await.is_empty());
    Ok(())
}

fn rewrite_last(
    store: &mut DurableHarness,
    change: impl FnOnce(&mut bitrouter_orchestrator::core::checkpoint::CheckpointPayload),
) -> TestResult {
    let batch = store.batches.last_mut().ok_or("batch")?;
    let mut payload = batch.decode(&store.limits)?;
    change(&mut payload);
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
async fn recovery_time_rejects_erased_or_rewritten_clock_history_and_event() -> TestResult {
    let source = super::recovery::completed_store().await?;
    let harness =
        super::recovery::harness_at(super::recovery::prefix(&source, "input.accepted")?).await;
    let mut request = super::recovery::request(&*harness.store.lock().await, false)?;
    request.active_time.as_mut().ok_or("clock")?.active_ms = 500;
    let (session, _) = super::recovery::restore(request, harness.clone(), Vec::new()).await?;
    let restored = harness.store.lock().await.clone();
    session
        .signals("after-clock", signal_update(&session, Vec::new()).await)
        .await?;
    session.disconnect().await;
    let later = harness.store.lock().await.clone();
    for case in ["erase", "rewrite", "counter", "head", "event"] {
        let mut store = if case == "event" {
            restored.clone()
        } else {
            later.clone()
        };
        rewrite_last(&mut store, |payload| {
            let run = &mut payload.checkpoint.state["run"];
            match case {
                "erase" => run["activity_reconciliations"] = json!([]),
                "rewrite" => run["activity_reconciliations"][0]["active_ms"] = json!(499),
                "counter" => run["active_ms"] = json!(499),
                "head" => {
                    run["activity_reconciliations"][0]["durable_head"]["payload_sha256"] =
                        json!("0".repeat(64))
                }
                _ => payload.events[0].payload["active_time"]["active_ms"] = json!(499),
            }
        })?;
        let count = store.batches.len();
        let harness = super::recovery::harness_at(store).await;
        let request = super::recovery::request(&*harness.store.lock().await, true)?;
        let error = super::recovery::restore(request, harness.clone(), Vec::new())
            .await
            .err()
            .ok_or("forged clock accepted")?;
        assert_eq!(
            error.downcast_ref::<CoreError>().ok_or("core error")?.code,
            ErrorCode::CheckpointConflict,
            "{case}"
        );
        assert_eq!(harness.store.lock().await.batches.len(), count);
    }
    Ok(())
}

#[tokio::test]
async fn recovery_time_settled_sessions_need_no_downtime_claim() -> TestResult {
    let source = super::recovery::completed_store().await?;
    let harness = super::recovery::harness_at(source).await;
    let mut request = super::recovery::request(&*harness.store.lock().await, true)?;
    request.active_time = None;
    let (session, executor) =
        super::recovery::restore(request, harness.clone(), Vec::new()).await?;
    assert_eq!(
        session.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert!(harness.sent.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn recovery_time_delayed_stop_during_status_ack_does_not_freeze_an_overestimate() -> TestResult
{
    let (source, commands) = waiting().await?;
    let mut store = source.store.lock().await.clone();
    store.grant.execution_epoch += 1;
    store.grant.core_instance_id = "replacement".into();
    let mut fixture = Harness::new(None, Some("session.restored"));
    fixture.hold_also_kind = Some("tool.status");
    let harness = Arc::new(fixture);
    *harness.store.lock().await = store;
    let mut request = super::recovery::request(&*harness.store.lock().await, true)?;
    request.active_time.as_mut().ok_or("clock")?.active_ms = 599_000;
    request.tools = commands
        .iter()
        .map(|command| observed(command, ToolStatus::Running))
        .collect();
    let target = harness.clone();
    let restoring = tokio::spawn(async move {
        super::recovery::restore(request, target, Vec::new())
            .await
            .map_err(|error| error.to_string())
    });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    let observer = harness
        .restoration_activity
        .lock()
        .await
        .clone()
        .ok_or("observer")?;
    observer
        .stopped(
            &commands[0].invocation_id,
            &commands[0].attempt_id,
            std::time::Instant::now(),
        )
        .await?;
    let second_stop = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    harness.resume.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    // The first stop's checkpoint is already proposed. The second tool had
    // stopped before it, but its event was still queued in the harness bridge.
    observer
        .stopped(
            &commands[1].invocation_id,
            &commands[1].attempt_id,
            second_stop,
        )
        .await?;
    harness.hold_enabled.store(false, Ordering::SeqCst);
    harness.resume.add_permits(1);
    let (session, executor) = restoring.await??;
    assert_eq!(
        session.snapshot().await.run.ok_or("run")?.active_ms,
        599_000,
        "restoring checkpoints must not seal a provisional running interval"
    );
    session
        .signals("confirmed-clock", signal_update(&session, Vec::new()).await)
        .await?;
    let state = session.snapshot().await;
    let run = state.run.as_ref().ok_or("run")?;
    assert!(
        run.active_ms < 600_000,
        "delayed stop permanently inflated the counter: {}",
        run.active_ms
    );
    assert!(run.resource_error.is_none());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert!(harness.sent.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn recovery_time_observer_stays_open_through_restored_model_output_acks() -> TestResult {
    let source = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(
        vec![
            output(vec![call("root-read")]),
            output(vec![text("child output")]),
        ],
        source.clone(),
        false,
    )
    .await?;
    let root = session.start("input", 1, input()).await?.assigned_ids["agent_id"].clone();
    session.drive().await?;
    let command = source.sent.lock().await[0].clone();
    let child = session
        .collaborate(
            "child",
            session.head().await.state_revision,
            &root,
            Action::Spawn {
                task: work("child task"),
            },
        )
        .await?
        .assigned_ids["agent_id"]
        .clone();
    session.drive().await?;
    session.disconnect().await;
    let mut store = source.store.lock().await.clone();
    let index = store
        .batches
        .iter()
        .position(|batch| {
            batch.decode(&store.limits).is_ok_and(|payload| {
                payload.events.iter().any(|event| {
                    event.kind == "model.attempt.outcome" && event.agent_id.as_ref() == Some(&child)
                })
            })
        })
        .ok_or("child outcome")?;
    store.batches.truncate(index + 1);
    let batch = store.batches.last().ok_or("batch")?;
    store.head = store
        .acknowledgements
        .get(&batch.identity.batch_id)
        .ok_or("ack")?
        .head();
    store
        .acknowledgements
        .retain(|_, ack| ack.state_revision <= store.head.state_revision);
    store.grant.execution_epoch += 1;
    store.grant.core_instance_id = "replacement".into();
    let harness = Arc::new(Harness::new(None, Some("model.output.applied")));
    *harness.store.lock().await = store;
    let mut request = super::recovery::request(&*harness.store.lock().await, true)?;
    request.active_time.as_mut().ok_or("clock")?.active_ms = 599_000;
    request.tools.push(observed(&command, ToolStatus::Running));
    let target = harness.clone();
    let restoring = tokio::spawn(async move {
        super::recovery::restore(request, target, Vec::new())
            .await
            .map_err(|error| error.to_string())
    });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    let observer = harness
        .restoration_activity
        .lock()
        .await
        .clone()
        .ok_or("observer")?;
    observer
        .stopped(
            &command.invocation_id,
            &command.attempt_id,
            std::time::Instant::now(),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    harness.resume.add_permits(1);
    let (session, executor) = restoring.await??;
    session
        .signals(
            "clock-after-output",
            signal_update(&session, Vec::new()).await,
        )
        .await?;
    let state = session.snapshot().await;
    let run = state.run.as_ref().ok_or("run")?;
    assert!(run.active_ms < 600_000 && run.resource_error.is_none());
    assert!(
        state.agents[&child]
            .turn
            .as_ref()
            .ok_or("child turn")?
            .steps
            .last()
            .ok_or("step")?
            .settled
    );
    assert!(
        state.root_turn().ok_or("root")?.invocations[0]
            .tool_observations
            .values()
            .any(|observation| observation.status == ToolStatus::Stopped)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert!(harness.sent.lock().await.is_empty());
    Ok(())
}

struct FailedDrain(Arc<Harness>);

#[async_trait]
impl HarnessPort for FailedDrain {
    async fn read_artifact(
        &self,
        reference: &ArtifactRef,
        offset: u64,
        max_bytes: u64,
    ) -> Result<Vec<u8>, CoreError> {
        self.0.read_artifact(reference, offset, max_bytes).await
    }

    async fn commit(&self, batch: CheckpointBatch) -> Result<CheckpointAck, CoreError> {
        self.0.commit(batch).await
    }
    async fn send(&self, message: ServerMessage) -> Result<(), CoreError> {
        self.0.send(message).await
    }
    async fn synchronize_restoration(
        &self,
        _observer: bitrouter_orchestrator::core::session::restoration_activity::RestorationActivity,
    ) -> Result<(), CoreError> {
        Err(CoreError::rejected(
            ErrorCode::RecoveryRequired,
            "lifecycle bridge cannot establish a drain",
        ))
    }
}

#[tokio::test]
async fn recovery_time_failed_drain_reports_the_committed_restore_without_dispatch() -> TestResult {
    let source = super::recovery::completed_store().await?;
    let harness =
        super::recovery::harness_at(super::recovery::prefix(&source, "input.accepted")?).await;
    let before = harness.store.lock().await.batches.len();
    let request = super::recovery::request(&*harness.store.lock().await, true)?;
    let app = Arc::new(App::builder().build()?);
    let caps = Capabilities {
        version: 1,
        core_instance_id: "replacement".into(),
        operations: Vec::new(),
        transports: vec!["in_process".into()],
        unsupported_features: Vec::new(),
        limits: Limits::default(),
        max_sessions: 16,
        max_host_model_attempts: 16,
    };
    let error = CoreSession::restore(
        request,
        &caps,
        app,
        CallerContext::local(),
        Arc::new(FailedDrain(harness.clone())),
    )
    .await
    .err()
    .ok_or("failed drain accepted")?;
    assert_eq!(error.code, ErrorCode::RecoveryRequired);
    assert_eq!(error.commit_status, CommitStatus::Committed);
    assert_eq!(harness.store.lock().await.batches.len(), before + 1);
    assert_eq!(
        harness.committed_kinds().await?.last().map(String::as_str),
        Some("session.restored")
    );
    assert!(harness.sent.lock().await.is_empty());
    Ok(())
}
