//! Oversized complete output retains billing evidence without durable content.

use super::*;
use bitrouter_sdk::language_model::types::UsageOrigin;

#[derive(Clone, Default)]
struct UsageRecords(Arc<Mutex<Vec<(u64, u64, bool)>>>);

#[async_trait]
impl SettlementRecorder for UsageRecords {
    async fn record(&self, ctx: &mut SettlementContext) -> bitrouter_sdk::Result<()> {
        self.0.lock().await.push((
            ctx.prompt_tokens,
            ctx.completion_tokens,
            ctx.raw_usage.is_some(),
        ));
        Ok(())
    }
}

async fn fixture(
    port: Arc<dyn HarnessPort>,
) -> Result<(CoreSession, Arc<RecordingExecutor>, UsageRecords), Box<dyn std::error::Error>> {
    let executor = Arc::new(RecordingExecutor {
        mock: MockExecutor::new(vec![MockResponse::Generate(GenerateResult {
            // Escaping alone pushes the wire size beyond the root's 1 MiB
            // bound, although the text itself occupies less than that.
            content: vec![text(&"\0".repeat(200_000)), call("must-not-run")],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                origin: UsageOrigin::ProviderReported,
                raw: Some(Box::new(json!({"billing_extension":"kept by SDK"}))),
                ..Default::default()
            }),
            finish_reason: Some(FinishReason::ToolCalls),
            response_id: None,
            stop_details: None,
            provider_metadata: Default::default(),
        })]),
        agent_once: Mutex::new(Default::default()),
        prompts: Mutex::new(Vec::new()),
        calls: AtomicUsize::new(0),
    });
    let records = UsageRecords::default();
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first"), target("fallback")]);
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone())
                .native_cost_estimator(Arc::new(accounting::FixtureCost))
                .settlement_recorder(records.clone());
        })
        .build()?;
    let session = bind_app(Arc::new(app), port).await?;
    let mut task = input();
    task.limits = Some(Limits {
        checkpoint_bytes: 1024 * 1024,
        ..Limits::default()
    });
    session.start("input", 1, task).await?;
    Ok((session, executor, records))
}

fn retained(state: &SessionSnapshot) -> TestResult {
    let attempt = &state.root_turn().ok_or("turn")?.steps[0].attempts[0];
    let receipt = attempt.receipt.as_ref().ok_or("receipt")?;
    assert!(receipt.report.result.is_none());
    assert_eq!(receipt.cost_micro_usd, Some(7));
    assert_eq!(receipt.usage_origin, Some(UsageOrigin::ProviderReported));
    let rejected = receipt
        .report
        .output_rejection
        .as_ref()
        .ok_or("rejection")?;
    assert_eq!(Some(rejected.byte_limit), attempt.canonical_output_bytes);
    assert!(rejected.byte_limit > 0 && rejected.byte_limit < 1024 * 1024);
    assert_eq!(
        rejected
            .usage
            .as_ref()
            .map(|usage| (usage.prompt_tokens, usage.completion_tokens)),
        Some((10, 5))
    );
    let encoded = serde_json::to_string(receipt)?;
    assert!(!encoded.contains("must-not-run"));
    assert!(!encoded.contains("billing_extension"));
    Ok(())
}

