//! Capacity waits use real admission and model barriers, not scheduler sleeps.
use super::*;
use std::sync::atomic::AtomicBool;

struct Gate {
    open: AtomicBool,
    visits: tokio::sync::mpsc::UnboundedSender<()>,
    model: Control,
}

impl Gate {
    fn new() -> (Arc<Self>, tokio::sync::mpsc::UnboundedReceiver<()>) {
        let (visits, receiver) = tokio::sync::mpsc::unbounded_channel();
        (
            Arc::new(Self {
                open: AtomicBool::new(true),
                visits,
                model: Control {
                    acknowledged: true,
                    attempts: AtomicUsize::new(0),
                },
            }),
            receiver,
        )
    }
}

#[async_trait::async_trait]
impl NativeExecutionControl for Gate {
    async fn plan(&self, plan: NativePlan) -> bitrouter_sdk::Result<NativePlanAdmission> {
        self.model.plan(plan).await
    }
    async fn before_attempt(&self, id: &str, index: u32) -> bitrouter_sdk::Result<()> {
        self.model.before_attempt(id, index).await
    }
    async fn after_attempt(&self, _: NativeAttemptReport) {}
}

#[async_trait::async_trait]
impl WorkControl for Gate {
    async fn authorize(&self) -> bitrouter_sdk::Result<()> {
        let _ = self.visits.send(());
        if self.open.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(bitrouter_sdk::error::BitrouterError::bad_request(
                "waiting source withdrawn",
            ))
        }
    }
    fn outcome_acknowledged(&self) -> bool {
        true
    }
}

async fn signal(
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    tokio::time::timeout(Duration::from_secs(3), receiver.recv())
        .await?
        .ok_or("controlled execution barrier closed")?;
    Ok(())
}

struct Capacity {
    service: ThreadService,
    input: crate::thread::ThreadTarget,
    root: crate::turn::TurnReceipt,
    executor: Arc<HeldExecutor>,
    arrivals: tokio::sync::mpsc::UnboundedReceiver<()>,
}

impl Capacity {
    async fn new(
        path: &std::path::Path,
        duration: Duration,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let (entered, mut arrivals) = tokio::sync::mpsc::unbounded_channel();
        let executor = Arc::new(HeldExecutor {
            entered,
            release: tokio::sync::Semaphore::new(0),
            inner: MockExecutor::new(vec![
                MockResponse::Generate(final_turn()),
                MockResponse::Generate(final_turn()),
            ]),
        });
        let service = ThreadService::with_limits(
            app_with_executor(executor.clone())?,
            &[],
            crate::service::RuntimeLimits {
                active_turns: 1,
                ..Default::default()
            },
        )?;
        let root = owner(&service, path).await?;
        let root_target = target(&root);
        let mut config = service
            .read_stored_thread_view(&root_target, &CallerContext::local())
            .await?
            .config;
        config.max_duration = duration;
        let input = service
            .create_goal_input_thread(
                &root_target,
                crate::thread::ThreadRequest {
                    caller: CallerContext::local(),
                    workspace: path.into(),
                    config,
                    permission_profile: PermissionProfile::ReadOnly,
                    verification_command: None,
                    idempotency_key: crate::goal::workflow::input_thread_key("goal"),
                },
            )
            .await?;
        let root = service
            .start_goal_generation(
                &root_target,
                &CallerContext::local(),
                request(GenerationPurpose::Planning, "held-capacity"),
                Arc::new(Control {
                    acknowledged: true,
                    attempts: AtomicUsize::new(0),
                }),
            )
            .await?;
        signal(&mut arrivals).await?;
        Ok(Self {
            service,
            input: target(&input),
            root,
            executor,
            arrivals,
        })
    }

    async fn close(self) {
        self.executor.release.add_permits(2);
        self.service.shutdown().await;
    }
}

