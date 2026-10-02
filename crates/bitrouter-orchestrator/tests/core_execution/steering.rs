use super::*;
use bitrouter_orchestrator::core::checkpoint::ToolStartFence;
use bitrouter_orchestrator::core::protocol::{ToolObservation, ToolStatus};
use bitrouter_orchestrator::core::session::steering::SteeringDisposition;

struct Barrier {
    seen: Semaphore,
    release: Semaphore,
}
impl Barrier {
    fn new() -> Self {
        Self {
            seen: Semaphore::new(0),
            release: Semaphore::new(0),
        }
    }
    async fn hold(&self) {
        self.seen.add_permits(1);
        if let Ok(permit) = self.release.acquire().await {
            permit.forget();
        }
    }
    async fn reached(&self) -> TestResult {
        tokio::time::timeout(Duration::from_secs(30), self.seen.acquire())
            .await??
            .forget();
        Ok(())
    }
}

#[derive(Clone)]
struct HeldFinalization {
    first: Arc<AtomicBool>,
    barrier: Arc<Barrier>,
    others: Arc<Semaphore>,
}
#[async_trait]
impl ObserveHook for HeldFinalization {
    async fn after_phase(&self, _: Phase, _: &PipelineContext) {}
    async fn on_stream_part(&self, _: &StreamContext, _: &StreamPart) {}
    async fn on_request_end(&self, _: &PipelineContext, _: &RequestOutcome) {
        if self.first.swap(false, Ordering::SeqCst) {
            self.barrier.hold().await;
        } else {
            self.others.add_permits(1);
        }
    }
}

#[tokio::test]
async fn steering_awaits_abandoned_sdk_finalization_without_starving_another_agent() -> TestResult {
    let hook = HeldFinalization {
        first: Arc::new(AtomicBool::new(true)),
        barrier: Arc::new(Barrier::new()),
        others: Arc::new(Semaphore::new(0)),
    };
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first")]);
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(Arc::new(MockExecutor::new(
                    std::iter::once(output(vec![call("old-read")]))
                        .chain((0..8).map(|_| output(vec![text("done")])))
                        .collect(),
                )))
                .observe_hook(hook.clone());
        })
        .build()?;
    let harness = Arc::new(Harness::new(None, None));
    let session = bind_app(Arc::new(app), harness.clone()).await?;
    let mut task = input();
    task.limits = Some(Limits {
        active_models: 1,
        ..Limits::default()
    });
    let accepted = session.start("input", 1, task).await?;
    let original = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    hook.barrier.reached().await?;
    original.abort();
    let _ = original.await;
    let commands = harness.sent.lock().await.clone();
    for command in commands {
        session.tool_result("old-result", result(&command)).await?;
    }
    let child = session
        .collaborate(
            "spawn",
            session.head().await.state_revision,
            &accepted.assigned_ids["agent_id"],
            Action::Spawn {
                task: work("independent child"),
            },
        )
        .await?;
    steer(&session, "steer", "revised root instruction").await?;
    let driving = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(30), hook.others.acquire())
        .await??
        .forget();
    let state = session.snapshot().await;
    assert_eq!(
        state.steering["steer"].disposition,
        SteeringDisposition::Received
    );
    assert!(
        !state.agents[&child.assigned_ids["agent_id"]]
            .turn
            .as_ref()
            .ok_or("child")?
            .steps
            .is_empty()
    );
    hook.barrier.release.add_permits(1);
    let done = tokio::time::timeout(Duration::from_secs(30), driving).await???;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(
        done.steering["steer"].disposition,
        SteeringDisposition::Applied
    );
    Ok(())
}

struct AdmissionPort {
    harness: Arc<Harness>,
    child_id: Mutex<Option<String>>,
    intent: Barrier,
    input: Barrier,
    control_enabled: AtomicBool,
}