#[tokio::test]
async fn canonical_output_rejection_keeps_usage_and_restores_without_tools() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, records) = fixture(harness.clone()).await?;
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Failed)
    );
    retained(&done)?;
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(*records.0.lock().await, vec![(10, 5, true)]);
    assert!(harness.sent.lock().await.is_empty());
    session.disconnect().await;
    let restored_harness = recovery::harness_at(harness.store.lock().await.clone()).await;
    let request = recovery::request(&*restored_harness.store.lock().await, false)?;
    let (restored, restored_executor) =
        recovery::restore(request, restored_harness.clone(), Vec::new()).await?;
    retained(&restored.snapshot().await)?;
    assert_eq!(
        restored.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Failed)
    );
    assert_eq!(restored_executor.calls.load(Ordering::SeqCst), 0);
    assert!(restored_harness.sent.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn canonical_output_rejection_ack_loss_does_not_retry_or_settle_again() -> TestResult {
    for committed in [false, true] {
        let harness = Arc::new(Harness::new(None, None));
        let port = Arc::new(reconnect::FaultPort::new(
            harness.clone(),
            "model.attempt.outcome",
            committed,
        ));
        let (session, executor, records) = fixture(port).await?;
        assert!(session.drive().await.is_err());
        reconnect::reconnect(&session, &harness).await?;
        let done = session.drive().await?;
        assert_eq!(
            done.run.as_ref().map(|run| run.status),
            Some(RunStatus::Failed)
        );
        retained(&done)?;
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(*records.0.lock().await, vec![(10, 5, true)]);
        assert!(harness.sent.lock().await.is_empty());
    }
    Ok(())
}

struct ConcurrentOutput {
    seen: Semaphore,
    resume: Semaphore,
    bytes: AtomicUsize,
    calls: AtomicUsize,
}

#[async_trait]
impl Executor for ConcurrentOutput {
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
        let MockResponse::Generate(mut result) = output(vec![text("")]) else {
            return Err(bitrouter_sdk::BitrouterError::internal("fixture result"));
        };
        let used = serde_json::to_vec(&result)
            .map_err(|error| bitrouter_sdk::BitrouterError::internal(error.to_string()))?
            .len();
        let padding = self
            .bytes
            .load(Ordering::SeqCst)
            .checked_sub(used)
            .ok_or_else(|| {
                bitrouter_sdk::BitrouterError::internal("fixture output bound is too small")
            })?;
        result.content = vec![text(&"x".repeat(padding))];
        MockExecutor::new(vec![MockResponse::Generate(result)])
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

#[derive(Clone)]
struct HeldReports {
    seen: Arc<Semaphore>,
    resume: Arc<Semaphore>,
}

#[async_trait]
impl ObserveHook for HeldReports {
    async fn after_phase(&self, _: Phase, _: &PipelineContext) {}
    async fn on_stream_part(&self, _: &StreamContext, _: &StreamPart) {}
    async fn on_request_end(&self, _: &PipelineContext, _: &RequestOutcome) {}
    async fn on_hop_end(
        &self,
        _: &PipelineContext,
        _: &RoutingTarget,
        _: bitrouter_sdk::language_model::hooks::HopOutcome<'_>,
    ) {
        self.seen.add_permits(1);
        if let Ok(permit) = self.resume.acquire().await {
            permit.forget();
        }
    }
}

#[tokio::test]
async fn concurrent_canonical_results_fill_their_frozen_allowances_and_settle() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let executor = Arc::new(ConcurrentOutput {
        seen: Semaphore::new(0),
        resume: Semaphore::new(0),
        bytes: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
    });
    let reports = HeldReports {
        seen: Arc::new(Semaphore::new(0)),
        resume: Arc::new(Semaphore::new(0)),
    };
    let records = UsageRecords::default();
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first")]);
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone())
                .observe_hook(reports.clone())
                .settlement_recorder(records.clone());
        })
        .build()?;
    let session = bind_app(Arc::new(app), harness.clone()).await?;
    let mut task = input();
    task.limits = Some(Limits {
        checkpoint_bytes: 512 * 1024,
        active_models: 2,
        ..Limits::default()
    });
    let accepted = session.start("input", 1, task).await?;
    session
        .collaborate(
            "spawn",
            session.head().await.state_revision,
            &accepted.assigned_ids["agent_id"],
            Action::Spawn {
                task: work("parallel result"),
            },
        )
        .await?;
    let driver = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(30), executor.seen.acquire_many(2))
        .await??
        .forget();
    let state = session.snapshot().await;
    let limits = state
        .agents
        .values()
        .filter_map(|agent| agent.turn.as_ref())
        .flat_map(|turn| &turn.steps)
        .flat_map(|step| &step.attempts)
        .map(|attempt| {
            attempt
                .canonical_output_bytes
                .ok_or("missing frozen output allowance")
        })
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(limits.len(), 2);
    assert_eq!(limits[0], limits[1]);
    assert!(limits[0] > 64 * 1024);
    executor
        .bytes
        .store(usize::try_from(limits[0])?, Ordering::SeqCst);
    let mut signal = signal_update(&session, Vec::new()).await;
    signal
        .facts
        .insert("concurrent_fact".into(), json!("f".repeat(32 * 1024)));
    session.signals("signal", signal).await?;
    executor.resume.add_permits(2);
    tokio::time::timeout(Duration::from_secs(30), reports.seen.acquire_many(2))
        .await??
        .forget();
    let retained = session.snapshot().await;
    for attempt in retained
        .agents
        .values()
        .filter_map(|agent| agent.turn.as_ref())
        .flat_map(|turn| &turn.steps)
        .flat_map(|step| &step.attempts)
    {
        let receipt = attempt
            .receipt
            .as_ref()
            .ok_or("result was not durably admitted")?;
        assert!(receipt.report.output_rejection.is_none());
        assert_eq!(
            serde_json::to_vec(receipt.report.result.as_ref().ok_or("result")?)?.len() as u64,
            attempt.canonical_output_bytes.ok_or("allowance")?
        );
    }
    assert!(retained.run.as_ref().ok_or("run")?.resource_error.is_none());
    // Cancellation after both committed outputs tests settlement/retention;
    // applying these outputs to future history has a separate admission gate.
    session
        .cancel_run(
            "cancel",
            session.head().await.state_revision,
            &accepted.assigned_ids["run_id"],
        )
        .await?;
    reports.resume.add_permits(2);
    let done = tokio::time::timeout(Duration::from_secs(30), driver).await???;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Cancelled)
    );
    assert_eq!(*records.0.lock().await, vec![(7, 3, false), (7, 3, false)]);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    session
        .release("release", session.head().await.state_revision)
        .await?;
    Ok(())
}