#[tokio::test]
async fn generation_capacity_wait_reuses_its_key_and_rejects_conflicting_input()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let mut capacity = Capacity::new(directory.path(), Duration::from_secs(10)).await?;
    let (control, mut visits) = Gate::new();
    let input = request(GenerationPurpose::Input, "waiting-input");
    let waiting = {
        let service = capacity.service.clone();
        let target = capacity.input.clone();
        let input = input.clone();
        let control = control.clone();
        tokio::spawn(async move {
            service
                .wait_goal_generation(&target, &CallerContext::local(), input, control)
                .await
        })
    };
    // Two source checks prove an actual refusal and a new wait iteration while
    // the first provider call still holds the only active slot.
    signal(&mut visits).await?;
    signal(&mut visits).await?;
    assert_eq!(
        capacity.service.read(&capacity.root.turn_id)?.status,
        TurnStatus::Running
    );
    assert!(
        capacity
            .service
            .find_generation_admission(&capacity.input, &CallerContext::local(), &input.origin)
            .await?
            .is_none()
    );
    capacity.executor.release.add_permits(1);
    let receipt = tokio::time::timeout(Duration::from_secs(3), waiting).await???;
    signal(&mut capacity.arrivals).await?;
    assert_ne!(receipt.turn_id, capacity.root.turn_id);
    let mut conflicting = input.clone();
    conflicting.prompt.messages = vec![Message::text(
        Role::User,
        "changed input on the accepted key",
    )];
    let error = capacity
        .service
        .wait_goal_generation(
            &capacity.input,
            &CallerContext::local(),
            conflicting,
            control.clone(),
        )
        .await
        .err()
        .ok_or("changed input reused the accepted key")?;
    assert_eq!(error.code, ErrorCode::Conflict);
    capacity.executor.release.add_permits(1);
    let result = finished(&capacity.service, &receipt.turn_id).await?;
    assert_eq!(result.status, TurnStatus::Completed, "{:?}", result.detail);
    control.open.store(false, Ordering::SeqCst);
    let repeated = capacity
        .service
        .wait_goal_generation(
            &capacity.input,
            &CallerContext::local(),
            input,
            control.clone(),
        )
        .await?;
    assert_eq!(repeated.turn_id, receipt.turn_id);
    assert_eq!(control.model.attempts.load(Ordering::SeqCst), 1);
    capacity.close().await;
    Ok(())
}

#[tokio::test]
async fn generation_capacity_wait_bounds_time_and_honors_revocation_and_input_limits()
-> Result<(), Box<dyn std::error::Error>> {
    for reason in ["deadline", "revoked", "input_bound"] {
        let directory = tempfile::tempdir()?;
        let duration = if reason == "deadline" {
            Duration::from_millis(25)
        } else {
            Duration::from_secs(10)
        };
        let capacity = Capacity::new(directory.path(), duration).await?;
        let (control, mut visits) = Gate::new();
        let mut input = request(GenerationPurpose::Input, reason);
        if reason == "input_bound" {
            input.prompt.messages = vec![Message::text(
                Role::User,
                "x".repeat(
                    capacity
                        .service
                        .capabilities()
                        .limits
                        .context_bytes_per_thread
                        + 1,
                ),
            )];
        }
        let waiting = {
            let service = capacity.service.clone();
            let target = capacity.input.clone();
            let input = input.clone();
            let control = control.clone();
            tokio::spawn(async move {
                service
                    .wait_goal_generation(&target, &CallerContext::local(), input, control)
                    .await
            })
        };
        if reason == "revoked" {
            signal(&mut visits).await?;
            signal(&mut visits).await?;
            control.open.store(false, Ordering::SeqCst);
        }
        let error = tokio::time::timeout(Duration::from_secs(3), waiting)
            .await??
            .err()
            .ok_or("capacity failure admitted a new Turn")?;
        match reason {
            "deadline" => {
                assert_eq!(error.code, ErrorCode::Overloaded);
                assert!(error.message.contains("wait expired"));
            }
            "revoked" => assert_eq!(error.code, ErrorCode::Unauthorized),
            _ => {
                assert_eq!(error.code, ErrorCode::InvalidRequest);
                assert!(error.message.contains("context bound"));
            }
        }
        assert!(
            capacity
                .service
                .find_generation_admission(&capacity.input, &CallerContext::local(), &input.origin)
                .await?
                .is_none()
        );
        assert_eq!(control.model.attempts.load(Ordering::SeqCst), 0);
        assert_eq!(
            capacity.service.read(&capacity.root.turn_id)?.status,
            TurnStatus::Running
        );
        capacity.close().await;
    }
    Ok(())
}