#[tokio::test]
async fn steering_preserves_slots_for_an_abandoned_drivers_live_provider() -> TestResult {
    let executor = Arc::new(TwoHeldAgents {
        root: Barrier::new(),
        child: Barrier::new(),
        first_root: AtomicBool::new(true),
    });
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first")]);
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone());
        })
        .build()?;
    let session = bind_app(Arc::new(app), Arc::new(Harness::new(None, None))).await?;
    let mut task = input();
    task.limits = Some(Limits {
        active_models: 1,
        ..Limits::default()
    });
    let accepted = session.start("input", 1, task).await?;
    let original = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    executor.root.reached().await?;
    original.abort();
    assert!(original.await.is_err());
    let child = session
        .collaborate(
            "spawn",
            session.head().await.state_revision,
            &accepted.assigned_ids["agent_id"],
            Action::Spawn {
                task: work("independent child"),
            },
        )
        .await?;
    steer(&session, "steer", "revised root instruction").await?;
    let mut driving = Box::pin(session.drive());
    // Fully poll the new driver to its wait; it must not start B while the
    // detached A provider still owns the sole slot.
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut driving)
            .await
            .is_err()
    );
    assert_eq!(executor.child.seen.available_permits(), 0);
    assert!(
        session.snapshot().await.agents[&child.assigned_ids["agent_id"]]
            .turn
            .as_ref()
            .ok_or("child")?
            .steps
            .is_empty()
    );
    executor.root.release.add_permits(1);
    executor.child.release.add_permits(1);
    let done = tokio::time::timeout(Duration::from_secs(30), driving).await??;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(
        done.steering["steer"].disposition,
        SteeringDisposition::Applied
    );
    executor.child.reached().await?;
    Ok(())
}
#[async_trait]
impl HarnessPort for AdmissionPort {
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
        let payload = batch.decode(&Limits::default())?;
        let child_id = self.child_id.lock().await.clone();
        if payload.events.iter().any(|event| {
            event.kind == "model.attempt.intent" && event.agent_id.as_ref() == child_id.as_ref()
        }) {
            self.intent.hold().await;
        }
        if self.control_enabled.load(Ordering::SeqCst)
            && payload
                .events
                .iter()
                .any(|event| event.kind == "collaboration.runtime")
        {
            self.input.hold().await;
        }
        self.harness.commit(batch).await
    }
    async fn send(&self, message: ServerMessage) -> Result<(), CoreError> {
        self.harness.send(message).await
    }
}

fn child_prompt(prompt: &Prompt) -> bool {
    prompt
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|part| matches!(part, Content::Text{text,..} if text == "independent child"))
}

#[derive(Clone)]
struct HeldChildAdmission(Arc<Barrier>);
#[async_trait]
impl ObserveHook for HeldChildAdmission {
    async fn after_phase(&self, _: Phase, _: &PipelineContext) {}
    async fn on_stream_part(&self, _: &StreamContext, _: &StreamPart) {}
    async fn on_request_end(&self, _: &PipelineContext, _: &RequestOutcome) {}
    async fn on_hop_start(&self, ctx: &PipelineContext, _: &RoutingTarget) {
        if child_prompt(ctx.prompt()) {
            self.0.hold().await;
        }
    }
}

struct TwoHeldAgents {
    root: Barrier,
    child: Barrier,
    first_root: AtomicBool,
}

#[derive(Clone)]
struct HeldProviderAdmission {
    provider: String,
    first: Arc<AtomicBool>,
    barrier: Arc<Barrier>,
}
#[async_trait]
impl ObserveHook for HeldProviderAdmission {
    async fn after_phase(&self, _: Phase, _: &PipelineContext) {}
    async fn on_stream_part(&self, _: &StreamContext, _: &StreamPart) {}
    async fn on_request_end(&self, _: &PipelineContext, _: &RequestOutcome) {}
    async fn on_hop_start(&self, _: &PipelineContext, target: &RoutingTarget) {
        if target.provider_name == self.provider && self.first.swap(false, Ordering::SeqCst) {
            self.barrier.hold().await;
        }
    }
}

