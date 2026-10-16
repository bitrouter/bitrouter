//! Actual native admission/model barriers distinguish contention from recovery.
use super::*;
use crate::service::RuntimeLimits;
use crate::service::tests::support::{HeldModel, app_with_executor};
use std::time::Duration;

struct Gate {
    budget: Arc<SharedBudget>,
    visits: tokio::sync::mpsc::UnboundedSender<()>,
}

#[async_trait::async_trait]
impl NativeExecutionControl for Gate {
    async fn plan(&self, plan: NativePlan) -> bitrouter_sdk::Result<NativePlanAdmission> {
        self.budget.plan(plan).await
    }
    async fn before_attempt(&self, id: &str, index: u32) -> bitrouter_sdk::Result<()> {
        self.budget.before_attempt(id, index).await
    }
    async fn after_attempt(&self, report: NativeAttemptReport) {
        self.budget.after_attempt(report).await;
    }
}

#[async_trait::async_trait]
impl WorkControl for Gate {
    async fn authorize(&self) -> bitrouter_sdk::Result<()> {
        let result = self.budget.authorize().await;
        let _ = self.visits.send(());
        result
    }
    fn outcome_acknowledged(&self) -> bool {
        self.budget.outcome_acknowledged()
    }
}

fn gate() -> (Arc<Gate>, tokio::sync::mpsc::UnboundedReceiver<()>) {
    let (visits, receiver) = tokio::sync::mpsc::unbounded_channel();
    (
        Arc::new(Gate {
            budget: decisions::control(1, true),
            visits,
        }),
        receiver,
    )
}

async fn refusal(
    visits: &mut tokio::sync::mpsc::UnboundedReceiver<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    // The outer live check and the underlying admission check happen on the
    // first pass. A third visit proves a second pass after an actual refusal.
    for _ in 0..3 {
        tokio::time::timeout(Duration::from_secs(3), visits.recv())
            .await?
            .ok_or("waiting source check channel closed")?;
    }
    Ok(())
}

struct Capacity {
    service: ThreadService,
    worker: ThreadSnapshot,
    source: GoalWork,
    model: Arc<HeldModel>,
    store: Arc<MemoryExecutionStore>,
    first: crate::turn::TurnReceipt,
    _directories: Vec<tempfile::TempDir>,
}

impl Capacity {
    async fn new(
        shared_workspace: bool,
        duration: Duration,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let first = tempfile::tempdir()?;
        let second = tempfile::tempdir()?;
        let model = Arc::new(HeldModel::with_turns(vec![final_turn(), final_turn()]));
        let store = Arc::new(MemoryExecutionStore::default());
        let service = ThreadService::with_limits_and_store(
            app_with_executor(model.clone())?,
            &[first.path().into(), second.path().into()],
            RuntimeLimits {
                active_turns: if shared_workspace { 2 } else { 1 },
                ..Default::default()
            },
            store.clone(),
        )?;
        let root = root(&service, &first).await?;
        let (held, held_source) = worker(&service, &root, &first, "held").await?;
        let directory = if shared_workspace { &first } else { &second };
        let source = GoalWork {
            owner_thread_id: root.thread_id.clone(),
            goal_id: "goal".into(),
            task_id: "waiting".into(),
            execution_id: "waiting-attempt".into(),
            task_revision: 1,
            control_source_revision: 0,
        };
        let mut request = thread_request(directory, &crate::service::worker::thread_key(&source)?);
        request.config.max_duration = duration;
        let worker = service
            .create_goal_worker_thread(&target(&root), &source, request, None)
            .await?;
        let first_turn = service
            .start_goal_worker_turn(
                &target(&held),
                &CallerContext::local(),
                held_source,
                input("hold native provider", "held"),
                decisions::control(1, true),
            )
            .await?;
        model.wait().await?;
        Ok(Self {
            service,
            worker,
            source,
            model,
            store,
            first: first_turn,
            _directories: vec![first, second],
        })
    }

