use super::*;
use bitrouter_orchestrator::core::checkpoint::ToolStartFence;
use bitrouter_orchestrator::core::protocol::{ToolObservation, ToolStatus};
use bitrouter_orchestrator::core::session::AgentStatus;

fn task() -> TaskInput {
    TaskInput {
        limits: Some(Limits {
            active_seconds: 1,
            ..Limits::default()
        }),
        ..input()
    }
}

fn observed(command: &ToolExecute, status: ToolStatus) -> ToolObservation {
    ToolObservation {
        invocation_id: command.invocation_id.clone(),
        attempt_id: command.attempt_id.clone(),
        status,
        evidence: Vec::new(),
    }
}

fn fence(command: &ToolExecute) -> ToolStartFence {
    ToolStartFence {
        invocation_id: command.invocation_id.clone(),
        attempt_id: command.attempt_id.clone(),
    }
}

async fn reached(semaphore: &Semaphore) -> TestResult {
    tokio::time::timeout(Duration::from_secs(30), semaphore.acquire())
        .await??
        .forget();
    Ok(())
}

async fn limited(session: &CoreSession) -> Result<SessionSnapshot, Box<dyn std::error::Error>> {
    Ok(tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let state = session.snapshot().await;
            if state
                .run
                .as_ref()
                .is_some_and(|run| run.resource_error.is_some())
            {
                break state;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?)
}

async fn drive(session: &CoreSession) -> Result<SessionSnapshot, Box<dyn std::error::Error>> {
    Ok(tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match session.drive().await {
                Err(error) if error.code == ErrorCode::Busy => tokio::task::yield_now().await,
                result => break result,
            }
        }
    })
    .await??)
}