#[tokio::test]
async fn steering_before_first_or_fallback_attempt_preserves_attribution_without_dispatch()
-> TestResult {
    for fallback in [false, true] {
        let hook = HeldProviderAdmission {
            provider: if fallback { "second" } else { "first" }.into(),
            first: Arc::new(AtomicBool::new(true)),
            barrier: Arc::new(Barrier::new()),
        };
        let mut responses = Vec::new();
        if fallback {
            responses.push(MockResponse::Error(
                bitrouter_sdk::BitrouterError::Upstream {
                    status: 503,
                    message: "fixture failure".into(),
                },
            ));
        }
        responses.push(output(vec![text("revised answer")]));
        let executor = Arc::new(RecordingExecutor {
            mock: MockExecutor::new(responses),
            agent_once: Mutex::new(Default::default()),
            prompts: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let table = StaticRoutingTable::new();
        table.insert("fixture-model", vec![target("first"), target("second")]);
        let app = App::builder()
            .language_model(|builder| {
                builder
                    .routing_table(Arc::new(table))
                    .executor(executor.clone())
                    .observe_hook(hook.clone());
            })
            .build()?;
        let session = bind_app(Arc::new(app), Arc::new(Harness::new(None, None))).await?;
        session.start("input", 1, input()).await?;
        let driving = tokio::spawn({
            let session = session.clone();
            async move { session.drive().await }
        });
        hook.barrier.reached().await?;
        steer(&session, "steer", "revised instruction").await?;
        assert_eq!(executor.calls.load(Ordering::SeqCst), usize::from(fallback));
        hook.barrier.release.add_permits(1);
        let done = tokio::time::timeout(Duration::from_secs(30), driving).await???;
        assert_eq!(
            done.run.as_ref().map(|run| run.status),
            Some(RunStatus::Completed)
        );
        let steps = &done.root_turn().ok_or("turn")?.steps;
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].attempts.len(), usize::from(fallback));
        assert!(steps[0].interrupted && steps[0].settled);
        assert_eq!(steps[1].attempts.len(), 1);
        assert_eq!(
            executor.calls.load(Ordering::SeqCst),
            1 + usize::from(fallback)
        );
        assert!(
            serde_json::to_string(executor.prompts.lock().await.last().ok_or("prompt")?)?
                .contains("revised instruction")
        );
    }
    Ok(())
}
#[async_trait]
impl Executor for TwoHeldAgents {
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> bitrouter_sdk::Result<ExecutionResult> {
        if child_prompt(prompt) {
            self.child.hold().await;
        } else if self.first_root.swap(false, Ordering::SeqCst) {
            self.root.hold().await;
        }
        MockExecutor::new(vec![output(vec![text("done")])])
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

#[tokio::test]
async fn steering_provisional_fence_does_not_reject_another_agents_admitted_attempt() -> TestResult
{
    for disposition in ["accepted", "stale", "abandoned"] {
        let harness = Arc::new(Harness::new(None, None));
        let port = Arc::new(AdmissionPort {
            harness,
            child_id: Mutex::new(None),
            intent: Barrier::new(),
            input: Barrier::new(),
            control_enabled: AtomicBool::new(false),
        });
        let hook = HeldChildAdmission(Arc::new(Barrier::new()));
        let executor = Arc::new(TwoHeldAgents {
            root: Barrier::new(),
            child: Barrier::new(),
            first_root: AtomicBool::new(true),
        });
        let table = StaticRoutingTable::new();
        table.insert("fixture-model", vec![target("first")]);
        let app = App::builder()
            .language_model(|builder| {
                builder
                    .routing_table(Arc::new(table))
                    .executor(executor.clone())
                    .observe_hook(hook.clone());
            })
            .build()?;
        let session = bind_app(Arc::new(app), port.clone()).await?;
        let accepted = session.start("input", 1, input()).await?;
        let child = session
            .collaborate(
                "spawn",
                session.head().await.state_revision,
                &accepted.assigned_ids["agent_id"],
                Action::Spawn {
                    task: work("independent child"),
                },
            )
            .await?;
        *port.child_id.lock().await = Some(child.assigned_ids["agent_id"].clone());
        let driving = tokio::spawn({
            let session = session.clone();
            async move { session.drive().await }
        });
        executor.root.reached().await?;
        hook.0.reached().await?;
        hook.0.release.add_permits(1);
        port.intent.reached().await?;
        port.control_enabled.store(true, Ordering::SeqCst);
        let control = session.collaborate(
            "control",
            session.head().await.state_revision + 1,
            &accepted.assigned_ids["agent_id"],
            Action::List {},
        );
        tokio::pin!(control);
        // Queue the input serializer behind B's intent ACK. Once that ACK
        // returns, B's final provider admission queues behind this input.
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut control)
                .await
                .is_err()
        );
        port.intent.release.add_permits(1);
        let controlling = tokio::spawn({
            let port = port.clone();
            async move { port.input.reached().await.map_err(|e| e.to_string()) }
        });
        tokio::select! {
            reached = controlling => reached??,
            result = &mut control => return Err(format!("control finished without barrier: {result:?}").into()),
        }
        let revision = if disposition == "stale" {
            0
        } else {
            session.head().await.state_revision + 1
        };
        let mut steering = Box::pin(session.steer(
            "steer",
            revision,
            &accepted.assigned_ids["run_id"],
            &accepted.assigned_ids["agent_turn_id"],
            "revised root instruction".into(),
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut steering)
                .await
                .is_err()
        );
        if disposition == "abandoned" {
            drop(steering);
        } else {
            port.input.release.add_permits(1);
            control.as_mut().await?;
            let received = steering.await;
            if disposition == "stale" {
                assert_eq!(
                    received.err().map(|error| error.code),
                    Some(ErrorCode::StaleRevision)
                );
            } else {
                received?;
            }
        }
        if disposition == "abandoned" {
            port.input.release.add_permits(1);
            control.as_mut().await?;
        }
        executor.child.reached().await?;
        executor.child.release.add_permits(1);
        executor.root.release.add_permits(1);
        let done = tokio::time::timeout(Duration::from_secs(30), driving).await???;
        assert_eq!(
            done.run.as_ref().map(|run| run.status),
            Some(RunStatus::Completed)
        );
        assert_eq!(
            done.agents[&child.assigned_ids["agent_id"]]
                .turn
                .as_ref()
                .ok_or("child")?
                .status,
            bitrouter_orchestrator::core::session::AgentStatus::Completed
        );
        assert_eq!(
            done.steering.contains_key("steer"),
            disposition == "accepted"
        );
    }
    Ok(())
}

