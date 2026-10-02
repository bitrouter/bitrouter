use super::*;
use bitrouter_orchestrator::core::accounting::work::CostWorkState;
use bitrouter_orchestrator::core::protocol::ProviderAttemptEvidence;
use bitrouter_orchestrator::core::session::AgentStatus;

pub(super) struct FaultPort {
    harness: Arc<Harness>,
    kind: &'static str,
    committed: bool,
    enabled: AtomicBool,
    proposals: Mutex<Vec<CheckpointBatch>>,
}

impl FaultPort {
    pub(super) fn new(harness: Arc<Harness>, kind: &'static str, committed: bool) -> Self {
        Self {
            harness,
            kind,
            committed,
            enabled: AtomicBool::new(true),
            proposals: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl HarnessPort for FaultPort {
    async fn observe_restoration(
        &self,
        observer: bitrouter_orchestrator::core::session::restoration_activity::RestorationActivity,
    ) -> Result<(), CoreError> {
        self.harness.observe_restoration(observer).await
    }
    async fn synchronize_restoration(
        &self,
        observer: bitrouter_orchestrator::core::session::restoration_activity::RestorationActivity,
    ) -> Result<(), CoreError> {
        self.harness.synchronize_restoration(observer).await
    }
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
        let matches = batch
            .decode(&Limits::default())?
            .events
            .iter()
            .any(|event| event.kind == self.kind);
        self.proposals.lock().await.push(batch.clone());
        if matches && self.enabled.swap(false, Ordering::SeqCst) {
            if self.committed {
                self.harness.commit(batch).await?;
            }
            return Err(CoreError::rejected(
                ErrorCode::CheckpointUnavailable,
                "connection interrupted",
            ));
        }
        self.harness.commit(batch).await
    }
    async fn send(&self, message: ServerMessage) -> Result<(), CoreError> {
        self.harness.send(message).await
    }
}

pub(super) async fn reconnect(
    session: &CoreSession,
    harness: &Harness,
) -> Result<DurableHead, Box<dyn std::error::Error>> {
    let (grant, head) = {
        let store = harness.store.lock().await;
        (store.grant.clone(), store.head.clone())
    };
    Ok(tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match session.reconnect(&grant, &head).await {
                Err(error) if error.code == ErrorCode::Busy => tokio::task::yield_now().await,
                result => break result,
            }
        }
    })
    .await??)
}

