use super::*;
use bitrouter_orchestrator::core::accounting::work::{CostWorkKind, CostWorkState};
use bitrouter_orchestrator::core::protocol::ModelMode;
use bitrouter_sdk::extension::request_check::{Decision, Input};
use bitrouter_sdk::language_model::hooks::{HookDecision, PreRequestHook, RouteHook};
use bitrouter_sdk::language_model::native_accounting::{NativeCostObservation, NativeCostSource};
use bitrouter_sdk::language_model::native_preparation::NativePreparationWorkKind as Kind;
use bitrouter_sdk::language_model::request_checks::{
    CheckerFailure, CheckerResult, RequestCheckBinding, RequestCheckerRunner,
};
use bitrouter_sdk::language_model::routing::{
    ModelInfo, ModelResolution, ModelSelector, RouterRequestIdentity, RoutingPrefs, RoutingTable,
};

struct Probe {
    calls: Mutex<Vec<Kind>>,
    transforms: AtomicUsize,
    hold: Option<Kind>,
    held: AtomicBool,
    entered: Semaphore,
    resume: Semaphore,
    deny: bool,
    cost_reads: Mutex<Vec<String>>,
}

impl Probe {
    fn new(hold: Option<Kind>, deny: bool) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            transforms: AtomicUsize::new(0),
            hold,
            held: AtomicBool::new(false),
            entered: Semaphore::new(0),
            resume: Semaphore::new(0),
            deny,
            cost_reads: Mutex::new(Vec::new()),
        }
    }

    async fn called(&self, kind: Kind) -> bitrouter_sdk::Result<()> {
        self.calls.lock().await.push(kind);
        if self.hold == Some(kind) && !self.held.swap(true, Ordering::SeqCst) {
            self.entered.add_permits(1);
            self.resume
                .acquire()
                .await
                .map_err(|_| bitrouter_sdk::BitrouterError::internal("fixture stopped"))?
                .forget();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        Ok(())
    }

    async fn count(&self, kind: Kind) -> usize {
        if kind == Kind::PromptTransform {
            return self.transforms.load(Ordering::SeqCst);
        }
        self.calls
            .lock()
            .await
            .iter()
            .filter(|seen| **seen == kind)
            .count()
    }
}