#[tokio::test]
async fn active_budget_fences_approvals_then_cleans_up_and_preserves_failure_over_cancel()
-> TestResult {
    let mut fixture = Harness::new(None, Some("run.limit_reached"));
    fixture.wait_for_approval = true;
    let harness = Arc::new(fixture);
    let (session, executor, _) = setup(
        vec![
            output(vec![call("one"), call("two")]),
            output(vec![text("next run")]),
        ],
        harness.clone(),
        false,
    )
    .await?;
    let first = session.start("first", 1, task()).await?;
    session
        .enqueue("next", session.head().await.state_revision, input())
        .await?;
    session.drive().await?;
    let commands = harness.sent.lock().await.clone();
    assert_eq!(commands.len(), 2);
    session
        .tool_status(
            "approval",
            observed(&commands[1], ToolStatus::WaitingApproval),
        )
        .await?;
    assert!(
        harness
            .store
            .lock()
            .await
            .try_start_tool(fence(&commands[0]))
    );
    session
        .tool_status("running", observed(&commands[0], ToolStatus::Running))
        .await?;
    reached(&harness.seen).await?;
    assert!(
        session
            .snapshot()
            .await
            .run
            .ok_or("run")?
            .resource_error
            .is_none()
    );
    assert!(harness.cancelled.lock().await.is_empty());
    harness.hold_enabled.store(false, Ordering::SeqCst);
    harness.resume.add_permits(1);
    let state = limited(&session).await?;
    assert_eq!(
        state.run.as_ref().map(|run| run.status),
        Some(RunStatus::Cancelling)
    );
    assert!(state.root_queue.paused);
    for command in &commands {
        assert!(
            harness
                .store
                .lock()
                .await
                .tool_start_fences
                .contains(&fence(command))
        );
    }
    assert!(
        !harness
            .store
            .lock()
            .await
            .try_start_tool(fence(&commands[1]))
    );
    reached(&harness.cancel_seen).await?;
    reached(&harness.cancel_seen).await?;
    drive(&session).await?;
    session
        .cancel_run(
            "late-cancel",
            session.head().await.state_revision,
            &first.assigned_ids["run_id"],
        )
        .await?;
    session.tool_result("actual", result(&commands[0])).await?;
    let mut prevented = result(&commands[1]);
    prevented.status = ToolOutcome::NotExecuted;
    prevented.output.clear();
    session.tool_result("prevented", prevented).await?;
    let done = drive(&session).await?;
    let run = done.run.as_ref().ok_or("run")?;
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(
        run.resource_error
            .as_ref()
            .map(|error| (error.code, error.commit_status)),
        Some((ErrorCode::LimitExceeded, CommitStatus::Committed))
    );
    assert!(
        done.root_turn()
            .ok_or("turn")?
            .invocations
            .iter()
            .all(|call| call.consumed)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(done.root_queue.pending.len(), 1);
    session
        .resume_queue("resume", session.head().await.state_revision)
        .await?;
    let next = drive(&session).await?;
    assert_eq!(
        next.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert!(
        next.run
            .as_ref()
            .is_some_and(|run| run.resource_error.is_none() && run.active_ms < 1000)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn active_budget_excludes_pure_approval_and_stopped_status_ack_waits() -> TestResult {
    let mut fixture = Harness::new(None, Some("tool.status"));
    fixture.wait_for_approval = true;
    fixture.hold_enabled.store(false, Ordering::SeqCst);
    let harness = Arc::new(fixture);
    let (session, _, _) = setup(
        vec![output(vec![call("read")]), output(vec![text("done")])],
        harness.clone(),
        false,
    )
    .await?;
    session.start("input", 1, task()).await?;
    session.drive().await?;
    let command = harness.sent.lock().await[0].clone();
    session
        .tool_status("approval", observed(&command, ToolStatus::WaitingApproval))
        .await?;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        session
            .snapshot()
            .await
            .run
            .ok_or("run")?
            .resource_error
            .is_none()
    );
    assert!(harness.store.lock().await.try_start_tool(fence(&command)));
    session
        .tool_status("running", observed(&command, ToolStatus::Running))
        .await?;
    harness.hold_enabled.store(true, Ordering::SeqCst);
    let stopping = tokio::spawn({
        let session = session.clone();
        let command = command.clone();
        async move {
            session
                .tool_status("stopped", observed(&command, ToolStatus::Stopped))
                .await
        }
    });
    reached(&harness.seen).await?;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        session
            .snapshot()
            .await
            .run
            .ok_or("run")?
            .resource_error
            .is_none()
    );
    harness.hold_enabled.store(false, Ordering::SeqCst);
    harness.resume.add_permits(1);
    stopping.await??;
    session.tool_result("result", result(&command)).await?;
    let done = drive(&session).await?;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert!(done.run.as_ref().is_some_and(|run| run.active_ms < 1000));
    Ok(())
}

#[tokio::test]
async fn active_budget_stops_unsent_tools_and_small_cleanup_results_remain_restorable() -> TestResult
{
    let mut fixture = Harness::new(None, None);
    fixture.hold_after_send = true;
    let harness = Arc::new(fixture);
    let (session, executor, _) = setup(
        vec![output(vec![call("one"), call("two")])],
        harness.clone(),
        false,
    )
    .await?;
    let mut signals = signal_update(&session, Vec::new()).await;
    signals.manifest.max_tool_output_bytes = 1;
    session.signals("small-output", signals).await?;
    session
        .start("input", session.head().await.state_revision, task())
        .await?;
    let driving = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    reached(&harness.delivered).await?;
    let command = harness.sent.lock().await[0].clone();
    session
        .tool_status("running", observed(&command, ToolStatus::Running))
        .await?;
    limited(&session).await?;
    harness.resume.add_permits(1);
    tokio::time::timeout(Duration::from_secs(30), driving).await???;
    assert_eq!(harness.sent.lock().await.len(), 1);
    let state = session.snapshot().await;
    let not_sent = &state.root_turn().ok_or("turn")?.invocations[1];
    assert_eq!(
        not_sent.result.as_ref().map(|result| result.status),
        Some(ToolOutcome::NotExecuted)
    );
    assert!(
        not_sent
            .result
            .as_ref()
            .is_some_and(|result| result.output.is_empty())
    );
    let mut actual = result(&command);
    actual.output.clear();
    session.tool_result("actual", actual).await?;
    assert_eq!(
        drive(&session).await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Failed)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    session.disconnect().await;
    let replacement = super::recovery::harness_at(harness.store.lock().await.clone()).await;
    let request = super::recovery::request(&*replacement.store.lock().await, false)?;
    let (restored, _) = super::recovery::restore(request, replacement.clone(), Vec::new()).await?;
    assert_eq!(
        restored.snapshot().await.run.as_ref().map(|run| run.status),
        Some(RunStatus::Failed)
    );
    assert!(replacement.sent.lock().await.is_empty());
    Ok(())
}

struct HeldExecutor {
    seen: Semaphore,
    resume: Semaphore,
    calls: AtomicUsize,
    mock: MockExecutor,
}
#[async_trait]
impl Executor for HeldExecutor {
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> bitrouter_sdk::Result<ExecutionResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.add_permits(1);
        self.resume
            .acquire()
            .await
            .map_err(|error| bitrouter_sdk::BitrouterError::internal(error.to_string()))?
            .forget();
        self.mock.execute(target, prompt, ctx).await
    }
    async fn execute_stream(
        &self,
        _: &RoutingTarget,
        _: &Prompt,
        _: &PipelineContext,
    ) -> bitrouter_sdk::Result<StreamPartStream> {
        Err(bitrouter_sdk::BitrouterError::internal("unexpected stream"))
    }
}

#[tokio::test]
async fn active_budget_preserves_provider_settlement_without_starting_tools_or_verification()
-> TestResult {
    for verify in [false, true] {
        let harness = Arc::new(Harness::new(None, Some("model.attempt.outcome")));
        let executor = Arc::new(HeldExecutor {
            seen: Semaphore::new(0),
            resume: Semaphore::new(0),
            calls: AtomicUsize::new(0),
            mock: MockExecutor::new(vec![output(if verify {
                vec![text("answer")]
            } else {
                vec![call("too-late")]
            })]),
        });
        let settlements = Arc::new(AtomicUsize::new(0));
        let table = StaticRoutingTable::new();
        table.insert("fixture-model", vec![target("first")]);
        let app = App::builder()
            .language_model(|builder| {
                builder
                    .routing_table(Arc::new(table))
                    .executor(executor.clone())
                    .settlement_recorder(Recorder(settlements.clone()));
            })
            .build()?;
        let session = bind_app(Arc::new(app), harness.clone()).await?;
        let mut input = task();
        if verify {
            input.verification = Some(Verification {
                tool: "read".into(),
                arguments: json!({"path":"file.txt"}),
            });
        }
        session.start("input", 1, input).await?;
        let driving = tokio::spawn({
            let session = session.clone();
            async move { session.drive().await }
        });
        reached(&executor.seen).await?;
        reached(&harness.seen).await?;
        let expired = limited(&session).await?;
        assert_eq!(
            expired.run.as_ref().map(|run| run.status),
            Some(RunStatus::Cancelling)
        );
        assert!(
            expired.root_turn().ok_or("turn")?.steps[0].attempts[0]
                .receipt
                .is_none()
        );
        assert!(!driving.is_finished());
        // Budget cancellation stopped the executor; terminal cleanup still
        // waits for its interrupted-attempt receipt and SDK settlement.
        harness.resume.add_permits(1);
        let done = tokio::time::timeout(Duration::from_secs(30), driving).await???;
        assert_eq!(
            done.run.as_ref().map(|run| run.status),
            Some(RunStatus::Failed)
        );
        let receipt = done.root_turn().ok_or("turn")?.steps[0].attempts[0]
            .receipt
            .as_ref()
            .ok_or("receipt")?;
        assert!(receipt.report.result.is_none());
        assert!(receipt.cost_micro_usd.is_none());
        assert_eq!(settlements.load(Ordering::SeqCst), 1);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert!(harness.sent.lock().await.is_empty());
        assert!(
            !harness
                .committed_kinds()
                .await?
                .iter()
                .any(|kind| kind == "tool.verification.intent")
        );
    }
    Ok(())
}

struct NotifiedFault {
    inner: Arc<super::reconnect::FaultPort>,
    failed: Semaphore,
}

#[tokio::test]
async fn active_budget_rejects_restore_without_cleanup_capacity() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first")]);
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(Arc::new(MockExecutor::new(vec![output(vec![call(
                    "read",
                )])])));
        })
        .build()?;
    let limits = Limits {
        active_seconds: 1,
        checkpoint_bytes: 2 * 1024 * 1024,
        ..Limits::default()
    };
    let session = bind_app_with_limits(Arc::new(app), harness.clone(), limits.clone()).await?;
    session.start("input", 1, input()).await?;
    session.drive().await?;
    let command = harness.sent.lock().await[0].clone();
    session
        .tool_status("running", observed(&command, ToolStatus::Running))
        .await?;
    limited(&session).await?;
    drive(&session).await?;
    let mut actual = result(&command);
    actual.output = "x".repeat(8192);
    session.tool_result("result", actual).await?;
    session
        .signals("checkpoint", signal_update(&session, Vec::new()).await)
        .await?;
    session.disconnect().await;

    // Simulate a nearly full, internally consistent durable snapshot. The
    // restore and interruption records fit, but pairing the required tool
    // result into history exceeds the negotiated checkpoint bound. Reject
    // before changing the durable head or granting execution authority.
    let mut store = harness.store.lock().await.clone();
    store.limits = limits;
    let batch = store.batches.last_mut().ok_or("checkpoint")?;
    let mut payload = batch.decode(&store.limits)?;
    payload.checkpoint.state["run"]["terminal_reason"] = json!("");
    let retained = serde_json::to_vec(&payload)?.len();
    let padding = (store.limits.checkpoint_bytes as usize)
        .checked_sub(retained + 2048)
        .ok_or("fixture too large")?;
    payload.checkpoint.state["run"]["terminal_reason"] = json!("x".repeat(padding));
    *batch = CheckpointBatch::encode(&payload, &store.limits)?;
    store.head.payload_sha256 = Some(batch.payload_sha256.clone());
    store
        .acknowledgements
        .get_mut(&batch.identity.batch_id)
        .ok_or("ack")?
        .payload_sha256 = batch.payload_sha256.clone();
    let replacement = super::recovery::harness_at(store).await;
    let request = super::recovery::request(&*replacement.store.lock().await, false)?;
    let before = replacement.store.lock().await.batches.len();
    let head = replacement.store.lock().await.head.clone();
    let error = super::recovery::restore(request, replacement.clone(), Vec::new())
        .await
        .err()
        .ok_or("restore without cleanup capacity was accepted")?;
    let error = error.downcast_ref::<CoreError>().ok_or("core error")?;
    assert_eq!(error.code, ErrorCode::LimitExceeded);
    assert_eq!(error.commit_status, CommitStatus::NotCommitted);
    let store = replacement.store.lock().await;
    assert_eq!(store.head, head);
    let state: SessionSnapshot = serde_json::from_value(
        store
            .batches
            .last()
            .ok_or("checkpoint")?
            .decode(&store.limits)?
            .checkpoint
            .state,
    )?;
    assert_eq!(
        state.run.as_ref().map(|run| run.status),
        Some(RunStatus::Cancelling)
    );
    assert!(
        state.root_turn().ok_or("turn")?.invocations[0]
            .result
            .is_some()
    );
    assert!(!state.root_turn().ok_or("turn")?.invocations[0].consumed);
    assert_eq!(store.batches.len(), before);
    assert!(replacement.sent.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn active_budget_waits_for_a_detached_provider_then_reconciles_its_settlement() -> TestResult
{
    let harness = Arc::new(Harness::new(None, Some("model.attempt.outcome")));
    let executor = Arc::new(HeldExecutor {
        seen: Semaphore::new(0),
        resume: Semaphore::new(0),
        calls: AtomicUsize::new(0),
        mock: MockExecutor::new(vec![output(vec![call("late-read")])]),
    });
    let settlements = Arc::new(AtomicUsize::new(0));
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first")]);
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone())
                .settlement_recorder(Recorder(settlements.clone()));
        })
        .build()?;
    let session = bind_app(Arc::new(app), harness.clone()).await?;
    session.start("input", 1, task()).await?;
    let original = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    reached(&executor.seen).await?;
    original.abort();
    assert!(original.await.is_err());
    reached(&harness.seen).await?;
    limited(&session).await?;
    // The executor stopped, but the detached SDK still owns its outcome and
    // settlement. Do not declare it abandoned before acknowledgement.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        session.snapshot().await.run.as_ref().map(|run| run.status),
        Some(RunStatus::Cancelling)
    );
    harness.resume.add_permits(1);
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if session
                .snapshot()
                .await
                .run
                .as_ref()
                .is_some_and(|run| run.status == RunStatus::RecoveryRequired)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    super::reconnect::reconnect(&session, &harness).await?;
    let done = drive(&session).await?;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Failed)
    );
    assert!(
        done.root_turn().ok_or("turn")?.steps[0].attempts[0]
            .receipt
            .is_some()
    );
    assert_eq!(settlements.load(Ordering::SeqCst), 1);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert!(harness.sent.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn active_budget_restored_exhaustion_blocks_models_verification_and_success() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![text("answer")])], harness.clone(), false).await?;
    let mut task = input();
    task.verification = Some(Verification {
        tool: "read".into(),
        arguments: json!({"path":"file.txt"}),
    });
    session.start("input", 1, task).await?;
    session.drive().await?;
    let command = harness.sent.lock().await[0].clone();
    session.tool_result("verified", result(&command)).await?;
    session.drive().await?;
    session.disconnect().await;
    let complete = harness.store.lock().await.clone();
    for kind in ["input.accepted", "model.output.applied", "agent.completed"] {
        let mut store = super::recovery::prefix(&complete, kind)?;
        let batch = store.batches.last_mut().ok_or("checkpoint")?;
        let mut payload = batch.decode(&store.limits)?;
        let run = &mut payload.checkpoint.state["run"];
        run["active_ms"] = json!(run["limits"]["active_seconds"].as_u64().ok_or("limit")? * 1000);
        *batch = CheckpointBatch::encode(&payload, &store.limits)?;
        store.head.payload_sha256 = Some(batch.payload_sha256.clone());
        store
            .acknowledgements
            .get_mut(&batch.identity.batch_id)
            .ok_or("ack")?
            .payload_sha256 = batch.payload_sha256.clone();
        let replacement = super::recovery::harness_at(store).await;
        let request = super::recovery::request(&*replacement.store.lock().await, false)?;
        let (restored, executor) =
            super::recovery::restore(request, replacement.clone(), Vec::new()).await?;
        let done = drive(&restored).await?;
        assert_eq!(
            done.run.as_ref().map(|run| run.status),
            Some(RunStatus::Failed),
            "{kind}"
        );
        assert!(
            done.run
                .as_ref()
                .is_some_and(|run| run.resource_error.is_some())
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0, "{kind}");
        assert!(replacement.sent.lock().await.is_empty(), "{kind}");
    }
    Ok(())
}