    fn wait(
        &self,
        control: Arc<Gate>,
    ) -> tokio::task::JoinHandle<Result<crate::turn::TurnReceipt, crate::service::ServiceError>>
    {
        let service = self.service.clone();
        let target = target(&self.worker);
        let source = self.source.clone();
        tokio::spawn(async move {
            service
                .wait_goal_worker_turn(
                    &target,
                    &CallerContext::local(),
                    source,
                    input("continue the original work", "original-work"),
                    control,
                )
                .await
        })
    }

    async fn assert_unaccepted(&self) -> Result<(), Box<dyn std::error::Error>> {
        assert!(
            self.service
                .find_goal_worker_turn(
                    &target(&self.worker),
                    &CallerContext::local(),
                    "original-work",
                )
                .await?
                .is_none()
        );
        assert_eq!(
            self.service.read(&self.first.turn_id)?.status,
            TurnStatus::Running
        );
        assert_eq!(self.model.calls.load(Ordering::SeqCst), 1);
        let events = self
            .store
            .load(&self.worker.thread_id)
            .await?
            .ok_or("worker root missing")?;
        assert!(!events.records.iter().any(|record| matches!(
            record,
            ExecutionRecord::TurnQueued { .. } | ExecutionRecord::TurnActivated { .. }
        )));
        Ok(())
    }

    async fn close(self) {
        self.model.release.add_permits(1);
        self.service.shutdown().await;
    }
}

#[tokio::test]
async fn native_worker_waits_for_known_capacity_and_replays_only_original_admission()
-> Result<(), Box<dyn std::error::Error>> {
    for shared_workspace in [false, true] {
        let capacity = Capacity::new(shared_workspace, Duration::from_secs(10)).await?;
        let (control, mut visits) = gate();
        let waiting = capacity.wait(control.clone());
        refusal(&mut visits).await?;
        capacity.assert_unaccepted().await?;
        assert_eq!(control.budget.admitted.load(Ordering::SeqCst), 0);
        capacity.model.release.add_permits(1);
        let receipt = tokio::time::timeout(Duration::from_secs(3), waiting).await???;
        assert_ne!(receipt.turn_id, capacity.first.turn_id);
        let finished = wait_for(&capacity.service, &receipt.turn_id, TurnStatus::Completed).await?;
        assert_eq!(
            finished.status,
            TurnStatus::Completed,
            "{:?}",
            finished.detail
        );
        assert!(!finished.unknown_effect);
        assert_eq!(capacity.model.calls.load(Ordering::SeqCst), 2);
        control.budget.revoked.store(true, Ordering::SeqCst);
        let repeated = capacity.wait(control.clone()).await??;
        assert_eq!(repeated.turn_id, receipt.turn_id);
        let error = capacity
            .service
            .wait_goal_worker_turn(
                &target(&capacity.worker),
                &CallerContext::local(),
                capacity.source.clone(),
                input("changed body on the same key", "original-work"),
                control.clone(),
            )
            .await
            .err()
            .ok_or("changed input reused admission")?;
        assert_eq!(error.code, ErrorCode::Conflict);
        assert_eq!(control.budget.admitted.load(Ordering::SeqCst), 1);
        assert_eq!(control.budget.observed.load(Ordering::SeqCst), 1);
        capacity.close().await;
    }
    Ok(())
}