#[tokio::test]
async fn canonical_output_reservation_rejects_forgery_and_preserves_legacy() -> TestResult {
    let original = recovery::completed_store().await?;
    for forged in [false, true] {
        let mut store = original.clone();
        tool_payloads::rewrite_last(&mut store, |payload| {
            let root = payload.checkpoint.state["agent_id"]
                .as_str()
                .map(str::to_owned);
            if let Some(root) = root {
                let steps = &mut payload.checkpoint.state["agents"][root]["turn"]["steps"];
                if let Some(steps) = steps.as_array_mut() {
                    for step in steps {
                        if let Some(attempts) = step["attempts"].as_array_mut() {
                            for attempt in attempts {
                                if forged {
                                    attempt["canonical_output_bytes"] = json!(1);
                                } else if let Some(attempt) = attempt.as_object_mut() {
                                    attempt.remove("canonical_output_bytes");
                                }
                            }
                        }
                    }
                }
            }
            if !forged {
                strip_ledger_allowances(&mut payload.checkpoint.state);
            }
        })?;
        let harness = recovery::harness_at(store).await;
        let head = harness.store.lock().await.head.clone();
        let request = recovery::request(&*harness.store.lock().await, false)?;
        let restored = recovery::restore(request, harness.clone(), Vec::new()).await;
        if forged {
            assert_eq!(
                restored
                    .err()
                    .ok_or("forged output allowance accepted")?
                    .downcast_ref::<CoreError>()
                    .map(|error| error.code),
                Some(ErrorCode::CheckpointConflict)
            );
            assert_eq!(harness.store.lock().await.head, head);
        } else {
            let (restored, executor) = restored?;
            let state = restored.snapshot().await;
            assert!(
                state
                    .root_turn()
                    .ok_or("turn")?
                    .steps
                    .iter()
                    .flat_map(|step| &step.attempts)
                    .all(|attempt| attempt.canonical_output_bytes.is_none())
            );
            assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        }
    }
    Ok(())
}

#[tokio::test]
async fn canonical_output_reservation_blocks_takeover_without_headroom() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![text("done")])], harness.clone(), false).await?;
    let mut task = input();
    task.limits = Some(Limits {
        checkpoint_bytes: 128 * 1024,
        input_bytes: 16 * 1024,
        active_models: 1,
        ..Limits::default()
    });
    session.start("input", 1, task).await?;
    session.drive().await?;
    session.disconnect().await;
    let original = recovery::prefix(&*harness.store.lock().await, "model.attempt.intent")?;
    let payload = original
        .batches
        .last()
        .ok_or("intent")?
        .decode(&Limits::default())?;
    let state: SessionSnapshot = serde_json::from_value(payload.checkpoint.state.clone())?;
    let root = state.agent_id;
    let allowance = state.agents[&root].turn.as_ref().ok_or("turn")?.steps[0].attempts[0]
        .canonical_output_bytes
        .ok_or("allowance")?;
    let padding = (128 * 1024usize)
        .checked_sub(serde_json::to_vec(&payload)?.len())
        .and_then(|bytes| bytes.checked_sub(usize::try_from(allowance / 2).ok()?))
        .ok_or("fixture has no padding capacity")?;
    for legacy in [false, true] {
        let mut store = original.clone();
        tool_payloads::rewrite_last(&mut store, |payload| {
            payload.checkpoint.state["signals"]["facts"]["retained_facts"] =
                json!("f".repeat(padding));
            if legacy {
                payload.checkpoint.state["agents"][&root]["turn"]["steps"][0]["attempts"][0]
                    .as_object_mut()
                    .map(|attempt| attempt.remove("canonical_output_bytes"));
                strip_ledger_allowances(&mut payload.checkpoint.state);
            }
        })?;
        let replacement = recovery::harness_at(store).await;
        let before = replacement.store.lock().await.head.clone();
        let request = recovery::request(&*replacement.store.lock().await, false)?;
        let restored = recovery::restore(request, replacement.clone(), Vec::new()).await;
        if legacy {
            let (restored, executor) = restored?;
            assert!(
                restored.snapshot().await.root_turn().ok_or("turn")?.steps[0].attempts[0]
                    .canonical_output_bytes
                    .is_none()
            );
            assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        } else {
            let error = restored
                .err()
                .ok_or("takeover consumed reserved result capacity")?;
            let error = error
                .downcast_ref::<CoreError>()
                .ok_or("typed capacity error")?;
            assert_eq!(error.code, ErrorCode::LimitExceeded);
            assert!(error.message.contains("cleanup capacity"));
            assert_eq!(replacement.store.lock().await.head, before);
        }
    }
    Ok(())
}

fn strip_ledger_allowances(state: &mut serde_json::Value) {
    if let Some(runs) = state["cost_work"].as_object_mut() {
        for run in runs.values_mut() {
            if let Some(work) = run["work"].as_object_mut() {
                for entry in work.values_mut() {
                    if let Some(source) = entry["provider_source"].as_object_mut() {
                        source.remove("canonical_output_bytes");
                    }
                }
            }
        }
    }
}
