use super::*;
use bitrouter_orchestrator::core::protocol::{
    Restore, RunActivityReconciliation, ToolObservation, ToolStatus,
};
use bitrouter_orchestrator::core::session::AgentStatus;

pub(super) fn capabilities(owner: &str) -> Capabilities {
    Capabilities {
        version: 1,
        core_instance_id: owner.into(),
        operations: Vec::new(),
        transports: vec!["in_process".into()],
        unsupported_features: Vec::new(),
        limits: Limits::default(),
        max_sessions: 16,
        max_host_model_attempts: 16,
    }
}

pub(super) fn application(
    responses: Vec<MockResponse>,
) -> Result<(Arc<App>, Arc<RecordingExecutor>), Box<dyn std::error::Error>> {
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first")]);
    let executor = Arc::new(RecordingExecutor {
        mock: MockExecutor::new(responses),
        agent_once: Mutex::new(Default::default()),
        prompts: Mutex::new(Vec::new()),
        calls: AtomicUsize::new(0),
    });
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone());
        })
        .build()?;
    Ok((Arc::new(app), executor))
}

pub(super) async fn completed_store() -> Result<DurableHarness, Box<dyn std::error::Error>> {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(
        vec![output(vec![call("read")]), output(vec![text("finished")])],
        harness.clone(),
        false,
    )
    .await?;
    session
        .start("input", session.head().await.state_revision, input())
        .await?;
    session.drive().await?;
    let command = harness
        .sent
        .lock()
        .await
        .first()
        .ok_or("missing tool")?
        .clone();
    session.tool_result("tool-result", result(&command)).await?;
    assert_eq!(
        session.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    session.disconnect().await;
    Ok(harness.store.lock().await.clone())
}

pub(super) fn prefix(
    store: &DurableHarness,
    kind: &str,
) -> Result<DurableHarness, Box<dyn std::error::Error>> {
    let index = store
        .batches
        .iter()
        .position(|batch| {
            batch
                .decode(&store.limits)
                .is_ok_and(|payload| payload.events.iter().any(|event| event.kind == kind))
        })
        .ok_or_else(|| format!("missing checkpoint {kind}"))?;
    let mut store = store.clone();
    store.batches.truncate(index + 1);
    let last = store.batches.last().ok_or("missing last batch")?;
    store.head = store
        .acknowledgements
        .get(&last.identity.batch_id)
        .ok_or("missing ACK")?
        .head();
    store
        .acknowledgements
        .retain(|_, ack| ack.state_revision <= store.head.state_revision);
    Ok(store)
}

pub(super) fn request(
    store: &DurableHarness,
    tail: bool,
) -> Result<Restore, Box<dyn std::error::Error>> {
    let last = store.batches.last().ok_or("missing checkpoint")?;
    let state: SessionSnapshot =
        serde_json::from_value(last.decode(&store.limits)?.checkpoint.state)?;
    Ok(Restore {
        binding: Bind {
            grant: store.grant.clone(),
            durable_head: store.head.clone(),
            checkpoint: Some(if tail {
                store.batches.first().ok_or("missing anchor")?.clone()
            } else {
                last.clone()
            }),
            manifest: state.manifest,
            limits: store.limits.clone(),
        },
        journal_tail: if tail {
            store.batches.iter().skip(1).cloned().collect()
        } else {
            Vec::new()
        },
        tools: Vec::new(),
        results: Vec::new(),
        available_artifacts: store.artifacts.values().cloned().collect(),
        previous_owner_stopped: true,
        // This deterministic fixture certifies no additional unrecorded work.
        // Clock-reconciliation tests supply their explicit cumulative intervals.
        active_time: state.run.map(|run| RunActivityReconciliation {
            run_id: run.run_id,
            durable_head: store.head.clone(),
            active_ms: run.active_ms,
        }),
    })
}

pub(super) async fn harness_at(mut store: DurableHarness) -> Arc<Harness> {
    store.grant.execution_epoch += 1;
    store.grant.core_instance_id = "replacement".into();
    let harness = Arc::new(Harness::new(None, None));
    *harness.store.lock().await = store;
    harness
}

pub(super) async fn restore(
    request: Restore,
    harness: Arc<dyn HarnessPort>,
    responses: Vec<MockResponse>,
) -> Result<(CoreSession, Arc<RecordingExecutor>), Box<dyn std::error::Error>> {
    let (app, executor) = application(responses)?;
    let caps = capabilities(&request.binding.grant.core_instance_id);
    let session =
        CoreSession::restore(request, &caps, app, CallerContext::local(), harness).await?;
    Ok((session, executor))
}

#[tokio::test]
async fn interrupted_registration_preserves_the_original_restore_event() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![], harness.clone(), false).await?;
    session.start_response("input", 1, input()).await?;
    session.disconnect().await;
    let replacement = harness_at(harness.store.lock().await.clone()).await;
    let request = request(&*replacement.store.lock().await, true)?;
    let base = request.binding.durable_head.state_revision;
    let (app, executor) = application(vec![])?;
    let caps = capabilities(&request.binding.grant.core_instance_id);
    let port = replacement.clone();
    let (published, retained) = tokio::sync::oneshot::channel();
    let pending = tokio::spawn(async move {
        CoreSession::restore_registered(
            request,
            &caps,
            app,
            CallerContext::local(),
            http::HeaderMap::new(),
            port,
            move |session| async move {
                let _ = published.send(session);
                std::future::pending::<Result<(), CoreError>>().await
            },
        )
        .await
    });
    let restored = tokio::time::timeout(Duration::from_secs(5), retained).await??;
    assert_eq!(restored.head().await.state_revision, base);
    pending.abort();
    assert!(pending.await.is_err());
    reconnect::reconnect(&restored, &replacement).await?;
    let kinds = replacement.committed_kinds().await?;
    let initialized = kinds
        .iter()
        .position(|kind| kind == "session.restored")
        .ok_or("restore event")?;
    let reconnected = kinds
        .iter()
        .position(|kind| kind == "session.reconnected")
        .ok_or("reconnect event")?;
    assert!(initialized < reconnected);
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| *kind == "session.restored")
            .count(),
        1
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    restored.disconnect().await;
    let next = harness_at(replacement.store.lock().await.clone()).await;
    let next_request = self::request(&*next.store.lock().await, true)?;
    let (validated, executor) = restore(next_request, next, vec![]).await?;
    assert_eq!(
        validated
            .snapshot()
            .await
            .run
            .ok_or("run")?
            .activity_reconciliations
            .len(),
        2
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

fn observation(command: &ToolExecute, status: ToolStatus) -> ToolObservation {
    ToolObservation {
        invocation_id: command.invocation_id.clone(),
        attempt_id: command.attempt_id.clone(),
        status,
        evidence: Vec::new(),
    }
}

fn pending(store: &DurableHarness) -> Result<ToolExecute, Box<dyn std::error::Error>> {
    let state: SessionSnapshot = serde_json::from_value(
        store
            .batches
            .last()
            .ok_or("missing batch")?
            .decode(&store.limits)?
            .checkpoint
            .state,
    )?;
    Ok(state
        .root_turn()
        .ok_or("missing turn")?
        .invocations
        .first()
        .ok_or("missing invocation")?
        .dispatch
        .clone())
}

#[tokio::test]
async fn recovery_replays_committed_output_and_never_confirmed_effects() -> TestResult {
    let store = completed_store().await?;
    for kind in [
        "model.attempt.outcome",
        "model.output.applied",
        "tool.result",
        "tool.results.consumed",
        "run.completed",
    ] {
        let harness = harness_at(prefix(&store, kind)?).await;
        let mut restore_input = request(&*harness.store.lock().await, true)?;
        let old_command = if kind == "model.output.applied" {
            let command = pending(&*harness.store.lock().await)?;
            restore_input
                .tools
                .push(observation(&command, ToolStatus::NotStarted));
            Some(command)
        } else {
            None
        };
        let (session, executor) = restore(
            restore_input,
            harness.clone(),
            vec![output(vec![text("continued")])],
        )
        .await?;
        let state = session.drive().await?;
        if matches!(kind, "model.attempt.outcome" | "model.output.applied") {
            assert_eq!(executor.calls.load(Ordering::SeqCst), 0, "{kind}");
            assert_eq!(
                state.run.as_ref().map(|run| run.status),
                Some(RunStatus::Waiting)
            );
            let command = harness
                .sent
                .lock()
                .await
                .first()
                .ok_or("missing restored command")?
                .clone();
            assert_eq!(command.execution_epoch, 2);
            if let Some(old) = old_command {
                assert_eq!(old.invocation_id, command.invocation_id);
                assert_eq!(old.attempt_id, command.attempt_id);
            }
            session
                .tool_result("continued-tool", result(&command))
                .await?;
            assert_eq!(
                session.drive().await?.run.as_ref().map(|run| run.status),
                Some(RunStatus::Completed)
            );
        } else {
            assert!(harness.sent.lock().await.is_empty(), "{kind}");
            assert_eq!(
                state.run.as_ref().map(|run| run.status),
                Some(RunStatus::Completed),
                "{kind}"
            );
            assert_eq!(
                executor.calls.load(Ordering::SeqCst),
                usize::from(kind != "run.completed"),
                "{kind}"
            );
            let receipt = session
                .operation("tool-result")
                .await
                .ok_or("lost receipt")?;
            let command = pending(&*harness.store.lock().await)?;
            let replay = session.tool_result("tool-result", result(&command)).await?;
            assert_eq!(receipt, replay);
        }
    }
    Ok(())
}

#[tokio::test]
async fn recovery_closes_interrupted_work_with_new_attempt_and_preserved_unknown_spend()
-> TestResult {
    use bitrouter_orchestrator::core::accounting::work::{CostWorkKind, CostWorkState};
    let store = completed_store().await?;
    for kind in [
        "input.accepted",
        "model.step.preparing",
        "model.attempt.intent",
    ] {
        let harness = harness_at(prefix(&store, kind)?).await;
        let restore_input = request(&*harness.store.lock().await, false)?;
        let (session, executor) = restore(
            restore_input,
            harness,
            vec![output(vec![text("replacement output")])],
        )
        .await?;
        let state = session.drive().await?;
        let run = state.run.as_ref().ok_or("missing run")?;
        assert_eq!(run.status, RunStatus::Completed, "{kind}");
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        let steps = &state.root_turn().ok_or("missing turn")?.steps;
        if kind != "input.accepted" {
            assert!(steps[0].interrupted && steps[0].settled);
            assert_ne!(steps[0].step_id, steps[1].step_id);
        }
        if kind == "model.attempt.intent" {
            assert_eq!(run.model_attempts, 2);
            assert!(steps[0].attempts[0].receipt.is_none());
            assert_ne!(
                steps[0].attempts[0].attempt_id,
                steps[1].attempts[0].attempt_id
            );
            let ledger = state
                .cost_work
                .get(&run.run_id)
                .ok_or("missing cost ledger")?;
            let prior = ledger
                .work
                .get(&steps[0].attempts[0].attempt_id)
                .ok_or("missing uncertain spend")?;
            assert_eq!(prior.kind, CostWorkKind::ProviderAttempt);
            assert_eq!(prior.state, CostWorkState::IntentRecorded);
            assert!(
                run.token_accounting
                    .as_ref()
                    .ok_or("missing accounting")?
                    .complete_estimate_micro_usd(run.model_attempts)
                    .is_none()
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn recovery_unknown_writes_and_shells_block_until_harness_supplies_result() -> TestResult {
    for effect in [ToolEffect::Write, ToolEffect::Shell] {
        let original = Arc::new(Harness::new(None, None));
        let (session, _, _) =
            setup(vec![output(vec![call("effect")])], original.clone(), false).await?;
        let mut update = signal_update(&session, Vec::new()).await;
        update.manifest.tools[0].effect = effect;
        update.manifest.tool_manifest_digest = HarnessManifest::digest(&update.manifest.tools)?;
        session.signals("effect-manifest", update).await?;
        session
            .start("input", session.head().await.state_revision, input())
            .await?;
        session.drive().await?;
        session.disconnect().await;
        let harness = harness_at(original.store.lock().await.clone()).await;
        let command = pending(&*harness.store.lock().await)?;
        let restore_input = request(&*harness.store.lock().await, false)?;
        let (restored, executor) = restore(
            restore_input,
            harness.clone(),
            vec![output(vec![text("must not run")])],
        )
        .await?;
        assert_eq!(
            restored.drive().await?.run.as_ref().map(|run| run.status),
            Some(RunStatus::RecoveryRequired)
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        assert!(harness.sent.lock().await.is_empty());
        restored.disconnect().await;
        let harness = harness_at(harness.store.lock().await.clone()).await;
        let mut restore_input = request(&*harness.store.lock().await, true)?;
        restore_input.results.push(result(&command));
        let (restored, executor) = restore(
            restore_input,
            harness.clone(),
            vec![output(vec![text("confirmed effect")])],
        )
        .await?;
        assert_eq!(
            restored.drive().await?.run.as_ref().map(|run| run.status),
            Some(RunStatus::Completed)
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert!(harness.sent.lock().await.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn recovery_retains_running_tools_and_pending_approval_without_redispatch() -> TestResult {
    let store = prefix(&completed_store().await?, "model.output.applied")?;
    for status in [ToolStatus::Running, ToolStatus::WaitingApproval] {
        let harness = harness_at(store.clone()).await;
        let command = pending(&*harness.store.lock().await)?;
        let mut restore_input = request(&*harness.store.lock().await, false)?;
        restore_input.tools.push(observation(&command, status));
        let (session, executor) = restore(
            restore_input,
            harness.clone(),
            vec![output(vec![text("done")])],
        )
        .await?;
        assert_eq!(
            session.drive().await?.run.as_ref().map(|run| run.status),
            Some(RunStatus::Waiting)
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        assert!(harness.sent.lock().await.is_empty());
        assert_eq!(
            session
                .snapshot()
                .await
                .root_turn()
                .ok_or("missing turn")?
                .invocations[0]
                .recovery_observation
                .as_ref()
                .map(|value| value.status),
            Some(status)
        );
        session.tool_result("result", result(&command)).await?;
        assert_eq!(
            session.drive().await?.run.as_ref().map(|run| run.status),
            Some(RunStatus::Completed)
        );
    }
    Ok(())
}

#[tokio::test]
async fn recovery_adopts_lost_input_ack_and_requires_matching_restore_ack() -> TestResult {
    let mut fixture = Harness::new(None, None);
    fixture.wrong_ack = Some(true);
    let original = Arc::new(fixture);
    let (session, _, _) = setup(Vec::new(), original.clone(), false).await?;
    let revision = session.head().await.state_revision;
    assert_eq!(
        session
            .start("input", revision, input())
            .await
            .err()
            .ok_or("expected unknown commit")?
            .commit_status,
        CommitStatus::Unknown
    );
    let mut store = original.store.lock().await.clone();
    let original_count = store.batches.len();
    store.grant.execution_epoch = 2;
    store.grant.core_instance_id = "replacement".into();
    let held = Arc::new(Harness::new(None, Some("session.restored")));
    *held.store.lock().await = store;
    let restore_input = request(&*held.store.lock().await, true)?;
    let (app, executor) = application(vec![output(vec![text("done")])])?;
    let target = held.clone();
    let pending_restore = tokio::spawn(async move {
        CoreSession::restore(
            restore_input,
            &capabilities("replacement"),
            app,
            CallerContext::local(),
            target,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), held.seen.acquire())
        .await??
        .forget();
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert!(!pending_restore.is_finished());
    held.resume.add_permits(1);
    let restored = pending_restore.await??;
    let receipt = restored.start("input", revision, input()).await?;
    assert_eq!(receipt.state_revision, revision + 1);
    assert_eq!(held.store.lock().await.batches.len(), original_count + 1);
    assert_eq!(
        restored.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    Ok(())
}

#[tokio::test]
async fn recovery_rejects_broken_chains_stale_owners_and_unreconciled_io() -> TestResult {
    let store = completed_store().await?;
    for case in [
        "gap",
        "digest",
        "head",
        "session",
        "same_epoch",
        "owner_running",
        "unknown_tool",
        "conflicting_result",
    ] {
        let harness = harness_at(store.clone()).await;
        let mut restore_input = request(&*harness.store.lock().await, true)?;
        let code = match case {
            "gap" => {
                restore_input.journal_tail.remove(0);
                ErrorCode::CheckpointConflict
            }
            "digest" => {
                restore_input.journal_tail[0].payload_sha256 = "0".repeat(64);
                ErrorCode::CheckpointConflict
            }
            "head" => {
                restore_input.binding.durable_head.state_revision += 1;
                restore_input.binding.durable_head.event_seq += 1;
                ErrorCode::CheckpointConflict
            }
            "session" => {
                restore_input.binding.grant.session_id = "other_session".into();
                ErrorCode::UnauthorizedScope
            }
            "same_epoch" => {
                restore_input.binding.grant.execution_epoch = 1;
                ErrorCode::StaleEpoch
            }
            "owner_running" => {
                restore_input.previous_owner_stopped = false;
                ErrorCode::RecoveryRequired
            }
            "unknown_tool" => {
                restore_input.tools.push(ToolObservation {
                    invocation_id: "unknown".into(),
                    attempt_id: "unknown_attempt".into(),
                    status: ToolStatus::NotStarted,
                    evidence: Vec::new(),
                });
                ErrorCode::InvalidToolResult
            }
            _ => {
                let mut value = result(&pending(&*harness.store.lock().await)?);
                value.output = "conflicting result".into();
                restore_input.results.push(value);
                ErrorCode::OperationConflict
            }
        };
        let (app, executor) = application(Vec::new())?;
        let caps = capabilities(&restore_input.binding.grant.core_instance_id);
        let error = CoreSession::restore(
            restore_input,
            &caps,
            app,
            CallerContext::local(),
            harness.clone(),
        )
        .await
        .err()
        .ok_or("restore unexpectedly accepted")?;
        assert_eq!(error.code, code, "{case}");
        assert_eq!(
            harness.store.lock().await.batches.len(),
            store.batches.len(),
            "{case}"
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    }
    Ok(())
}

#[tokio::test]
async fn recovery_restores_cancellation_and_cancels_running_tool_under_current_epoch() -> TestResult
{
    let original = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![call("read")])], original.clone(), false).await?;
    let receipt = session
        .start("input", session.head().await.state_revision, input())
        .await?;
    session.drive().await?;
    session
        .cancel_run(
            "cancel",
            session.head().await.state_revision,
            receipt.assigned_ids.get("run_id").ok_or("run missing")?,
        )
        .await?;
    session.disconnect().await;
    for status in [ToolStatus::NotStarted, ToolStatus::Running] {
        let harness = harness_at(original.store.lock().await.clone()).await;
        let command = pending(&*harness.store.lock().await)?;
        let mut restore_input = request(&*harness.store.lock().await, false)?;
        restore_input.tools.push(observation(&command, status));
        let (restored, executor) = restore(restore_input, harness.clone(), Vec::new()).await?;
        let state = restored.drive().await?;
        if status == ToolStatus::Running {
            assert_eq!(
                state.run.as_ref().map(|run| run.status),
                Some(RunStatus::Cancelling)
            );
            tokio::time::timeout(Duration::from_secs(5), harness.cancel_seen.acquire())
                .await??
                .forget();
            assert_eq!(harness.cancelled.lock().await[0].2, 2);
            restored.tool_result("ended", result(&command)).await?;
        }
        assert_eq!(
            restored.drive().await?.run.as_ref().map(|run| run.status),
            Some(RunStatus::Cancelled)
        );
        assert!(harness.sent.lock().await.is_empty());
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            restored
                .snapshot()
                .await
                .root_turn()
                .map(|turn| turn.status),
            Some(AgentStatus::Interrupted)
        );
    }
    Ok(())
}

#[tokio::test]
async fn recovery_requires_artifact_bytes_and_complete_checkpoint_dependencies() -> TestResult {
    let original = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![call("read")])], original.clone(), false).await?;
    session
        .start("input", session.head().await.state_revision, input())
        .await?;
    session.drive().await?;
    let command = original
        .sent
        .lock()
        .await
        .first()
        .ok_or("missing command")?
        .clone();
    let artifact = ArtifactRef {
        artifact_id: "tool_evidence".into(),
        sha256: sha256(b"evidence"),
        bytes: 8,
        media_type: "text/plain".into(),
    };
    original
        .store
        .lock()
        .await
        .put_artifact(artifact.clone(), b"evidence")?;
    let mut outcome = result(&command);
    outcome.evidence.push(artifact);
    session.tool_result("result", outcome).await?;
    session.disconnect().await;
    for case in ["missing", "corrupt", "omitted_dependency"] {
        let harness = harness_at(original.store.lock().await.clone()).await;
        let mut restore_input = request(&*harness.store.lock().await, false)?;
        let code = if case == "omitted_dependency" {
            let batch = restore_input
                .binding
                .checkpoint
                .as_ref()
                .ok_or("missing checkpoint")?;
            let mut payload = batch.decode(&Limits::default())?;
            payload.checkpoint.artifact_refs.clear();
            let batch = CheckpointBatch::encode(&payload, &Limits::default())?;
            restore_input.binding.durable_head.payload_sha256 = Some(batch.payload_sha256.clone());
            restore_input.binding.checkpoint = Some(batch);
            ErrorCode::CheckpointConflict
        } else {
            if case == "missing" {
                restore_input.available_artifacts.clear();
            } else {
                restore_input.available_artifacts[0].bytes += 1;
            }
            ErrorCode::ArtifactUnavailable
        };
        let (app, executor) = application(Vec::new())?;
        let error = CoreSession::restore(
            restore_input,
            &capabilities("replacement"),
            app,
            CallerContext::local(),
            harness,
        )
        .await
        .err()
        .ok_or("missing evidence accepted")?;
        assert_eq!(error.code, code, "{case}");
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    }
    Ok(())
}

#[tokio::test]
async fn recovery_preserves_exhausted_attempt_budget_and_failed_terminal_run() -> TestResult {
    let original = Arc::new(Harness::new(None, Some("model.attempt.intent")));
    let (session, executor, _) = setup(
        vec![output(vec![text("not dispatched")])],
        original.clone(),
        false,
    )
    .await?;
    let mut task = input();
    task.limits = Some(Limits {
        model_attempts: 1,
        ..Limits::default()
    });
    session
        .start("input", session.head().await.state_revision, task)
        .await?;
    let driver = session.clone();
    let driving = tokio::spawn(async move { driver.drive().await });
    tokio::time::timeout(Duration::from_secs(5), original.seen.acquire())
        .await??
        .forget();
    // Crash before intent ACK: the harness must not claim that this intent was
    // persisted. Here release it, then stop on the next provider boundary using
    // the recorded durable prefix for the replacement process.
    original.resume.add_permits(1);
    driving.await??;
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    session.disconnect().await;
    let store = prefix(&original.store.lock().await.clone(), "model.attempt.intent")?;
    let harness = harness_at(store).await;
    let restore_input = request(&*harness.store.lock().await, false)?;
    let (restored, executor) = restore(restore_input, harness.clone(), Vec::new()).await?;
    let state = restored.drive().await?;
    assert_eq!(
        state.run.as_ref().map(|run| run.status),
        Some(RunStatus::Failed)
    );
    assert_eq!(state.run.as_ref().map(|run| run.model_attempts), Some(1));
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    restored.disconnect().await;
    let harness = harness_at(harness.store.lock().await.clone()).await;
    let restore_input = request(&*harness.store.lock().await, false)?;
    let (restored, executor) = restore(restore_input, harness, Vec::new()).await?;
    assert_eq!(
        restored.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Failed)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn recovery_workspace_change_revokes_previously_prepared_tool_calls() -> TestResult {
    let original = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![call("write")])], original.clone(), false).await?;
    let mut update = signal_update(&session, Vec::new()).await;
    update.manifest.workspace_revision = Some("v1".into());
    update.workspace_revision = Some("v1".into());
    update.manifest.tools[0].effect = ToolEffect::Write;
    update.manifest.tool_manifest_digest = HarnessManifest::digest(&update.manifest.tools)?;
    session.signals("workspace", update).await?;
    session
        .start("input", session.head().await.state_revision, input())
        .await?;
    session.drive().await?;
    session.disconnect().await;
    let store = original.store.lock().await.clone();
    for kind in ["model.attempt.outcome", "model.output.applied"] {
        for version in [Some("v2".to_owned()), None] {
            let harness = harness_at(prefix(&store, kind)?).await;
            let mut restore_input = request(&*harness.store.lock().await, false)?;
            restore_input.binding.manifest.workspace_revision = version;
            if kind == "model.output.applied" {
                restore_input.tools.push(observation(
                    &pending(&*harness.store.lock().await)?,
                    ToolStatus::NotStarted,
                ));
            }
            let (restored, _) = restore(
                restore_input,
                harness.clone(),
                vec![output(vec![text("old write revoked")])],
            )
            .await?;
            let state = restored.drive().await?;
            assert!(harness.sent.lock().await.is_empty());
            assert_eq!(
                state.root_turn().ok_or("missing turn")?.invocations[0]
                    .result
                    .as_ref()
                    .map(|result| result.status),
                Some(ToolOutcome::Denied)
            );
            assert_eq!(
                state.run.as_ref().map(|run| run.status),
                Some(RunStatus::Completed)
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn recovery_reconciles_effect_unknown_without_erasing_original_evidence() -> TestResult {
    let original = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![call("read")])], original.clone(), false).await?;
    session
        .start("input", session.head().await.state_revision, input())
        .await?;
    session.drive().await?;
    let command = original
        .sent
        .lock()
        .await
        .first()
        .ok_or("missing command")?
        .clone();
    let mut uncertain = result(&command);
    uncertain.status = ToolOutcome::EffectUnknown;
    uncertain.output = "process ended without a reliable tool outcome".into();
    session.tool_result("uncertain", uncertain.clone()).await?;
    session.disconnect().await;
    let harness = harness_at(original.store.lock().await.clone()).await;
    let mut restore_input = request(&*harness.store.lock().await, false)?;
    restore_input.results.push(result(&command));
    let (restored, _) = restore(
        restore_input,
        harness.clone(),
        vec![output(vec![text("reconciled")])],
    )
    .await?;
    let state = restored.drive().await?;
    assert_eq!(
        state.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    let call = &state.root_turn().ok_or("missing turn")?.invocations[0];
    assert_eq!(call.prior_uncertain_result.as_ref(), Some(&uncertain));
    assert_eq!(call.result.as_ref(), Some(&result(&command)));
    assert!(call.consumed);
    assert!(harness.sent.lock().await.is_empty());
    assert!(restored.operation("uncertain").await.is_some());
    Ok(())
}

#[tokio::test]
async fn recovery_preserves_subtree_cancellation_across_unknown_effect_reconciliation() -> TestResult
{
    let original = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(
        vec![output(vec![text("root answer")])],
        original.clone(),
        false,
    )
    .await?;
    let root = session
        .start("input", session.head().await.state_revision, input())
        .await?
        .assigned_ids
        .get("agent_id")
        .cloned()
        .ok_or("root missing")?;
    let child = session
        .collaborate(
            "spawn",
            session.head().await.state_revision,
            &root,
            Action::Spawn {
                task: work("child"),
            },
        )
        .await?
        .assigned_ids
        .get("agent_id")
        .cloned()
        .ok_or("child missing")?;
    executor
        .agent_once
        .lock()
        .await
        .insert(child.clone(), vec![call("child-read")]);
    session.drive().await?;
    let command = original
        .sent
        .lock()
        .await
        .iter()
        .find(|call| call.agent_id == child)
        .ok_or("missing child command")?
        .clone();
    session
        .collaborate(
            "interrupt",
            session.head().await.state_revision,
            &root,
            Action::Interrupt {
                agent_id: child.clone(),
            },
        )
        .await?;
    session.disconnect().await;
    let harness = harness_at(original.store.lock().await.clone()).await;
    let restore_input = request(&*harness.store.lock().await, false)?;
    let (restored, executor) = restore(restore_input, harness.clone(), Vec::new()).await?;
    assert_eq!(
        restored.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::RecoveryRequired)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    restored.disconnect().await;
    let harness = harness_at(harness.store.lock().await.clone()).await;
    let mut restore_input = request(&*harness.store.lock().await, false)?;
    restore_input.results.push(result(&command));
    let (restored, executor) = restore(
        restore_input,
        harness.clone(),
        vec![output(vec![text("root after child cleanup")])],
    )
    .await?;
    let state = restored.drive().await?;
    let turn = state
        .agents
        .get(&child)
        .and_then(|agent| agent.turn.as_ref())
        .ok_or("child lost")?;
    assert!(turn.cancellation_requested);
    assert_eq!(turn.status, AgentStatus::Interrupted);
    assert_eq!(turn.steps.len(), 1);
    assert!(
        executor
            .prompts
            .lock()
            .await
            .iter()
            .all(|prompt| prompt.system.as_ref().is_none_or(
                |system| !system.starts_with(&format!("You are agent {child} for this task."))
            ))
    );
    assert!(harness.sent.lock().await.is_empty());
    Ok(())
}