#[tokio::test]
async fn native_worker_wait_refuses_withdrawn_sources_deadlines_and_unknown_workspace_owners()
-> Result<(), Box<dyn std::error::Error>> {
    for reason in [
        "revoked",
        "grant_withdrawn",
        "deadline",
        "unknown_workspace",
        "shutdown",
        "input_bound",
    ] {
        let duration = if reason == "deadline" {
            Duration::from_millis(250)
        } else {
            Duration::from_secs(10)
        };
        let capacity = Capacity::new(reason == "unknown_workspace", duration).await?;
        let (control, mut visits) = gate();
        let waiting = if reason == "input_bound" {
            let error = capacity
                .service
                .wait_goal_worker_turn(
                    &target(&capacity.worker),
                    &CallerContext::local(),
                    capacity.source.clone(),
                    input(
                        &"x".repeat(capacity.service.inner.limits.request_bytes + 1),
                        "original-work",
                    ),
                    control.clone(),
                )
                .await
                .err()
                .ok_or("oversized request waited or dispatched")?;
            assert_eq!(error.code, ErrorCode::InvalidRequest);
            None
        } else {
            Some(capacity.wait(control.clone()))
        };
        if let Some(waiting) = waiting {
            if reason != "deadline" {
                refusal(&mut visits).await?;
            }
            capacity.assert_unaccepted().await?;
            let expected = match reason {
                "revoked" => {
                    control.budget.revoked.store(true, Ordering::SeqCst);
                    ErrorCode::Unauthorized
                }
                "grant_withdrawn" => {
                    capacity.service.lock_state().workspace_profiles.insert(
                        capacity.worker.workspace.clone(),
                        vec![PermissionProfile::ReadOnly],
                    );
                    ErrorCode::Unauthorized
                }
                "unknown_workspace" => {
                    // Model the service's conservative unknown-effect fence;
                    // the actual owner remains running and must not be stolen.
                    let mut state = capacity.service.lock_state();
                    let held = state
                        .turns
                        .get_mut(&capacity.first.turn_id)
                        .ok_or("held Turn missing")?;
                    held.snapshot.status = TurnStatus::RecoveryRequired;
                    held.snapshot.unknown_effect = true;
                    ErrorCode::RecoveryRequired
                }
                "shutdown" => {
                    capacity.service.lock_state().closing = true;
                    ErrorCode::ShuttingDown
                }
                _ => ErrorCode::Overloaded,
            };
            capacity.service.inner.runtime_changed.notify_waiters();
            let error = tokio::time::timeout(Duration::from_secs(3), waiting)
                .await??
                .err()
                .ok_or("closed wait admitted work")?;
            assert_eq!(error.code, expected, "{reason}: {error}");
        }
        let root = capacity
            .store
            .load(&capacity.worker.thread_id)
            .await?
            .ok_or("worker root missing")?;
        assert!(
            !root
                .records
                .iter()
                .any(|record| matches!(record, ExecutionRecord::TurnQueued { .. }))
        );
        assert_eq!(control.budget.admitted.load(Ordering::SeqCst), 0);
        assert_eq!(capacity.model.calls.load(Ordering::SeqCst), 1);
        capacity.close().await;
    }
    Ok(())
}

#[tokio::test]
async fn native_worker_wait_never_treats_a_foreign_os_workspace_lock_as_known_contention()
-> Result<(), Box<dyn std::error::Error>> {
    let capacity = Capacity::new(true, Duration::from_secs(10)).await?;
    let directory = &capacity._directories[0];
    let service = ThreadService::new(
        app_with_executor(capacity.model.clone())?,
        &[directory.path().into()],
    )?;
    let owner = root(&service, directory).await?;
    let (thread, source) = worker(&service, &owner, directory, "other-runtime").await?;
    let (control, _) = gate();
    let error = tokio::time::timeout(
        Duration::from_secs(3),
        service.wait_goal_worker_turn(
            &target(&thread),
            &CallerContext::local(),
            source,
            input("must not steal the OS lock", "foreign-owner"),
            control.clone(),
        ),
    )
    .await?
    .err()
    .ok_or("foreign OS lock was bypassed")?;
    assert_eq!(error.code, ErrorCode::Conflict);
    assert!(!error.message.contains("wait expired"));
    assert!(
        service
            .find_goal_worker_turn(&target(&thread), &CallerContext::local(), "foreign-owner")
            .await?
            .is_none()
    );
    assert_eq!(control.budget.admitted.load(Ordering::SeqCst), 0);
    assert_eq!(capacity.model.calls.load(Ordering::SeqCst), 1);
    service.shutdown().await;
    capacity.close().await;
    Ok(())
}