#[tokio::test]
async fn active_budget_restore_rejects_inconsistent_failure_and_cleanup_facts() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
    session.start("input", 1, task()).await?;
    session.drive().await?;
    let command = harness.sent.lock().await[0].clone();
    session
        .tool_status("running", observed(&command, ToolStatus::Running))
        .await?;
    limited(&session).await?;
    drive(&session).await?;
    session.disconnect().await;
    for case in [
        "code",
        "commit",
        "counter",
        "completed",
        "cancelled",
        "cleanup",
    ] {
        let replacement = super::recovery::harness_at(harness.store.lock().await.clone()).await;
        let mut request = super::recovery::request(&*replacement.store.lock().await, false)?;
        let mut payload = request
            .binding
            .checkpoint
            .as_ref()
            .ok_or("checkpoint")?
            .decode(&Limits::default())?;
        let state = &mut payload.checkpoint.state;
        let root = state["agent_id"].as_str().ok_or("root")?.to_owned();
        match case {
            "code" => state["run"]["resource_error"]["code"] = json!("busy"),
            "commit" => state["run"]["resource_error"]["commit_status"] = json!("not_committed"),
            "counter" => state["run"]["active_ms"] = json!(0),
            "completed" => state["run"]["status"] = json!("completed"),
            "cancelled" => state["run"]["status"] = json!("cancelled"),
            _ => state["agents"][&root]["turn"]["cancellation_requested"] = json!(false),
        }
        let batch = CheckpointBatch::encode(&payload, &Limits::default())?;
        request.binding.durable_head.payload_sha256 = Some(batch.payload_sha256.clone());
        request.binding.checkpoint = Some(batch);
        let before = replacement.store.lock().await.head.clone();
        let error = super::recovery::restore(request, replacement.clone(), Vec::new())
            .await
            .err()
            .ok_or("inconsistent failure restored")?;
        assert_eq!(
            error.downcast_ref::<CoreError>().map(|error| error.code),
            Some(ErrorCode::CheckpointConflict),
            "{case}"
        );
        assert_eq!(replacement.store.lock().await.head, before);
    }
    Ok(())
}
#[async_trait]
impl HarnessPort for NotifiedFault {
    async fn read_artifact(
        &self,
        reference: &ArtifactRef,
        offset: u64,
        max_bytes: u64,
    ) -> Result<Vec<u8>, CoreError> {
        self.inner.read_artifact(reference, offset, max_bytes).await
    }