#[tokio::test]
async fn steering_fences_approval_start_across_lost_ack_and_harness_restart() -> TestResult {
    for start_first in [false, true] {
        for committed in [false, true] {
            let mut fixture = Harness::new(None, None);
            fixture.wait_for_approval = true;
            let harness = Arc::new(fixture);
            let port = Arc::new(super::reconnect::FaultPort::new(
                harness.clone(),
                "input.steer.received",
                committed,
            ));
            let (session, _, _) = setup(vec![output(vec![call("old-call")])], port, false).await?;
            session.start("input", 1, input()).await?;
            session.drive().await?;
            let command = harness.sent.lock().await[0].clone();
            let identity = ToolStartFence {
                invocation_id: command.invocation_id.clone(),
                attempt_id: command.attempt_id.clone(),
            };
            if start_first {
                assert!(harness.store.lock().await.try_start_tool(identity.clone()));
            }
            assert!(steer(&session, "steer", "new instruction").await.is_err());
            assert_eq!(
                harness
                    .store
                    .lock()
                    .await
                    .tool_start_fences
                    .contains(&identity),
                committed
            );
            super::reconnect::reconnect(&session, &harness).await?;
            {
                let mut store = harness.store.lock().await;
                assert!(store.tool_start_fences.contains(&identity));
                assert!(!store.try_start_tool(identity.clone()));
                assert_eq!(store.started_tools.contains(&identity), start_first);
            }
            // An ACK proves start revocation, not whether the tool had started.
            assert_eq!(
                session.drive().await?.steering["steer"].disposition,
                SteeringDisposition::Received
            );
            session.disconnect().await;
            let replacement = super::recovery::harness_at(harness.store.lock().await.clone()).await;
            assert!(
                !replacement
                    .store
                    .lock()
                    .await
                    .try_start_tool(identity.clone())
            );
            let mut restore_input =
                super::recovery::request(&*replacement.store.lock().await, false)?;
            let mut outcome = result(&command);
            if !start_first {
                outcome.status = ToolOutcome::NotExecuted;
                outcome.output.clear();
            }
            restore_input.tools.push(ToolObservation {
                invocation_id: command.invocation_id.clone(),
                attempt_id: command.attempt_id.clone(),
                status: ToolStatus::Stopped,
                evidence: Vec::new(),
            });
            restore_input.results.push(outcome.clone());
            let (restored, executor) = super::recovery::restore(
                restore_input,
                replacement.clone(),
                vec![output(vec![text("revised answer")])],
            )
            .await?;
            let done = restored.drive().await?;
            assert_eq!(
                done.run.as_ref().map(|run| run.status),
                Some(RunStatus::Completed)
            );
            assert_eq!(
                done.steering["steer"].disposition,
                SteeringDisposition::Applied
            );
            assert_eq!(
                done.root_turn().ok_or("turn")?.invocations[0]
                    .result
                    .as_ref(),
                Some(&outcome)
            );
            assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
            assert!(replacement.cancelled.lock().await.is_empty());
            assert!(!replacement.store.lock().await.try_start_tool(identity));
        }
    }
    Ok(())
}