impl bitrouter_sdk::app::PromptTransform for Probe {
    fn apply(&self, _: &mut Prompt) {
        self.transforms.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl RoutingTable for Probe {
    async fn resolve_model(&self, model: &str) -> bitrouter_sdk::Result<ModelResolution> {
        self.called(Kind::RouterLookup).await?;
        let mut resolution = ModelResolution::passthrough(model);
        resolution.router = Some(RouterRequestIdentity {
            router_id: "checked".into(),
            original_selector: model.into(),
            binding_digest: "router-v1".into(),
        });
        resolution.policy = Some("fixture-policy".into());
        resolution.request_checks = ["first", "second"]
            .into_iter()
            .map(|id| RequestCheckBinding {
                checker_id: id.into(),
                binding_digest: format!("checker-{id}-v1"),
                max_input_bytes: 8192,
                timeout_ms: 5000,
            })
            .collect();
        Ok(resolution)
    }
    async fn route_chain(
        &self,
        _: &str,
        _: &RoutingPrefs,
        _: &CallerContext,
    ) -> bitrouter_sdk::Result<Vec<RoutingTarget>> {
        self.called(Kind::RouterLookup).await?;
        Ok(vec![target("first")])
    }
    fn list_models(&self) -> Vec<ModelInfo> {
        Vec::new()
    }
    fn model_info(&self, _: &str) -> Option<ModelInfo> {
        None
    }
    async fn reload(&self) -> bitrouter_sdk::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl ModelSelector for Probe {
    async fn select_variant(
        &self,
        _: &str,
        _: Option<&str>,
        _: &mut PipelineContext,
    ) -> bitrouter_sdk::Result<()> {
        self.called(Kind::ModelSelection).await
    }
}

#[async_trait]
impl RequestCheckerRunner for Probe {
    async fn check(
        &self,
        _: RequestCheckBinding,
        _: Input,
    ) -> Result<CheckerResult, CheckerFailure> {
        self.called(Kind::RequestCheck)
            .await
            .map_err(|_| CheckerFailure {
                kind: bitrouter_sdk::language_model::request_checks::CheckerFailureKind::Internal,
                detail: None,
            })?;
        Ok(CheckerResult {
            decision: if self.deny {
                Decision::Deny {
                    reason_code: "fixture_denied".into(),
                }
            } else {
                Decision::Allow
            },
            revision: "fixture-v1".into(),
        })
    }
}

#[async_trait]
impl NativeCostSource for Probe {
    async fn read(
        &self,
        _: &CallerContext,
        ids: &[String],
    ) -> bitrouter_sdk::Result<Vec<NativeCostObservation>> {
        self.cost_reads.lock().await.extend_from_slice(ids);
        Ok(ids
            .iter()
            .map(|id| NativeCostObservation {
                request_id: id.clone(),
                claims: Vec::new(),
                unknown_reason: Some("fixture_cost_unavailable".into()),
            })
            .collect())
    }
}

struct Hook(Arc<Probe>, Kind);

#[tokio::test]
async fn reconnect_retains_preparation_time_without_an_outcome_checkpoint() -> TestResult {
    let probe = Arc::new(Probe::new(Some(Kind::RequestCheck), false));
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor) = session(harness.clone(), probe.clone()).await?;
    session
        .start("input", session.head().await.state_revision, input())
        .await?;
    let driving = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(5), probe.entered.acquire())
        .await??
        .forget();
    let before = session.snapshot().await.run.ok_or("missing run")?.active_ms;
    tokio::time::sleep(Duration::from_millis(30)).await;
    session.disconnect().await;
    probe.resume.add_permits(1);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), driving)
            .await??
            .is_err()
    );
    assert!(session.pending_provider_evidence().await.reports.is_empty());
    super::reconnect::reconnect(&session, &harness).await?;
    let state = session.snapshot().await;
    let after = state.run.as_ref().ok_or("missing run")?.active_ms;
    assert!(
        after >= before + 30,
        "completed callback time must survive reconnect"
    );
    let step = &state.root_turn().ok_or("missing turn")?.steps[0];
    assert!(step.interrupted);
    assert!(
        step.preparation_work
            .last()
            .ok_or("missing callback")?
            .report
            .is_none()
    );
    tokio::time::sleep(Duration::from_millis(30)).await;
    super::reconnect::reconnect(&session, &harness).await?;
    assert_eq!(
        session.snapshot().await.run.ok_or("missing run")?.active_ms,
        after
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[async_trait]
impl PreRequestHook for Hook {
    async fn check(&self, _: &mut PipelineContext) -> bitrouter_sdk::Result<HookDecision> {
        self.0.called(self.1).await?;
        Ok(HookDecision::Allow)
    }
}

#[async_trait]
impl RouteHook for Hook {
    async fn resolve(
        &self,
        _: &mut Vec<RoutingTarget>,
        _: &mut PipelineContext,
    ) -> bitrouter_sdk::Result<()> {
        self.0.called(self.1).await
    }
}

async fn session(
    harness: Arc<dyn HarnessPort>,
    probe: Arc<Probe>,
) -> Result<(CoreSession, Arc<RecordingExecutor>), Box<dyn std::error::Error>> {
    let executor = Arc::new(RecordingExecutor {
        mock: MockExecutor::new(vec![output(vec![text("done")])]),
        agent_once: Mutex::new(Default::default()),
        prompts: Mutex::new(Vec::new()),
        calls: AtomicUsize::new(0),
    });
    let app = App::builder()
        .prompt_transform(probe.clone())
        .language_model(|builder| {
            builder
                .routing_table(probe.clone())
                .executor(executor.clone())
                .pre_resolution_hook(Hook(probe.clone(), Kind::PreResolutionHook))
                .router_preparation_hook(Hook(probe.clone(), Kind::RouterPreparationHook))
                .pre_request_hook(Hook(probe.clone(), Kind::PreRequestHook))
                .request_checker_runner(probe.clone())
                .model_selector(probe.clone())
                .route_hook(Hook(probe.clone(), Kind::RouteHook))
                .native_cost_source(probe);
        })
        .build()?;
    Ok((bind_app(Arc::new(app), harness).await?, executor))
}

fn task() -> TaskInput {
    let mut task = input();
    task.routing.model = ModelMode::Policy;
    task
}

struct PhaseHarness {
    inner: Arc<Harness>,
    kind: Kind,
    outcome: bool,
    lose: bool,
    once: AtomicBool,
    seen: Semaphore,
    resume: Semaphore,
    agent_id: Mutex<Option<String>>,
}

impl PhaseHarness {
    fn new(kind: Kind, outcome: bool, lose: bool) -> Self {
        Self {
            inner: Arc::new(Harness::new(None, None)),
            kind,
            outcome,
            lose,
            once: AtomicBool::new(false),
            seen: Semaphore::new(0),
            resume: Semaphore::new(0),
            agent_id: Mutex::new(None),
        }
    }
}

#[async_trait]
impl HarnessPort for PhaseHarness {
    async fn commit(&self, batch: CheckpointBatch) -> Result<CheckpointAck, CoreError> {
        let payload = batch.decode(&Limits::default())?;
        let agent_id = self.agent_id.lock().await.clone();
        let matches = payload.events.iter().any(|event| {
            let (name, kind) = if self.outcome {
                ("preparation.work.outcome", &event.payload["work"]["kind"])
            } else {
                ("preparation.work.intent", &event.payload["kind"])
            };
            event.kind == name
                && kind == &json!(self.kind)
                && agent_id
                    .as_ref()
                    .is_none_or(|agent_id| event.agent_id.as_ref() == Some(agent_id))
        });
        if matches && !self.once.swap(true, Ordering::SeqCst) {
            if self.lose {
                self.inner.commit(batch).await?;
                return Err(CoreError::rejected(
                    ErrorCode::CheckpointUnavailable,
                    "lost preparation ACK",
                ));
            }
            self.seen.add_permits(1);
            self.resume
                .acquire()
                .await
                .map_err(|_| {
                    CoreError::rejected(ErrorCode::CheckpointUnavailable, "fixture stopped")
                })?
                .forget();
        }
        self.inner.commit(batch).await
    }
    async fn send(&self, message: ServerMessage) -> Result<(), CoreError> {
        self.inner.send(message).await
    }
}

#[tokio::test]
async fn each_preparation_callback_waits_for_its_own_ack() -> TestResult {
    for kind in [
        Kind::PromptTransform,
        Kind::PreResolutionHook,
        Kind::RouterPreparationHook,
        Kind::PreRequestHook,
        Kind::RequestCheck,
        Kind::ModelSelection,
        Kind::RouterLookup,
        Kind::RouteHook,
    ] {
        let harness = Arc::new(PhaseHarness::new(kind, false, false));
        let probe = Arc::new(Probe::new(None, false));
        let (session, executor) = session(harness.clone(), probe.clone()).await?;
        session.start("input", 1, task()).await?;
        let driving = tokio::spawn({
            let session = session.clone();
            async move { session.drive().await }
        });
        tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
            .await??
            .forget();
        assert_eq!(probe.count(kind).await, 0);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        assert!(
            session.snapshot().await.root_turn().ok_or("turn")?.steps[0]
                .preparation_work
                .iter()
                .all(|record| record.work.kind != kind)
        );
        harness.resume.add_permits(1);
        let done = driving.await??;
        assert_eq!(
            done.run.as_ref().map(|run| run.status),
            Some(RunStatus::Completed)
        );
        let step = &done.root_turn().ok_or("turn")?.steps[0];
        let request_id = &step.plan.as_ref().ok_or("plan")?.request_id;
        assert_eq!(step.preparation_work.len(), 10);
        assert!(
            step.preparation_work
                .iter()
                .all(|record| record.report.is_some() && &record.work.request_id == request_id)
        );
        assert_eq!(probe.count(Kind::ModelSelection).await, 1);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    }
    Ok(())
}

#[tokio::test]
async fn cancellation_or_changed_signals_stop_the_next_checker() -> TestResult {
    for cancel in [true, false] {
        let probe = Arc::new(Probe::new(Some(Kind::RequestCheck), false));
        let (session, executor) =
            session(Arc::new(Harness::new(None, None)), probe.clone()).await?;
        let run_id = session.start("input", 1, task()).await?.assigned_ids["run_id"].clone();
        let driving = tokio::spawn({
            let session = session.clone();
            async move { session.drive().await }
        });
        tokio::time::timeout(Duration::from_secs(5), probe.entered.acquire())
            .await??
            .forget();
        if cancel {
            session
                .cancel_run("cancel", session.head().await.state_revision, &run_id)
                .await?;
        } else {
            session
                .signals("changed", signal_update(&session, Vec::new()).await)
                .await?;
        }
        probe.resume.add_permits(1);
        let _ = driving.await?;
        assert_eq!(probe.count(Kind::RequestCheck).await, 1);
        assert_eq!(probe.count(Kind::ModelSelection).await, 0);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        let state = session.snapshot().await;
        let checker = state.root_turn().ok_or("turn")?.steps[0]
            .preparation_work
            .iter()
            .find(|record| record.work.kind == Kind::RequestCheck)
            .ok_or("checker")?;
        assert!(checker.report.is_some());
        assert!(
            state.cost_work[&run_id]
                .work
                .values()
                .any(|work| work.kind == CostWorkKind::PreparationCallback
                    && work.state == CostWorkState::OutcomeRecorded)
        );
    }
    Ok(())
}

#[tokio::test]
async fn lost_checker_outcome_ack_blocks_following_callbacks() -> TestResult {
    let harness = Arc::new(PhaseHarness::new(Kind::RequestCheck, true, true));
    let probe = Arc::new(Probe::new(None, false));
    let (session, executor) = session(harness.clone(), probe.clone()).await?;
    session.start("input", 1, task()).await?;
    assert!(session.drive().await.is_err());
    let live = session.snapshot().await;
    let last = live.root_turn().ok_or("turn")?.steps[0]
        .preparation_work
        .last()
        .ok_or("checker")?;
    assert_eq!(last.work.kind, Kind::RequestCheck);
    assert!(last.report.is_none());
    let store = harness.inner.store.lock().await;
    let persisted: SessionSnapshot = serde_json::from_value(
        store
            .batches
            .last()
            .ok_or("checkpoint")?
            .decode(&store.limits)?
            .checkpoint
            .state,
    )?;
    assert!(
        persisted.root_turn().ok_or("turn")?.steps[0]
            .preparation_work
            .last()
            .ok_or("checker")?
            .report
            .is_some()
    );
    assert_eq!(probe.count(Kind::RequestCheck).await, 1);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn denied_checker_retains_sdk_cost_identity_without_a_model_attempt() -> TestResult {
    let probe = Arc::new(Probe::new(None, true));
    let (session, executor) = session(Arc::new(Harness::new(None, None)), probe.clone()).await?;
    session.start("input", 1, task()).await?;
    let done = session.drive().await?;
    let run = done.run.as_ref().ok_or("run")?;
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.model_attempts, 0);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    let step = &done.root_turn().ok_or("turn")?.steps[0];
    assert!(step.plan.is_none());
    let checker = step.preparation_work.last().ok_or("checker")?;
    assert_eq!(checker.work.kind, Kind::RequestCheck);
    assert!(
        checker
            .report
            .as_ref()
            .ok_or("report")?
            .error_code
            .is_some()
    );
    assert_eq!(
        probe.cost_reads.lock().await.as_slice(),
        std::slice::from_ref(&checker.work.request_id)
    );
    let ledger = &done.cost_work[&run.run_id];
    assert_eq!(
        ledger.charge_unknown[&checker.work.request_id],
        "fixture_cost_unavailable"
    );
    assert!(ledger.charges.is_empty());
    assert!(
        ledger
            .work
            .values()
            .filter(|work| work.kind == CostWorkKind::PreparationCallback)
            .all(|work| work.state == CostWorkState::OutcomeRecorded
                && work.elapsed_ms.is_some()
                && work.token_estimate.is_none()
                && work.unknown_cost_reason == "cost_not_reported")
    );
    Ok(())
}

#[tokio::test]
async fn preparation_ack_wait_does_not_exhaust_active_time_or_inflate_work() -> TestResult {
    for outcome in [false, true] {
        let harness = Arc::new(PhaseHarness::new(Kind::RequestCheck, outcome, false));
        let probe = Arc::new(Probe::new(None, false));
        let (session, _) = session(harness.clone(), probe).await?;
        let mut input = task();
        input.limits = Some(Limits {
            active_seconds: 1,
            ..Limits::default()
        });
        session.start("input", 1, input).await?;
        let driving = tokio::spawn({
            let session = session.clone();
            async move { session.drive().await }
        });
        tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
            .await??
            .forget();
        tokio::time::sleep(Duration::from_millis(1100)).await;
        harness.resume.add_permits(1);
        let done = driving.await??;
        let run = done.run.as_ref().ok_or("run")?;
        assert_eq!(run.status, RunStatus::Completed);
        assert!(run.active_ms < 1000);
        let checker = done.root_turn().ok_or("turn")?.steps[0]
            .preparation_work
            .iter()
            .find(|record| record.work.kind == Kind::RequestCheck)
            .ok_or("checker")?;
        assert!(checker.report.as_ref().ok_or("report")?.elapsed_ms < 500);
    }
    Ok(())
}

struct BudgetRace {
    root_id: Mutex<String>,
    root_seen: Semaphore,
    root_resume: Semaphore,
    child_seen: Semaphore,
    child_resume: Semaphore,
    child_released: Semaphore,
}

#[derive(Clone)]
struct RaceHook(Arc<BudgetRace>);

impl RaceHook {
    async fn is_root(&self, ctx: &PipelineContext) -> bool {
        let root = self.0.root_id.lock().await;
        ctx.prompt()
            .system
            .as_ref()
            .is_some_and(|system| system.starts_with(&format!("You are agent {root} ")))
    }
}

#[async_trait]
impl PreRequestHook for RaceHook {
    async fn check(&self, ctx: &mut PipelineContext) -> bitrouter_sdk::Result<HookDecision> {
        if self.is_root(ctx).await {
            self.0.root_seen.add_permits(1);
            self.0
                .root_resume
                .acquire()
                .await
                .map_err(|_| bitrouter_sdk::BitrouterError::internal("fixture stopped"))?
                .forget();
        }
        Ok(HookDecision::Allow)
    }
}

#[async_trait]
impl ObserveHook for RaceHook {
    async fn after_phase(&self, _: Phase, _: &PipelineContext) {}
    async fn on_stream_part(&self, _: &StreamContext, _: &StreamPart) {}
    async fn on_request_end(&self, _: &PipelineContext, _: &RequestOutcome) {}
    async fn on_hop_start(&self, ctx: &PipelineContext, _: &RoutingTarget) {
        if !self.is_root(ctx).await {
            self.0.child_seen.add_permits(1);
            if let Ok(permit) = self.0.child_resume.acquire().await {
                permit.forget();
            }
            self.0.child_released.add_permits(1);
        }
    }
}

#[tokio::test]
async fn preparation_rechecks_attempt_budget_consumed_during_its_ack() -> TestResult {
    let harness = Arc::new(PhaseHarness::new(Kind::RequestCheck, false, false));
    let probe = Arc::new(Probe::new(None, false));
    let race = Arc::new(BudgetRace {
        root_id: Mutex::new(String::new()),
        root_seen: Semaphore::new(0),
        root_resume: Semaphore::new(0),
        child_seen: Semaphore::new(0),
        child_resume: Semaphore::new(0),
        child_released: Semaphore::new(0),
    });
    let hook = RaceHook(race.clone());
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(probe.clone())
                .request_checker_runner(probe.clone())
                .executor(Arc::new(MockExecutor::new(vec![output(vec![text(
                    "done",
                )])])))
                .pre_request_hook(hook.clone())
                .observe_hook(hook);
        })
        .build()?;
    let session = bind_app(Arc::new(app), harness.clone()).await?;
    let mut input = task();
    input.limits = Some(Limits {
        model_attempts: 1,
        ..Limits::default()
    });
    let root = session.start("input", 1, input).await?.assigned_ids["agent_id"].clone();
    *race.root_id.lock().await = root.clone();
    *harness.agent_id.lock().await = Some(root.clone());
    session
        .collaborate(
            "child",
            session.head().await.state_revision,
            &root,
            Action::Spawn {
                task: work("use the final attempt"),
            },
        )
        .await?;
    let driving = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(5), race.root_seen.acquire())
        .await??
        .forget();
    tokio::time::timeout(Duration::from_secs(5), race.child_seen.acquire())
        .await??
        .forget();
    race.root_resume.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    assert_eq!(
        session
            .snapshot()
            .await
            .run
            .as_ref()
            .ok_or("run")?
            .model_attempts,
        0
    );
    // The current-thread executor runs the child from this release through its
    // next yield: its attempt transition queues on the held commit mutex before
    // the root's post-ACK dispatch check can queue on that same mutex.
    race.child_resume.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), race.child_released.acquire())
        .await??
        .forget();
    harness.resume.add_permits(1);
    let _ = tokio::time::timeout(Duration::from_secs(5), driving).await??;
    let state = session.snapshot().await;
    assert_eq!(state.run.as_ref().ok_or("run")?.model_attempts, 1);
    let pending = state.root_turn().ok_or("root")?.steps[0]
        .preparation_work
        .last()
        .ok_or("intent")?;
    assert_eq!(pending.work.kind, Kind::RequestCheck);
    assert!(
        pending.report.is_none(),
        "root callback must not start after the child reserves the final attempt"
    );
    assert_eq!(
        probe.count(Kind::RequestCheck).await,
        2,
        "only the child's two checks ran"
    );
    Ok(())
}