    async fn commit(&self, batch: CheckpointBatch) -> Result<CheckpointAck, CoreError> {
        let result = self.inner.commit(batch).await;
        if result.is_err() {
            self.failed.add_permits(1);
        }
        result
    }
    async fn send(&self, message: ServerMessage) -> Result<(), CoreError> {
        self.inner.send(message).await
    }
}

#[tokio::test]
async fn active_budget_lost_ack_reconciles_one_failure_without_replaying_tools() -> TestResult {
    for committed in [false, true] {
        let harness = Arc::new(Harness::new(None, None));
        let port = Arc::new(NotifiedFault {
            inner: Arc::new(super::reconnect::FaultPort::new(
                harness.clone(),
                "run.limit_reached",
                committed,
            )),
            failed: Semaphore::new(0),
        });
        let (session, executor, _) =
            setup(vec![output(vec![call("read")])], port.clone(), false).await?;
        session.start("input", 1, task()).await?;
        session.drive().await?;
        let command = harness.sent.lock().await[0].clone();
        session
            .tool_status("running", observed(&command, ToolStatus::Running))
            .await?;
        reached(&port.failed).await?;
        super::reconnect::reconnect(&session, &harness).await?;
        assert!(
            session
                .snapshot()
                .await
                .run
                .as_ref()
                .is_some_and(|run| run.resource_error.is_some() && run.active_ms >= 1000)
        );
        drive(&session).await?;
        session.tool_result("actual", result(&command)).await?;
        assert_eq!(
            drive(&session).await?.run.as_ref().map(|run| run.status),
            Some(RunStatus::Failed)
        );
        assert_eq!(
            harness
                .committed_kinds()
                .await?
                .iter()
                .filter(|kind| *kind == "run.limit_reached")
                .count(),
            1
        );
        assert_eq!(harness.sent.lock().await.len(), 1);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    }
    Ok(())
}

