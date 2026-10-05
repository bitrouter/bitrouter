//! Committed cancellation stops only the affected live provider futures.

#[path = "cancellation/start_fences.rs"]
mod start_fences;

use super::*;
use bitrouter_orchestrator::core::session::AgentStatus;
use std::collections::BTreeMap;

struct HeldCall {
    started: Semaphore,
    release: Semaphore,
    stopped: Semaphore,
    calls: AtomicUsize,
    completed: AtomicBool,
}

impl HeldCall {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            started: Semaphore::new(0),
            release: Semaphore::new(0),
            stopped: Semaphore::new(0),
            calls: AtomicUsize::new(0),
            completed: AtomicBool::new(false),
        })
    }
}

struct StopNotice(Arc<HeldCall>);

impl Drop for StopNotice {
    fn drop(&mut self) {
        self.0.stopped.add_permits(1);
    }
}

#[derive(Default)]
struct HeldExecutor(Mutex<BTreeMap<String, Arc<HeldCall>>>);

#[async_trait]
impl Executor for HeldExecutor {
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> bitrouter_sdk::Result<ExecutionResult> {
        let held = self
            .0
            .lock()
            .await
            .iter()
            .find(|(agent_id, _)| {
                prompt.system.as_ref().is_some_and(|system| {
                    system.starts_with(&format!("You are agent {agent_id} for this task."))
                })
            })
            .map(|(_, held)| held.clone())
            .ok_or_else(|| bitrouter_sdk::BitrouterError::internal("fixture actor missing"))?;
        if held.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let _stopped = StopNotice(held.clone());
            held.started.add_permits(1);
            held.release
                .acquire()
                .await
                .map_err(|error| bitrouter_sdk::BitrouterError::internal(error.to_string()))?
                .forget();
            held.completed.store(true, Ordering::SeqCst);
        }
        MockExecutor::always_text("completed")
            .execute(target, prompt, ctx)
            .await
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

async fn spawn_child(session: &CoreSession, actor: &str, name: &str) -> Result<String, CoreError> {
    let receipt = session
        .collaborate(
            name,
            session.head().await.state_revision,
            actor,
            Action::Spawn { task: work(name) },
        )
        .await?;
    receipt
        .assigned_ids
        .get("agent_id")
        .cloned()
        .ok_or_else(|| CoreError::rejected(ErrorCode::OperationConflict, "fixture child missing"))
}

#[tokio::test]
async fn committed_interrupt_stops_only_its_subtree_and_preserves_unknown_usage() -> TestResult {
    let harness = Arc::new(Harness::new(None, Some("collaboration.runtime")));
    harness.hold_enabled.store(false, Ordering::SeqCst);
    let executor = Arc::new(HeldExecutor::default());
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first"), target("fallback")]);
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone());
        })
        .build()?;
    let session = bind_app(Arc::new(app), harness.clone()).await?;
    let root = session.start("input", 1, input()).await?.assigned_ids["agent_id"].clone();
    let child = spawn_child(&session, &root, "child").await?;
    let grandchild = spawn_child(&session, &child, "grandchild").await?;
    let sibling = spawn_child(&session, &root, "sibling").await?;
    let calls = [&root, &child, &grandchild, &sibling]
        .into_iter()
        .map(|id| (id.clone(), HeldCall::new()))
        .collect::<BTreeMap<_, _>>();
    *executor.0.lock().await = calls.clone();
    let driver = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    for held in calls.values() {
        tokio::time::timeout(Duration::from_secs(5), held.started.acquire())
            .await??
            .forget();
    }
    assert_eq!(
        session
            .collaborate(
                "stale",
                0,
                &root,
                Action::Interrupt {
                    agent_id: child.clone()
                }
            )
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::StaleRevision)
    );
    harness.hold_enabled.store(true, Ordering::SeqCst);
    let interrupt = tokio::spawn({
        let session = session.clone();
        let root = root.clone();
        let child = child.clone();
        async move {
            session
                .collaborate(
                    "interrupt",
                    session.head().await.state_revision,
                    &root,
                    Action::Interrupt { agent_id: child },
                )
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    for held in calls.values() {
        assert_eq!(
            held.stopped.available_permits(),
            0,
            "no executor stops before ACK"
        );
    }
    harness.hold_enabled.store(false, Ordering::SeqCst);
    harness.resume.add_permits(1);
    interrupt.await??;
    for id in [&child, &grandchild] {
        tokio::time::timeout(Duration::from_secs(5), calls[id].stopped.acquire())
            .await??
            .forget();
        assert!(!calls[id].completed.load(Ordering::SeqCst));
        assert_eq!(calls[id].calls.load(Ordering::SeqCst), 1);
    }
    for id in [&root, &sibling] {
        assert_eq!(calls[id].stopped.available_permits(), 0);
        calls[id].release.add_permits(1);
    }
    let done = tokio::time::timeout(Duration::from_secs(5), driver).await???;
    assert_eq!(done.run.as_ref().ok_or("run")?.status, RunStatus::Completed);
    for id in [&child, &grandchild] {
        let turn = done.agents[id].turn.as_ref().ok_or("turn")?;
        assert_eq!(turn.status, AgentStatus::Interrupted);
        assert!(turn.steps[0].settled);
        assert_eq!(
            turn.steps[0].attempts.len(),
            1,
            "no fallback after interruption"
        );
        let receipt = turn.steps[0].attempts[0]
            .receipt
            .as_ref()
            .ok_or("receipt")?;
        assert!(receipt.report.result.is_none());
        assert!(
            receipt
                .report
                .error
                .as_ref()
                .is_some_and(|error| error.contains("cancelled"))
        );
        assert!(receipt.cost_micro_usd.is_none());
    }
    assert!(calls[&root].completed.load(Ordering::SeqCst));
    assert!(calls[&sibling].completed.load(Ordering::SeqCst));
    assert!(harness.sent.lock().await.is_empty());
    Ok(())
}