#[tokio::test]
async fn reconnect_adopts_or_retransmits_the_exact_input_batch() -> TestResult {
    for committed in [false, true] {
        let harness = Arc::new(Harness::new(None, None));
        let port = Arc::new(FaultPort::new(harness.clone(), "input.accepted", committed));
        let (session, executor, _) =
            setup(vec![output(vec![text("done")])], port.clone(), false).await?;
        let revision = session.head().await.state_revision;
        let failure = session
            .start("input", revision, input())
            .await
            .err()
            .ok_or("expected missing ACK")?;
        assert_eq!(failure.commit_status, CommitStatus::Unknown);
        let original = port
            .proposals
            .lock()
            .await
            .last()
            .ok_or("missing proposal")?
            .clone();
        let payload = original.decode(&Limits::default())?;
        let proposed: SessionSnapshot = serde_json::from_value(payload.checkpoint.state)?;
        let expected = proposed
            .operations
            .get("input")
            .ok_or("input receipt missing")?;
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        let head = reconnect(&session, &harness).await?;
        assert_eq!(head.execution_epoch, 1);
        assert_eq!(session.start("input", revision, input()).await?, *expected);
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
        assert_eq!(
            session.drive().await?.run.as_ref().map(|run| run.status),
            Some(RunStatus::Completed)
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    }
    Ok(())
}

#[tokio::test]
async fn reconnect_applies_acknowledged_output_without_reexecuting_provider_or_tools() -> TestResult
{
    let harness = Arc::new(Harness::new(None, None));
    let port = Arc::new(FaultPort::new(
        harness.clone(),
        "model.attempt.outcome",
        true,
    ));
    let (session, executor, settlements) = setup(
        vec![output(vec![call("read")]), output(vec![text("done")])],
        port,
        false,
    )
    .await?;
    session
        .start("input", session.head().await.state_revision, input())
        .await?;
    assert!(session.drive().await.is_err());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(settlements.load(Ordering::SeqCst), 1);
    assert!(harness.sent.lock().await.is_empty());
    assert!(session.pending_provider_evidence().await.reports.is_empty());
    reconnect(&session, &harness).await?;
    session.drive().await?;
    let command = harness
        .sent
        .lock()
        .await
        .first()
        .ok_or("missing command")?
        .clone();
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    session.disconnect().await;
    reconnect(&session, &harness).await?;
    session.drive().await?;
    assert_eq!(harness.sent.lock().await.len(), 1);
    session.tool_result("result", result(&command)).await?;
    assert_eq!(
        session.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    assert_eq!(settlements.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn reconnect_rejects_different_grants_and_divergent_durable_heads() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(Vec::new(), harness.clone(), false).await?;
    for field in ["epoch", "owner", "harness", "session"] {
        let mut changed = grant();
        match field {
            "epoch" => changed.execution_epoch += 1,
            "owner" => changed.core_instance_id = "other".into(),
            "harness" => changed.harness_id = "other".into(),
            _ => changed.session_id = "other".into(),
        }
        assert_eq!(
            session
                .reconnect(&changed, &session.head().await)
                .await
                .err()
                .ok_or("scope accepted")?
                .code,
            ErrorCode::UnauthorizedScope
        );
    }
    let mut head = session.head().await;
    head.payload_sha256 = Some("0".repeat(64));
    assert_eq!(
        session
            .reconnect(&grant(), &head)
            .await
            .err()
            .ok_or("divergent head accepted")?
            .code,
        ErrorCode::CheckpointConflict
    );
    assert!(
        session
            .start("blocked", session.head().await.state_revision, input())
            .await
            .is_err()
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    reconnect(&session, &harness).await?;
    session
        .start("input", session.head().await.state_revision, input())
        .await?;
    Ok(())
}

struct DisconnectingExecutor {
    session: Mutex<Option<CoreSession>>,
    calls: AtomicUsize,
    size: usize,
}
#[async_trait]
impl Executor for DisconnectingExecutor {
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> bitrouter_sdk::Result<ExecutionResult> {
        let count = self.calls.fetch_add(1, Ordering::SeqCst);
        let result = MockExecutor::new(vec![if count == 0 {
            if self.size > 0 {
                output(vec![text(&"x".repeat(self.size))])
            } else {
                output(vec![call("late-read")])
            }
        } else {
            output(vec![text("replacement completed")])
        }])
        .execute(target, prompt, ctx)
        .await?;
        if count == 0
            && let Some(session) = self.session.lock().await.as_ref()
        {
            session.disconnect().await;
        }
        Ok(result)
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

async fn disconnecting_session(
    size: usize,
    limits: Limits,
) -> Result<
    (
        CoreSession,
        Arc<Harness>,
        Arc<DisconnectingExecutor>,
        Arc<AtomicUsize>,
    ),
    Box<dyn std::error::Error>,
> {
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first")]);
    let executor = Arc::new(DisconnectingExecutor {
        session: Mutex::new(None),
        calls: AtomicUsize::new(0),
        size,
    });
    let settlements = Arc::new(AtomicUsize::new(0));
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone())
                .settlement_recorder(Recorder(settlements.clone()));
        })
        .build()?;
    let harness = Arc::new(Harness::new(None, None));
    let session = bind_app_with_limits(Arc::new(app), harness.clone(), limits).await?;
    *executor.session.lock().await = Some(session.clone());
    session
        .start("input", session.head().await.state_revision, input())
        .await?;
    Ok((session, harness, executor, settlements))
}

#[tokio::test]
async fn reconnect_imports_bounded_late_output_as_evidence_without_applying_calls() -> TestResult {
    let (session, harness, executor, settlements) =
        disconnecting_session(0, Limits::default()).await?;
    assert!(session.drive().await.is_err());
    let pending = session.pending_provider_evidence().await;
    assert!(!pending.overflowed);
    assert_eq!(pending.reports.len(), 1);
    assert!(pending.reports[0].report.result.is_some());
    reconnect(&session, &harness).await?;
    let state = session.snapshot().await;
    assert_eq!(
        state.provider_evidence.get(&pending.reports[0].attempt_id),
        Some(&pending.reports[0])
    );
    assert!(state.root_turn().ok_or("turn missing")?.steps[0].interrupted);
    assert!(
        state
            .root_turn()
            .ok_or("turn missing")?
            .invocations
            .is_empty()
    );
    assert!(session.pending_provider_evidence().await.reports.is_empty());
    assert_eq!(
        session.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    assert_eq!(settlements.load(Ordering::SeqCst), 2);
    assert!(harness.sent.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn reconnect_fails_closed_when_unacknowledged_output_exceeds_bound() -> TestResult {
    let limits = Limits {
        checkpoint_bytes: 256 * 1024,
        unacknowledged_bytes: 512 * 1024,
        ..Limits::default()
    };
    let (session, harness, executor, settlements) =
        disconnecting_session(1024 * 1024, limits).await?;
    assert!(session.drive().await.is_err());
    let pending = session.pending_provider_evidence().await;
    assert!(pending.overflowed);
    assert!(pending.reports.is_empty());
    let head = harness.store.lock().await.head.clone();
    assert_eq!(
        session
            .reconnect(&grant(), &head)
            .await
            .err()
            .ok_or("overflow resumed")?
            .code,
        ErrorCode::RecoveryRequired
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(settlements.load(Ordering::SeqCst), 1);
    Ok(())
}

fn evidence_from(
    store: &DurableHarness,
) -> Result<ProviderAttemptEvidence, Box<dyn std::error::Error>> {
    let store = recovery::prefix(store, "model.attempt.outcome")?;
    let state: SessionSnapshot = serde_json::from_value(
        store
            .batches
            .last()
            .ok_or("missing batch")?
            .decode(&store.limits)?
            .checkpoint
            .state,
    )?;
    let attempt = &state.root_turn().ok_or("missing turn")?.steps[0].attempts[0];
    Ok(ProviderAttemptEvidence {
        run_id: state.run.as_ref().ok_or("missing run")?.run_id.clone(),
        attempt_id: attempt.attempt_id.clone(),
        report: attempt
            .receipt
            .as_ref()
            .ok_or("missing report")?
            .report
            .clone(),
        active_ms: None,
    })
}

#[tokio::test]
async fn late_provider_evidence_remains_owned_after_the_original_run_is_replaced() -> TestResult {
    let store = recovery::completed_store().await?;
    let evidence = evidence_from(&store)?;
    let harness = recovery::harness_at(recovery::prefix(&store, "model.attempt.intent")?).await;
    let request = recovery::request(&*harness.store.lock().await, false)?;
    let (session, executor) = recovery::restore(
        request,
        harness.clone(),
        vec![
            output(vec![text("retry")]),
            output(vec![text("second run")]),
        ],
    )
    .await?;
    session.drive().await?;
    session
        .start("second", session.head().await.state_revision, input())
        .await?;
    let receipt = session.provider_evidence("late", evidence.clone()).await?;
    assert_eq!(
        session.provider_evidence("late", evidence.clone()).await?,
        receipt
    );
    let state = session.snapshot().await;
    let old = state
        .cost_work
        .get(&evidence.run_id)
        .and_then(|ledger| ledger.work.get(&evidence.attempt_id))
        .ok_or("old work lost")?;
    assert_eq!(old.state, CostWorkState::OutcomeRecorded);
    assert_ne!(
        state.run.as_ref().map(|run| &run.run_id),
        Some(&evidence.run_id)
    );
    assert_eq!(state.run.as_ref().map(|run| run.model_attempts), Some(0));
    for altered in ["request", "route", "attempt", "run", "output"] {
        let mut changed = evidence.clone();
        match altered {
            "request" => changed.report.request_id = "foreign".into(),
            "route" => changed.report.route.model = "foreign".into(),
            "attempt" => changed.attempt_id = "foreign".into(),
            "run" => changed.run_id = "foreign".into(),
            _ => changed.report.result = None,
        }
        assert!(
            session
                .provider_evidence(&format!("conflict-{altered}"), changed)
                .await
                .is_err()
        );
    }
    assert_eq!(
        session.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    assert!(harness.sent.lock().await.is_empty());
    Ok(())
}

struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
struct PendingExecutor {
    seen: Semaphore,
    dropped: Arc<AtomicBool>,
}
#[async_trait]
impl Executor for PendingExecutor {
    async fn execute(
        &self,
        _: &RoutingTarget,
        _: &Prompt,
        _: &PipelineContext,
    ) -> bitrouter_sdk::Result<ExecutionResult> {
        let _drop = DropFlag(self.dropped.clone());
        self.seen.add_permits(1);
        std::future::pending().await
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
#[derive(Clone)]
struct HeldEnd {
    seen: Arc<Semaphore>,
    release: Arc<Semaphore>,
}
#[async_trait]
impl ObserveHook for HeldEnd {
    async fn after_phase(&self, _: Phase, _: &PipelineContext) {}
    async fn on_stream_part(&self, _: &StreamContext, _: &StreamPart) {}
    async fn on_request_end(&self, _: &PipelineContext, _: &RequestOutcome) {
        self.seen.add_permits(1);
        if let Ok(permit) = self.release.acquire().await {
            permit.forget();
        }
    }
}

#[tokio::test]
async fn disconnect_cancels_provider_io_but_reconnect_waits_for_detached_sdk_settlement()
-> TestResult {
    for abandoned in [false, true] {
        cancelled_provider(abandoned).await?;
    }
    Ok(())
}

async fn cancelled_provider(abandoned: bool) -> TestResult {
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first")]);
    let executor = Arc::new(PendingExecutor {
        seen: Semaphore::new(0),
        dropped: Arc::new(AtomicBool::new(false)),
    });
    let hook = HeldEnd {
        seen: Arc::new(Semaphore::new(0)),
        release: Arc::new(Semaphore::new(0)),
    };
    let settlements = Arc::new(AtomicUsize::new(0));
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone())
                .observe_hook(hook.clone())
                .settlement_recorder(Recorder(settlements.clone()));
        })
        .build()?;
    let harness = Arc::new(Harness::new(None, None));
    let session = bind_app(Arc::new(app), harness.clone()).await?;
    session
        .start("input", session.head().await.state_revision, input())
        .await?;
    let driver = session.clone();
    let mut running = Some(tokio::spawn(async move { driver.drive().await }));
    tokio::time::timeout(Duration::from_secs(5), executor.seen.acquire())
        .await??
        .forget();
    if abandoned {
        let running = running.take().ok_or("missing driver")?;
        running.abort();
        assert!(running.await.is_err());
        assert_eq!(
            session
                .drive()
                .await
                .err()
                .ok_or("abandoned model resumed")?
                .code,
            ErrorCode::RecoveryRequired
        );
    }
    session.disconnect().await;
    tokio::time::timeout(Duration::from_secs(5), hook.seen.acquire())
        .await??
        .forget();
    assert!(executor.dropped.load(Ordering::SeqCst));
    assert_eq!(settlements.load(Ordering::SeqCst), 1);
    let head = harness.store.lock().await.head.clone();
    if let Some(running) = running {
        assert_eq!(
            session
                .reconnect(&grant(), &head)
                .await
                .err()
                .ok_or("running driver resumed")?
                .code,
            ErrorCode::Busy
        );
        running.abort();
        assert!(running.await.is_err());
    }
    assert_eq!(
        session
            .reconnect(&grant(), &head)
            .await
            .err()
            .ok_or("detached SDK resumed")?
            .code,
        ErrorCode::Busy
    );
    let evidence = session.pending_provider_evidence().await;
    assert_eq!(evidence.reports.len(), 1);
    assert!(evidence.reports[0].report.result.is_none());
    hook.release.add_permits(1);
    reconnect(&session, &harness).await?;
    let state = session.snapshot().await;
    assert!(state.root_turn().ok_or("turn missing")?.steps[0].interrupted);
    assert_eq!(
        state.root_turn().ok_or("turn missing")?.status,
        AgentStatus::Runnable
    );
    assert_eq!(state.provider_evidence.len(), 1);
    assert_eq!(state.run.as_ref().map(|run| run.model_attempts), Some(1));
    assert!(harness.sent.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn reconnect_retries_an_undelivered_material_request_with_the_same_identity() -> TestResult {
    let mut harness = Harness::new(None, None);
    harness.hold_material_send = true;
    let harness = Arc::new(harness);
    let (session, executor, _) =
        setup(vec![output(vec![text("done")])], harness.clone(), false).await?;
    session
        .signals(
            "inventory",
            signal_update(&session, vec![material("v1", "document", false)]).await,
        )
        .await?;
    session
        .start("input", session.head().await.state_revision, input())
        .await?;
    let driving = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    let request_id = session
        .snapshot()
        .await
        .signals
        .requests
        .keys()
        .next()
        .ok_or("missing request")?
        .clone();
    session.disconnect().await;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), driving)
            .await??
            .is_err()
    );
    reconnect(&session, &harness).await?;
    harness.resume.add_permits(1);
    session.drive().await?;
    assert_eq!(harness.material_requests.lock().await[0].0, request_id);
    session
        .material_result(
            "material",
            &request_id,
            Some(material("v1", "document", true)),
            None,
        )
        .await?;
    assert_eq!(
        session.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn reconnect_requires_tool_reconciliation_when_delivery_is_uncertain() -> TestResult {
    for delivered in [false, true] {
        let mut harness = Harness::new(None, None);
        harness.hold_send = !delivered;
        harness.hold_after_send = delivered;
        let harness = Arc::new(harness);
        let (session, executor, _) =
            setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
        session
            .start("input", session.head().await.state_revision, input())
            .await?;
        let driving = tokio::spawn({
            let session = session.clone();
            async move { session.drive().await }
        });
        let signal = if delivered {
            &harness.delivered
        } else {
            &harness.seen
        };
        tokio::time::timeout(Duration::from_secs(5), signal.acquire())
            .await??
            .forget();
        session.disconnect().await;
        assert!(
            tokio::time::timeout(Duration::from_secs(5), driving)
                .await??
                .is_err()
        );
        let head = harness.store.lock().await.head.clone();
        assert_eq!(
            session
                .reconnect(&grant(), &head)
                .await
                .err()
                .ok_or("uncertain tool resumed")?
                .code,
            ErrorCode::RecoveryRequired
        );
        assert!(session.drive().await.is_err());
        assert_eq!(harness.sent.lock().await.len(), usize::from(delivered));
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    }
    Ok(())
}

struct CancelPort {
    harness: Arc<Harness>,
    hold: AtomicBool,
    seen: Semaphore,
    dropped: Arc<AtomicBool>,
}

#[async_trait]
impl HarnessPort for CancelPort {
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
        self.harness.commit(batch).await
    }
    async fn send(&self, message: ServerMessage) -> Result<(), CoreError> {
        if matches!(&message, ServerMessage::ToolCancel { .. })
            && self.hold.swap(false, Ordering::SeqCst)
        {
            let _drop = DropFlag(self.dropped.clone());
            self.seen.add_permits(1);
            return std::future::pending().await;
        }
        self.harness.send(message).await
    }
}

#[tokio::test]
async fn reconnect_cancels_old_delivery_and_retries_tool_cancellation() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let port = Arc::new(CancelPort {
        harness: harness.clone(),
        hold: AtomicBool::new(true),
        seen: Semaphore::new(0),
        dropped: Arc::new(AtomicBool::new(false)),
    });
    let (session, executor, _) =
        setup(vec![output(vec![call("read")])], port.clone(), false).await?;
    session
        .start("input", session.head().await.state_revision, input())
        .await?;
    session.drive().await?;
    let command = harness.sent.lock().await[0].clone();
    let run_id = session.snapshot().await.run.ok_or("missing run")?.run_id;
    session
        .cancel_run("cancel", session.head().await.state_revision, &run_id)
        .await?;
    session.drive().await?;
    tokio::time::timeout(Duration::from_secs(5), port.seen.acquire())
        .await??
        .forget();
    session.disconnect().await;
    reconnect(&session, &harness).await?;
    session.drive().await?;
    tokio::time::timeout(Duration::from_secs(5), harness.cancel_seen.acquire())
        .await??
        .forget();
    assert!(port.dropped.load(Ordering::SeqCst));
    assert_eq!(
        harness.cancelled.lock().await[0],
        (
            command.invocation_id.clone(),
            command.attempt_id.clone(),
            command.execution_epoch
        )
    );
    session.tool_result("result", result(&command)).await?;
    assert_eq!(
        session.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Cancelled)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(harness.sent.lock().await.len(), 1);
    Ok(())
}