#[tokio::test]
async fn active_budget_unknown_effect_restoration_preserves_cleanup_and_never_redispatches()
-> TestResult {
    let mut fixture = Harness::new(None, None);
    fixture.wait_for_approval = true;
    let harness = Arc::new(fixture);
    let (session, _, _) = setup(
        vec![output(vec![call("one"), call("two")])],
        harness.clone(),
        false,
    )
    .await?;
    session.start("input", 1, task()).await?;
    session.drive().await?;
    let commands = harness.sent.lock().await.clone();
    assert!(
        harness
            .store
            .lock()
            .await
            .try_start_tool(fence(&commands[0]))
    );
    session
        .tool_status("running", observed(&commands[0], ToolStatus::Running))
        .await?;
    session
        .tool_status(
            "approval",
            observed(&commands[1], ToolStatus::WaitingApproval),
        )
        .await?;
    limited(&session).await?;
    drive(&session).await?;
    let mut unknown = result(&commands[0]);
    unknown.status = ToolOutcome::EffectUnknown;
    session.tool_result("unknown", unknown).await?;
    assert_eq!(
        session.snapshot().await.run.as_ref().map(|run| run.status),
        Some(RunStatus::RecoveryRequired)
    );
    session.disconnect().await;
    let replacement = super::recovery::harness_at(harness.store.lock().await.clone()).await;
    let mut request = super::recovery::request(&*replacement.store.lock().await, false)?;
    request.tools = vec![
        observed(&commands[0], ToolStatus::Stopped),
        observed(&commands[1], ToolStatus::NotStarted),
    ];
    request.results.push(result(&commands[0]));
    let (restored, executor) =
        super::recovery::restore(request, replacement.clone(), Vec::new()).await?;
    assert_eq!(
        restored
            .snapshot()
            .await
            .root_turn()
            .map(|turn| turn.status),
        Some(AgentStatus::Cancelling)
    );
    let done = drive(&restored).await?;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Failed)
    );
    assert!(
        done.run
            .as_ref()
            .is_some_and(|run| run.resource_error.is_some())
    );
    assert!(
        done.root_turn()
            .ok_or("turn")?
            .invocations
            .iter()
            .all(|call| call.consumed)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert!(replacement.sent.lock().await.is_empty());
    Ok(())
}