#[tokio::test]
async fn steering_revokes_restored_waiting_approval_and_late_execute() -> TestResult {
    let mut fixture = Harness::new(None, None);
    fixture.wait_for_approval = true;
    let harness = Arc::new(fixture);
    let (session, _, _) =
        setup(vec![output(vec![call("old-call")])], harness.clone(), false).await?;
    session.start("input", 1, input()).await?;
    session.drive().await?;
    let command = harness.sent.lock().await[0].clone();
    session.disconnect().await;
    let replacement = super::recovery::harness_at(harness.store.lock().await.clone()).await;
    let mut request = super::recovery::request(&*replacement.store.lock().await, false)?;
    request.tools.push(ToolObservation {
        invocation_id: command.invocation_id.clone(),
        attempt_id: command.attempt_id.clone(),
        status: ToolStatus::WaitingApproval,
        evidence: Vec::new(),
    });
    let (restored, executor) = super::recovery::restore(
        request,
        replacement.clone(),
        vec![output(vec![text("done")])],
    )
    .await?;
    steer(&restored, "steer", "revised instruction").await?;
    let identity = ToolStartFence {
        invocation_id: command.invocation_id.clone(),
        attempt_id: command.attempt_id.clone(),
    };
    assert!(
        !replacement
            .store
            .lock()
            .await
            .try_start_tool(identity.clone())
    );
    assert_eq!(
        restored.drive().await?.steering["steer"].disposition,
        SteeringDisposition::Received
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    let mut late_command = command.clone();
    late_command.execution_epoch = replacement.store.lock().await.grant.execution_epoch;
    assert!(
        replacement
            .send(ServerMessage::ToolExecute(late_command))
            .await
            .is_err()
    );
    assert!(
        !replacement
            .store
            .lock()
            .await
            .started_tools
            .contains(&identity)
    );
    let mut outcome = result(&command);
    outcome.status = ToolOutcome::NotExecuted;
    outcome.output.clear();
    restored.tool_result("cancelled-approval", outcome).await?;
    let done = restored.drive().await?;
    assert_eq!(
        done.steering["steer"].disposition,
        SteeringDisposition::Applied
    );
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    Ok(())
}

async fn steer(session: &CoreSession, operation: &str, text: &str) -> TestResult {
    let state = session.snapshot().await;
    let turn = state.root_turn().ok_or("missing turn")?;
    session
        .steer(
            operation,
            session.head().await.state_revision,
            &turn.run_id,
            &turn.agent_turn_id,
            text.into(),
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn steering_receipt_and_application_have_separate_ack_barriers() -> TestResult {
    for kind in ["input.steer.received", "input.steer.applied"] {
        let harness = Arc::new(Harness::new(None, Some(kind)));
        let (session, executor, _) =
            setup(vec![output(vec![text("done")])], harness.clone(), false).await?;
        let accepted = session.start("input", 1, input()).await?;
        let revision = session.head().await.state_revision;
        let run_id = accepted.assigned_ids["run_id"].clone();
        let turn_id = accepted.assigned_ids["agent_turn_id"].clone();
        let receiving = tokio::spawn({
            let session = session.clone();
            async move {
                session
                    .steer(
                        "steer",
                        revision,
                        &run_id,
                        &turn_id,
                        "new constraint".into(),
                    )
                    .await
            }
        });
        if kind == "input.steer.received" {
            tokio::time::timeout(Duration::from_secs(30), harness.seen.acquire())
                .await??
                .forget();
            assert!(session.snapshot().await.steering.is_empty());
            assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
            harness.resume.add_permits(1);
        }
        let receipt = receiving.await??;
        assert_eq!(
            receipt.disposition,
            bitrouter_orchestrator::core::protocol::OperationDisposition::Accepted
        );
        let driving = tokio::spawn({
            let session = session.clone();
            async move { session.drive().await }
        });
        if kind == "input.steer.applied" {
            tokio::time::timeout(Duration::from_secs(30), harness.seen.acquire())
                .await??
                .forget();
            let state = session.snapshot().await;
            assert_eq!(
                state.steering["steer"].disposition,
                SteeringDisposition::Received
            );
            assert!(
                !serde_json::to_string(&state.agents[&state.agent_id].history)?
                    .contains("new constraint")
            );
            assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
            harness.resume.add_permits(1);
        }
        let done = tokio::time::timeout(Duration::from_secs(30), driving).await???;
        assert_eq!(
            done.run.as_ref().map(|run| run.status),
            Some(RunStatus::Completed)
        );
        assert_eq!(
            done.steering["steer"].disposition,
            SteeringDisposition::Applied
        );
        assert!(done.steering["steer"].resolved_state_revision > Some(receipt.state_revision));
        assert!(
            serde_json::to_string(&executor.prompts.lock().await[0])?.contains("new constraint")
        );
        assert_eq!(session.operation("steer").await, Some(receipt.clone()));
        assert_eq!(
            session
                .steer(
                    "steer",
                    revision,
                    &accepted.assigned_ids["run_id"],
                    &accepted.assigned_ids["agent_turn_id"],
                    "new constraint".into()
                )
                .await?,
            receipt
        );
    }
    Ok(())
}

struct HeldExecutor {
    mock: MockExecutor,
    seen: Semaphore,
    resume: Semaphore,
    calls: AtomicUsize,
    prompts: Mutex<Vec<Prompt>>,
}

#[async_trait]
impl Executor for HeldExecutor {
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> bitrouter_sdk::Result<ExecutionResult> {
        self.prompts.lock().await.push(prompt.clone());
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.seen.add_permits(1);
            self.resume
                .acquire()
                .await
                .map_err(|_| bitrouter_sdk::BitrouterError::internal("fixture stopped"))?
                .forget();
        }
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
async fn steering_preserves_inflight_snapshot_and_supersedes_dependent_calls() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let executor = Arc::new(HeldExecutor {
        mock: MockExecutor::new(vec![
            output(vec![call("obsolete")]),
            output(vec![text("revised answer")]),
        ]),
        seen: Semaphore::new(0),
        resume: Semaphore::new(0),
        calls: AtomicUsize::new(0),
        prompts: Mutex::new(Vec::new()),
    });
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first")]);
    let settlements = Arc::new(AtomicUsize::new(0));
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone())
                .settlement_recorder(Recorder(settlements.clone()));
        })
        .build()?;
    let session = bind_app(Arc::new(app), harness.clone()).await?;
    session.start("input", 1, input()).await?;
    let driving = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(30), executor.seen.acquire())
        .await??
        .forget();
    let before = session.snapshot().await;
    steer(
        &session,
        "steer",
        "do not read; answer with the new constraint",
    )
    .await?;
    let received = session.snapshot().await;
    assert_eq!(
        serde_json::to_value(&before.root_turn().ok_or("turn")?.steps)?,
        serde_json::to_value(&received.root_turn().ok_or("turn")?.steps)?
    );
    assert_eq!(
        received.steering["steer"].disposition,
        SteeringDisposition::Received
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    executor.resume.add_permits(1);
    let done = tokio::time::timeout(Duration::from_secs(30), driving).await???;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    let turn = done.root_turn().ok_or("turn")?;
    assert!(turn.steps[0].interrupted && turn.steps[0].settled);
    assert!(
        turn.steps[0].attempts[0]
            .receipt
            .as_ref()
            .is_some_and(|receipt| receipt.report.result.is_some())
    );
    assert!(turn.invocations.is_empty());
    assert!(harness.sent.lock().await.is_empty());
    assert_eq!(settlements.load(Ordering::SeqCst), 2);
    let prompts = executor.prompts.lock().await;
    assert!(!serde_json::to_string(&prompts[0])?.contains("new constraint"));
    assert!(serde_json::to_string(&prompts[1])?.contains("new constraint"));
    drop(prompts);
    // A crash after the original complete report must not apply its old calls
    // when the steering receipt was already durable.
    let store = super::recovery::prefix(&*harness.store.lock().await, "model.attempt.outcome")?;
    let replacement = super::recovery::harness_at(store).await;
    let request = super::recovery::request(&*replacement.store.lock().await, false)?;
    let (restored, replay) = super::recovery::restore(
        request,
        replacement.clone(),
        vec![output(vec![text("restored answer")])],
    )
    .await?;
    let restored = restored.drive().await?;
    assert_eq!(
        restored.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(replay.calls.load(Ordering::SeqCst), 1);
    assert!(replacement.sent.lock().await.is_empty());
    assert!(restored.root_turn().ok_or("turn")?.steps[0].interrupted);
    Ok(())
}

#[tokio::test]
async fn steering_waits_for_running_tools_and_retains_call_result_pairing() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(
        vec![output(vec![call("read")]), output(vec![text("new answer")])],
        harness.clone(),
        false,
    )
    .await?;
    session.start("input", 1, input()).await?;
    session.drive().await?;
    steer(&session, "steer", "use the settled evidence differently").await?;
    let waiting = session.drive().await?;
    assert_eq!(
        waiting.steering["steer"].disposition,
        SteeringDisposition::Received
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert!(harness.cancelled.lock().await.is_empty());
    let command = harness.sent.lock().await[0].clone();
    session.tool_result("result", result(&command)).await?;
    let done = session.drive().await?;
    assert_eq!(
        done.steering["steer"].disposition,
        SteeringDisposition::Applied
    );
    assert!(done.root_turn().ok_or("turn")?.invocations[0].consumed);
    assert_eq!(harness.sent.lock().await.len(), 1);
    let prompt = serde_json::to_string(&executor.prompts.lock().await[1])?;
    assert!(prompt.contains("tool_result") && prompt.contains("settled evidence"));
    Ok(())
}

#[tokio::test]
async fn pending_steering_survives_restore_in_order_and_cancellation_wins() -> TestResult {
    for cancel in [false, true] {
        let harness = Arc::new(Harness::new(None, None));
        let (session, _, _) = setup(Vec::new(), harness.clone(), false).await?;
        let accepted = session.start("input", 1, input()).await?;
        steer(&session, "z-first", "first steering input").await?;
        steer(&session, "a-second", "second steering input").await?;
        if cancel {
            session
                .cancel_run(
                    "cancel",
                    session.head().await.state_revision,
                    &accepted.assigned_ids["run_id"],
                )
                .await?;
        }
        session.disconnect().await;
        let replacement = super::recovery::harness_at(harness.store.lock().await.clone()).await;
        let request = super::recovery::request(&*replacement.store.lock().await, false)?;
        let (restored, executor) =
            super::recovery::restore(request, replacement, vec![output(vec![text("done")])])
                .await?;
        let done = restored.drive().await?;
        let disposition = if cancel {
            SteeringDisposition::Cancelled
        } else {
            SteeringDisposition::Applied
        };
        assert_eq!(done.steering["z-first"].disposition, disposition);
        assert_eq!(done.steering["a-second"].disposition, disposition);
        assert_eq!(executor.calls.load(Ordering::SeqCst), usize::from(!cancel));
        if !cancel {
            let prompt = serde_json::to_string(&executor.prompts.lock().await[0].messages)?;
            assert!(
                prompt.find("first steering input").ok_or("first missing")?
                    < prompt
                        .find("second steering input")
                        .ok_or("second missing")?
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn steering_rejects_stale_conflicting_unknown_and_oversize_inputs() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(Vec::new(), harness, false).await?;
    let accepted = session.start("input", 1, input()).await?;
    let run = &accepted.assigned_ids["run_id"];
    let turn = &accepted.assigned_ids["agent_turn_id"];
    let revision = session.head().await.state_revision;
    let receipt = session
        .steer("steer", revision, run, turn, "revision".into())
        .await?;
    for (op, rev, target_run, target_turn, content, expected) in [
        (
            "steer",
            revision,
            run.as_str(),
            turn.as_str(),
            "different".into(),
            ErrorCode::OperationConflict,
        ),
        (
            "stale",
            revision,
            run.as_str(),
            turn.as_str(),
            "revision".into(),
            ErrorCode::StaleRevision,
        ),
        (
            "run",
            receipt.state_revision,
            "wrong",
            turn.as_str(),
            "revision".into(),
            ErrorCode::UnauthorizedScope,
        ),
        (
            "turn",
            receipt.state_revision,
            run.as_str(),
            "wrong",
            "revision".into(),
            ErrorCode::UnauthorizedScope,
        ),
        (
            "large",
            receipt.state_revision,
            run.as_str(),
            turn.as_str(),
            "x".repeat(65536),
            ErrorCode::LimitExceeded,
        ),
    ] {
        assert_eq!(
            session
                .steer(op, rev, target_run, target_turn, content)
                .await
                .err()
                .map(|error| error.code),
            Some(expected)
        );
        assert_eq!(session.head().await.state_revision, receipt.state_revision);
    }
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn steering_reconciles_lost_receipt_and_application_acks_without_duplicate_input()
-> TestResult {
    for kind in ["input.steer.received", "input.steer.applied"] {
        for committed in [false, true] {
            let harness = Arc::new(Harness::new(None, None));
            let port = Arc::new(super::reconnect::FaultPort::new(
                harness.clone(),
                kind,
                committed,
            ));
            let table = StaticRoutingTable::new();
            table.insert("fixture-model", vec![target("first")]);
            let executor = Arc::new(RecordingExecutor {
                mock: MockExecutor::new(vec![output(vec![text("done")])]),
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
            let session = bind_app(Arc::new(app), port).await?;
            let accepted = session.start("input", 1, input()).await?;
            let revision = session.head().await.state_revision;
            let received = session
                .steer(
                    "steer",
                    revision,
                    &accepted.assigned_ids["run_id"],
                    &accepted.assigned_ids["agent_turn_id"],
                    "unique steering text".into(),
                )
                .await;
            let lost = if kind == "input.steer.received" {
                received.err().ok_or("receipt ACK was not lost")?
            } else {
                received?;
                session
                    .drive()
                    .await
                    .err()
                    .ok_or("application ACK was not lost")?
            };
            assert_eq!(lost.commit_status, CommitStatus::Unknown);
            assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
            super::reconnect::reconnect(&session, &harness).await?;
            let done = session.drive().await?;
            assert_eq!(
                done.steering["steer"].disposition,
                SteeringDisposition::Applied
            );
            assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
            let history = serde_json::to_string(&done.agents[&done.agent_id].history)?;
            assert_eq!(history.matches("unique steering text").count(), 1);
            let receipt = session
                .steer(
                    "steer",
                    revision,
                    &accepted.assigned_ids["run_id"],
                    &accepted.assigned_ids["agent_turn_id"],
                    "unique steering text".into(),
                )
                .await?;
            assert_eq!(receipt.state_revision, revision + 1);
        }
    }
    Ok(())
}

#[tokio::test]
async fn steering_revokes_committed_unsent_workspace_and_collaboration_calls() -> TestResult {
    let harness = Arc::new(Harness::new(None, Some("model.output.applied")));
    let (session, executor, _) = setup(
        vec![
            output(vec![
                call("old-read"),
                core_call(
                    "spawn_agent",
                    json!({"task":work("obsolete child")}),
                    "old-spawn",
                ),
            ]),
            output(vec![text("revised answer")]),
        ],
        harness.clone(),
        false,
    )
    .await?;
    let accepted = session.start("input", 1, input()).await?;
    let driving = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(30), harness.seen.acquire())
        .await??
        .forget();
    let receiving = session.steer(
        "steer",
        session.head().await.state_revision + 1,
        &accepted.assigned_ids["run_id"],
        &accepted.assigned_ids["agent_turn_id"],
        "avoid obsolete actions".into(),
    );
    tokio::pin!(receiving);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut receiving)
            .await
            .is_err()
    );
    harness.hold_enabled.store(false, Ordering::SeqCst);
    harness.resume.add_permits(1);
    receiving.await?;
    let done = tokio::time::timeout(Duration::from_secs(30), driving).await???;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(done.agents.len(), 1);
    assert!(harness.sent.lock().await.is_empty());
    let turn = done.root_turn().ok_or("turn")?;
    assert_eq!(
        turn.invocations[0]
            .result
            .as_ref()
            .map(|result| result.status),
        Some(ToolOutcome::NotExecuted)
    );
    assert!(turn.invocations[0].consumed && turn.core_calls[0].consumed);
    assert_eq!(
        turn.core_calls[0].result.as_ref().map(|value| &value["ok"]),
        Some(&json!(false))
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn steering_bound_is_per_target_and_child_input_does_not_modify_root_context() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(
        (0..8).map(|_| output(vec![text("done")])).collect(),
        harness.clone(),
        false,
    )
    .await?;
    let mut task = input();
    task.limits = Some(Limits {
        mailbox_messages: 1,
        ..Limits::default()
    });
    let accepted = session.start("input", 1, task).await?;
    let child = session
        .collaborate(
            "spawn",
            session.head().await.state_revision,
            &accepted.assigned_ids["agent_id"],
            Action::Spawn {
                task: work("child"),
            },
        )
        .await?;
    let child_id = &child.assigned_ids["agent_id"];
    let before = session.snapshot().await;
    let turn = before.agents[child_id].turn.as_ref().ok_or("child turn")?;
    session
        .steer(
            "steer",
            session.head().await.state_revision,
            &turn.run_id,
            &turn.agent_turn_id,
            "child-only instruction".into(),
        )
        .await?;
    let full = session
        .steer(
            "overflow",
            session.head().await.state_revision,
            &turn.run_id,
            &turn.agent_turn_id,
            "overflow".into(),
        )
        .await;
    assert_eq!(
        full.err().map(|error| error.code),
        Some(ErrorCode::LimitExceeded)
    );
    assert_eq!(
        serde_json::to_value(&session.snapshot().await.agents[&before.agent_id])?,
        serde_json::to_value(&before.agents[&before.agent_id])?
    );
    let done = tokio::time::timeout(Duration::from_secs(30), session.drive()).await??;
    assert_eq!(
        done.steering["steer"].disposition,
        SteeringDisposition::Applied
    );
    let prompts = executor.prompts.lock().await;
    assert!(prompts.iter().any(|prompt| {
        serde_json::to_string(prompt).is_ok_and(|prompt| prompt.contains("child-only instruction"))
    }));
    let received = harness
        .store
        .lock()
        .await
        .batches
        .iter()
        .map(|batch| batch.decode(&Limits::default()))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flat_map(|payload| payload.events)
        .find(|event| event.kind == "input.steer.received")
        .ok_or("receipt event")?;
    assert_eq!(received.agent_id.as_deref(), Some(child_id.as_str()));
    Ok(())
}

#[tokio::test]
async fn steering_restore_rejects_rewritten_input_and_disposition_before_commit() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(Vec::new(), harness.clone(), false).await?;
    session.start("input", 1, input()).await?;
    steer(&session, "steer", "original steering text").await?;
    session.disconnect().await;
    for case in ["text", "receipt", "target", "resolution"] {
        let replacement = super::recovery::harness_at(harness.store.lock().await.clone()).await;
        let before = replacement.store.lock().await.batches.len();
        let mut request = super::recovery::request(&*replacement.store.lock().await, false)?;
        let mut payload = request
            .binding
            .checkpoint
            .as_ref()
            .ok_or("checkpoint")?
            .decode(&Limits::default())?;
        let state = &mut payload.checkpoint.state;
        match case {
            "text" => state["steering"]["steer"]["text"] = json!("forged text"),
            "receipt" => state["operations"]["steer"]["assigned_ids"]["run_id"] = json!("wrong"),
            "target" => state["steering"]["steer"]["agent_turn_id"] = json!("wrong"),
            _ => state["steering"]["steer"]["resolved_state_revision"] = json!(1),
        }
        let batch = CheckpointBatch::encode(&payload, &Limits::default())?;
        request.binding.durable_head.payload_sha256 = Some(batch.payload_sha256.clone());
        request.binding.checkpoint = Some(batch);
        let error = super::recovery::restore(request, replacement.clone(), Vec::new())
            .await
            .err()
            .ok_or("forgery restored")?;
        assert_eq!(
            error.downcast_ref::<CoreError>().map(|error| error.code),
            Some(ErrorCode::CheckpointConflict)
        );
        assert_eq!(replacement.store.lock().await.batches.len(), before);
    }
    Ok(())
}
