//! Oversized complete output retains billing evidence without durable content.

use super::*;
use bitrouter_ai::types::UsageOrigin;

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
    assert_eq!(limits[0], (512 * 1024) / (16 * 3));
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
                                    attempt.remove("canonical_output_version");
                                    attempt.remove("attempt_report_bytes");
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
    session
        .start("input", 1, task)
        .await
        .map_err(|e| format!("fixture start: {e:?}"))?;
    session
        .drive()
        .await
        .map_err(|e| format!("fixture drive: {e:?}"))?;
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
        .attempt_report_bytes
        .ok_or("report allowance")?;
    // Keep a fixed legacy cleanup margin; the newer canonical policy is
    // smaller and is no longer a useful estimate of legacy cleanup overhead.
    let legacy_margin = 16 * 1024;
    assert!(allowance * 7 > legacy_margin as u64);
    let padding = (128 * 1024usize)
        .checked_sub(serde_json::to_vec(&payload)?.len())
        .and_then(|bytes| bytes.checked_sub(legacy_margin))
        .ok_or("fixture has no padding capacity")?;
    for legacy in [false, true] {
        let mut store = original.clone();
        tool_payloads::rewrite_last(&mut store, |payload| {
            payload.checkpoint.state["signals"]["facts"]["retained_facts"] =
                json!("f".repeat(padding));
            if legacy {
                if let Some(attempt) = payload.checkpoint.state["agents"][&root]["turn"]["steps"][0]
                    ["attempts"][0]
                    .as_object_mut()
                {
                    attempt.remove("canonical_output_bytes");
                    attempt.remove("canonical_output_version");
                    attempt.remove("attempt_report_bytes");
                }
                strip_ledger_allowances(&mut payload.checkpoint.state);
            }
        })?;
        let replacement = recovery::harness_at(store).await;
        let before = replacement.store.lock().await.head.clone();
        let request = recovery::request(&*replacement.store.lock().await, false)?;
        let restored = recovery::restore(request, replacement.clone(), Vec::new()).await;
        if legacy {
            let (restored, executor) = restored.map_err(|e| format!("legacy restore: {e:?}"))?;
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
                        source.remove("canonical_output_version");
                        source.remove("attempt_report_bytes");
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn full_canonical_allowance_reaches_response_history_and_terminal_answer() -> TestResult {
    for lost_ack in [None, Some(false), Some(true)] {
        let harness = Arc::new(Harness::new(None, None));
        let port: Arc<dyn HarnessPort> = match lost_ack {
            Some(committed) => Arc::new(reconnect::FaultPort::new(
                harness.clone(),
                "model.output.applied",
                committed,
            )),
            None => harness.clone(),
        };
        let executor = Arc::new(ConcurrentOutput {
            seen: Semaphore::new(0),
            resume: Semaphore::new(0),
            bytes: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
        });
        let records = UsageRecords::default();
        let table = StaticRoutingTable::new();
        table.insert("fixture-model", vec![target("first")]);
        let app = App::builder()
            .language_model(|builder| {
                builder
                    .routing_table(Arc::new(table))
                    .executor(executor.clone())
                    .settlement_recorder(records.clone());
            })
            .build()?;
        let session = bind_app(Arc::new(app), port).await?;
        let mut task = input();
        task.limits = Some(Limits {
            checkpoint_bytes: 256 * 1024,
            input_bytes: 16 * 1024,
            active_models: 1,
            ..Limits::default()
        });
        let accepted = session.start_response("input", 1, task).await?;
        let response_id = accepted.assigned_ids["response_id"].clone();
        let driver = tokio::spawn({
            let session = session.clone();
            let response_id = response_id.clone();
            async move { session.drive_response(&response_id).await }
        });
        tokio::time::timeout(Duration::from_secs(30), executor.seen.acquire())
            .await??
            .forget();
        let state = session.snapshot().await;
        let bound = state.root_turn().ok_or("turn")?.steps[0].attempts[0]
            .canonical_output_bytes
            .ok_or("bound")?;
        executor
            .bytes
            .store(usize::try_from(bound)?, Ordering::SeqCst);
        executor.resume.add_permits(1);
        let outcome = tokio::time::timeout(Duration::from_secs(30), driver).await??;
        let response = if lost_ack.is_some() {
            assert!(outcome.is_err());
            reconnect::reconnect(&session, &harness).await?;
            session.drive_response(&response_id).await?
        } else {
            outcome?
        };
        assert_eq!(response.run_status, Some(RunStatus::Completed));
        assert_eq!(response.output.len(), 1);
        let state = session.snapshot().await;
        let turn = state.root_turn().ok_or("turn")?;
        let result = turn.steps[0].attempts[0]
            .receipt
            .as_ref()
            .ok_or("receipt")?
            .report
            .result
            .as_ref()
            .ok_or("result")?;
        assert_eq!(serde_json::to_vec(result)?.len() as u64, bound);
        assert_eq!(response.final_answer, turn.final_answer);
        assert_eq!(
            response.final_answer,
            state.run.as_ref().ok_or("run")?.final_answer
        );
        assert_eq!(response.output[0].message.content, result.content);
        assert_eq!(
            state.agents[&state.agent_id]
                .history
                .last()
                .ok_or("history")?
                .content,
            result.content
        );
        assert_eq!(*records.0.lock().await, vec![(7, 3, false)]);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert!(harness.sent.lock().await.is_empty());
        assert_eq!(session.drive_response(&response_id).await?, response);
        let bounds = Limits {
            checkpoint_bytes: 256 * 1024,
            input_bytes: 16 * 1024,
            active_models: 1,
            ..Limits::default()
        };
        for batch in &harness.store.lock().await.batches {
            batch.decode(&bounds)?;
        }
        session.disconnect().await;
        let restored_harness = recovery::harness_at(harness.store.lock().await.clone()).await;
        let request = recovery::request(&*restored_harness.store.lock().await, false)?;
        let (restored, restored_executor) =
            recovery::restore(request, restored_harness, Vec::new()).await?;
        assert_eq!(restored.drive_response(&response_id).await?, response);
        assert_eq!(restored_executor.calls.load(Ordering::SeqCst), 0);
        restored
            .release("release", restored.head().await.state_revision)
            .await?;
    }
    Ok(())
}

#[tokio::test]
async fn canonical_delivery_policy_validates_versions_and_restores_older_contracts() -> TestResult {
    let original = recovery::completed_store().await?;
    for mode in [
        "v1",
        "v2",
        "unknown",
        "missing_bytes",
        "mismatched_inventory",
        "report_missing",
        "report_wrong_size",
    ] {
        let mut store = original.clone();
        tool_payloads::rewrite_last(&mut store, |payload| {
            let state = &mut payload.checkpoint.state;
            let legacy_bound = state["run"]["limits"]["checkpoint_bytes"]
                .as_u64()
                .zip(state["run"]["limits"]["active_models"].as_u64())
                .map(|(bytes, active)| bytes / (if mode == "v2" { 8 } else { 2 } * (active + 1)));
            if let Some(agents) = state["agents"].as_object_mut() {
                for agent in agents.values_mut() {
                    if let Some(steps) = agent["turn"]["steps"].as_array_mut() {
                        for step in steps {
                            if let Some(attempts) = step["attempts"].as_array_mut() {
                                for attempt in attempts {
                                    match mode {
                                        "v1" | "v2" => {
                                            attempt["attempt_report_bytes"] =
                                                serde_json::Value::Null;
                                            attempt["canonical_output_version"] = if mode == "v2" {
                                                json!(2)
                                            } else {
                                                serde_json::Value::Null
                                            };
                                            attempt["canonical_output_bytes"] = json!(legacy_bound);
                                        }
                                        "unknown" => {
                                            attempt["canonical_output_version"] = json!(99)
                                        }
                                        "report_missing" => {
                                            attempt["attempt_report_bytes"] =
                                                serde_json::Value::Null
                                        }
                                        "report_wrong_size" => {
                                            attempt["attempt_report_bytes"] = json!(1)
                                        }
                                        "missing_bytes" => {
                                            attempt["canonical_output_bytes"] =
                                                serde_json::Value::Null
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if let Some(runs) = state["cost_work"].as_object_mut() {
                for run in runs.values_mut() {
                    if let Some(work) = run["work"].as_object_mut() {
                        for item in work.values_mut() {
                            if let Some(source) = item["provider_source"].as_object_mut() {
                                match mode {
                                    "v1" | "v2" => {
                                        source.remove("canonical_output_version");
                                        source.remove("attempt_report_bytes");
                                        if mode == "v2" {
                                            source.insert(
                                                "canonical_output_version".into(),
                                                json!(2),
                                            );
                                        }
                                        source.insert(
                                            "canonical_output_bytes".into(),
                                            json!(legacy_bound),
                                        );
                                    }
                                    "mismatched_inventory" => {
                                        source.remove("canonical_output_version");
                                        source.remove("attempt_report_bytes");
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
        })?;
        let harness = recovery::harness_at(store).await;
        let before = harness.store.lock().await.head.clone();
        let request = recovery::request(&*harness.store.lock().await, false)?;
        let restored = recovery::restore(request, harness.clone(), Vec::new()).await;
        if mode == "v1" || mode == "v2" {
            let (session, executor) = restored?;
            assert!(
                session
                    .snapshot()
                    .await
                    .root_turn()
                    .ok_or("turn")?
                    .steps
                    .iter()
                    .flat_map(|step| &step.attempts)
                    .all(|attempt| attempt.canonical_output_version
                        == if mode == "v2" { Some(2) } else { None }
                        && attempt.canonical_output_bytes.is_some()
                        && attempt.attempt_report_bytes.is_none())
            );
            assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        } else {
            assert_eq!(
                restored
                    .err()
                    .ok_or("invalid policy accepted")?
                    .downcast_ref::<CoreError>()
                    .map(|error| error.code),
                Some(ErrorCode::CheckpointConflict)
            );
            assert_eq!(harness.store.lock().await.head, before);
        }
    }
    Ok(())
}

struct WaitDeliveryExecutor {
    child_id: Mutex<String>,
    child: ConcurrentOutput,
    root_calls: AtomicUsize,
}

#[async_trait]
impl Executor for WaitDeliveryExecutor {
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> bitrouter_sdk::Result<ExecutionResult> {
        let child = self.child_id.lock().await.clone();
        if prompt.system.as_deref().is_some_and(|system| {
            system.starts_with(&format!("You are agent {child} for this task."))
        }) {
            return self.child.execute(target, prompt, ctx).await;
        }
        let response = if self.root_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            output(vec![core_call(
                "wait_agent",
                json!({"agent_ids":[child],"timeout_ms":600_000}),
                "wait-for-child",
            )])
        } else {
            output(vec![text("root completed")])
        };
        MockExecutor::new(vec![response])
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
async fn full_child_output_reaches_model_wait_after_ack_loss_and_restore() -> TestResult {
    for lost_ack in [None, Some(false), Some(true)] {
        let harness = Arc::new(Harness::new(None, None));
        let fault = Arc::new(reconnect::FaultPort::new(
            harness.clone(),
            "collaboration.applied",
            lost_ack.unwrap_or(false),
        ));
        fault.set_enabled(false);
        let executor = Arc::new(WaitDeliveryExecutor {
            child_id: Mutex::new(String::new()),
            child: ConcurrentOutput {
                seen: Semaphore::new(0),
                resume: Semaphore::new(0),
                bytes: AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
            },
            root_calls: AtomicUsize::new(0),
        });
        let records = UsageRecords::default();
        let table = StaticRoutingTable::new();
        table.insert("fixture-model", vec![target("first")]);
        let app = App::builder()
            .language_model(|builder| {
                builder
                    .routing_table(Arc::new(table))
                    .executor(executor.clone())
                    .settlement_recorder(records.clone());
            })
            .build()?;
        let session = bind_app(Arc::new(app), fault.clone()).await?;
        let mut task = input();
        task.limits = Some(Limits {
            checkpoint_bytes: 4 * 1024 * 1024,
            ..Limits::default()
        });
        let accepted = session.start_response("input", 1, task).await?;
        let response_id = accepted.assigned_ids["response_id"].clone();
        let spawned = session
            .collaborate(
                "spawn",
                session.head().await.state_revision,
                &accepted.assigned_ids["agent_id"],
                Action::Spawn {
                    task: work("child result"),
                },
            )
            .await?;
        let child_id = spawned.assigned_ids["agent_id"].clone();
        *executor.child_id.lock().await = child_id.clone();
        let driver = tokio::spawn({
            let session = session.clone();
            let response_id = response_id.clone();
            async move { session.drive_response(&response_id).await }
        });
        tokio::time::timeout(Duration::from_secs(30), executor.child.seen.acquire())
            .await??
            .forget();
        let before = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let state = session.snapshot().await;
                if state.root_turn().is_some_and(|turn| {
                    turn.core_calls
                        .iter()
                        .any(|call| call.wait.is_some() && call.result.is_none())
                }) {
                    break state;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(
            before.root_turn().ok_or("root")?.core_calls[0].wait_output_version,
            Some(1)
        );
        let bound = before.agents[&child_id].turn.as_ref().ok_or("child")?.steps[0].attempts[0]
            .canonical_output_bytes
            .ok_or("bound")?;
        executor
            .child
            .bytes
            .store(usize::try_from(bound)?, Ordering::SeqCst);
        fault.set_enabled(lost_ack.is_some());
        executor.child.resume.add_permits(1);
        let outcome = tokio::time::timeout(Duration::from_secs(30), driver).await??;
        let response = if lost_ack.is_some() {
            assert!(outcome.is_err());
            reconnect::reconnect(&session, &harness).await?;
            session.drive_response(&response_id).await?
        } else {
            outcome?
        };
        assert_eq!(response.run_status, Some(RunStatus::Completed));
        let state = session.snapshot().await;
        let child = &state.agents[&child_id];
        let child_turn = child.turn.as_ref().ok_or("child turn")?;
        let output = child_turn.steps[0].attempts[0]
            .receipt
            .as_ref()
            .ok_or("receipt")?
            .report
            .result
            .as_ref()
            .ok_or("result")?;
        assert_eq!(serde_json::to_vec(output)?.len() as u64, bound);
        let root = state.root_turn().ok_or("root turn")?;
        let wait = root.core_calls[0].result.as_ref().ok_or("wait result")?;
        assert_eq!(wait["ok"], json!(true));
        assert_eq!(
            wait["value"]["agents"][0]["final_answer"],
            json!(child_turn.final_answer)
        );
        assert_eq!(
            wait["value"]["agents"][0]["context_sources"],
            json!(child.context_sources)
        );
        assert!(root.core_calls[0].consumed);
        let paired = state.agents[&state.agent_id]
            .history
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|content| match content {
                Content::ToolResult {
                    call_id,
                    output: bitrouter_ai::types::ToolResultOutput::Text { value },
                    ..
                } if call_id == "wait-for-child" => Some(value),
                _ => None,
            })
            .ok_or("paired model wait")?;
        assert_eq!(serde_json::from_str::<serde_json::Value>(paired)?, *wait);
        assert_eq!(
            response
                .events
                .iter()
                .filter(|event| event.event.kind == "collaboration.applied"
                    && event.event.payload["result"] == *wait)
                .count(),
            1
        );
        assert_eq!(executor.child.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            records.0.lock().await.len(),
            1 + executor.root_calls.load(Ordering::SeqCst)
        );
        assert!(harness.sent.lock().await.is_empty());
        let proposals = fault.proposals.lock().await;
        let results = proposals
            .iter()
            .filter_map(|batch| {
                batch
                    .decode(&Limits::default())
                    .ok()
                    .map(|payload| (batch, payload))
            })
            .filter(|(_, payload)| {
                payload.events.iter().any(|event| {
                    event.kind == "collaboration.applied" && event.payload["result"] == *wait
                })
            })
            .map(|(batch, _)| batch)
            .collect::<Vec<_>>();
        assert_eq!(results.len(), if lost_ack == Some(false) { 2 } else { 1 });
        if results.len() == 2 {
            assert_eq!(results[0], results[1]);
        }
        drop(proposals);
        session.disconnect().await;
        let store = harness.store.lock().await.clone();
        let replacement = recovery::harness_at(store.clone()).await;
        let request = recovery::request(&*replacement.store.lock().await, false)?;
        let (restored, restored_executor) =
            recovery::restore(request, replacement, Vec::new()).await?;
        assert_eq!(restored.drive_response(&response_id).await?, response);
        assert_eq!(restored_executor.calls.load(Ordering::SeqCst), 0);
        if lost_ack.is_none() {
            for mode in ["legacy", "unknown", "wrong_action", "changed_in_journal"] {
                let mut changed = store.clone();
                tool_payloads::rewrite_last(&mut changed, |payload| {
                    let call = &mut payload.checkpoint.state["agents"][&state.agent_id]["turn"]["core_calls"]
                        [0];
                    match mode {
                        "unknown" => call["wait_output_version"] = json!(2),
                        "wrong_action" => {
                            call["action"] = json!({"name":"list_agents","arguments":{}})
                        }
                        _ => {
                            if let Some(call) = call.as_object_mut() {
                                call.remove("wait_output_version");
                            }
                        }
                    }
                })?;
                let replacement = recovery::harness_at(changed).await;
                let head = replacement.store.lock().await.head.clone();
                let mut request = recovery::request(&*replacement.store.lock().await, false)?;
                if mode == "changed_in_journal" {
                    // Use a bounded journal suffix; the entire fixture's full
                    // snapshots exceed the restore control envelope together.
                    let store = replacement.store.lock().await;
                    request.binding.checkpoint =
                        store.batches.get(store.batches.len() - 2).cloned();
                    request.journal_tail = store.batches.last().cloned().into_iter().collect();
                }
                let restored = recovery::restore(request, replacement.clone(), Vec::new()).await;
                if mode == "legacy" {
                    let (restored, executor) = restored?;
                    assert_eq!(
                        restored
                            .snapshot()
                            .await
                            .root_turn()
                            .ok_or("legacy root")?
                            .core_calls[0]
                            .wait_output_version,
                        None
                    );
                    assert_eq!(restored.drive_response(&response_id).await?, response);
                    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
                } else {
                    assert_eq!(
                        restored
                            .err()
                            .ok_or("invalid wait policy accepted")?
                            .downcast_ref::<CoreError>()
                            .map(|error| error.code),
                        Some(ErrorCode::CheckpointConflict)
                    );
                    assert_eq!(replacement.store.lock().await.head, head);
                }
            }
        }
        restored
            .release("release", restored.head().await.state_revision)
            .await?;
    }
    Ok(())
}

#[tokio::test]
async fn oversized_attempt_error_commits_terminal_failure_without_fallback() -> TestResult {
    for fault in [None, Some(false), Some(true)] {
        let harness = Arc::new(Harness::new(None, None));
        let port: Arc<dyn HarnessPort> = match fault {
            None => harness.clone(),
            Some(committed) => Arc::new(reconnect::FaultPort::new(
                harness.clone(),
                "model.attempt.outcome",
                committed,
            )),
        };
        let (session, executor, _) = setup(
            vec![
                MockResponse::Error(bitrouter_sdk::BitrouterError::Upstream {
                    status: 500,
                    message: "\0\"error".repeat(100_000),
                }),
                output(vec![call("must-not-run")]),
            ],
            port,
            true,
        )
        .await?;
        let mut task = input();
        task.limits = Some(Limits {
            checkpoint_bytes: 256 * 1024,
            ..Limits::default()
        });
        session.start("input", 1, task).await?;
        if fault.is_some() {
            assert!(session.drive().await.is_err());
            reconnect::reconnect(&session, &harness).await?;
        }
        let done = session.drive().await?;
        assert_eq!(
            done.run.as_ref().map(|run| run.status),
            Some(RunStatus::Failed)
        );
        let attempt = &done.root_turn().ok_or("turn")?.steps[0].attempts[0];
        let receipt = attempt.receipt.as_ref().ok_or("receipt")?;
        let rejected = receipt
            .report
            .report_rejection
            .as_ref()
            .ok_or("rejection")?;
        assert_eq!(attempt.canonical_output_version, Some(3));
        assert_eq!(Some(rejected.byte_limit), attempt.attempt_report_bytes);
        assert!(rejected.original.bytes > rejected.byte_limit);
        assert!(serde_json::to_vec(&receipt.report)?.len() as u64 <= rejected.byte_limit);
        assert!(
            !rejected.had_result
                && rejected.actual_provider.is_none()
                && rejected.actual_model.is_none()
        );
        assert!(receipt.cost_micro_usd.is_none());
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert!(harness.sent.lock().await.is_empty());
        session.disconnect().await;
        let replacement = recovery::harness_at(harness.store.lock().await.clone()).await;
        let request = recovery::request(&*replacement.store.lock().await, false)?;
        let (restored, executor) =
            recovery::restore(request, replacement.clone(), Vec::new()).await?;
        assert_eq!(
            restored.drive().await?.run.as_ref().map(|run| run.status),
            Some(RunStatus::Failed)
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        assert!(replacement.sent.lock().await.is_empty());
        restored
            .release("release", restored.head().await.state_revision)
            .await?;
    }
    Ok(())
}

struct MetadataCost {
    amount: u64,
    non_finite: bool,
}

impl bitrouter_sdk::language_model::native_accounting::NativeCostEstimator for MetadataCost {
    fn estimate(
        &self,
        report: &bitrouter_sdk::language_model::native::NativeAttemptReport,
    ) -> bitrouter_sdk::language_model::native_accounting::NativeTokenCost {
        use bitrouter_sdk::language_model::native_accounting::NativeTokenCost;
        let mut cost = accounting::FixtureCost.estimate(report);
        if let NativeTokenCost::ConfiguredEstimate {
            micro_usd,
            rates,
            pricing_version,
            ..
        } = &mut cost
        {
            *micro_usd = self.amount;
            if self.non_finite {
                rates.output = Some(f64::INFINITY);
            } else {
                *pricing_version = "price\0\"".repeat(20_000);
            }
        }
        cost
    }
}

#[tokio::test]
async fn report_metadata_rejection_restores_estimates_after_outcome_ack_loss() -> TestResult {
    for (amount, non_finite, committed) in [(0, false, false), (7, false, true), (7, true, false)] {
        let harness = Arc::new(Harness::new(None, None));
        let port = Arc::new(reconnect::FaultPort::new(
            harness.clone(),
            "model.attempt.outcome",
            committed,
        ));
        let executor = Arc::new(RecordingExecutor {
            mock: MockExecutor::new(vec![output(vec![call("must-not-run")])]),
            agent_once: Mutex::new(Default::default()),
            prompts: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let table = StaticRoutingTable::new();
        table.insert("fixture-model", vec![target("first"), target("fallback")]);
        let records = UsageRecords::default();
        let app = App::builder()
            .language_model(|builder| {
                builder
                    .routing_table(Arc::new(table))
                    .executor(executor.clone())
                    .native_cost_estimator(Arc::new(MetadataCost { amount, non_finite }))
                    .settlement_recorder(records.clone());
            })
            .build()?;
        let session = bind_app(Arc::new(app), port).await?;
        let mut task = input();
        task.limits = Some(Limits {
            checkpoint_bytes: 256 * 1024,
            ..Limits::default()
        });
        session.start("input", 1, task).await?;
        assert!(session.drive().await.is_err());
        reconnect::reconnect(&session, &harness).await?;
        let state = session.drive().await?;
        assert_eq!(
            state.run.as_ref().map(|run| run.status),
            Some(RunStatus::Failed)
        );
        let receipt = state.root_turn().ok_or("turn")?.steps[0].attempts[0]
            .receipt
            .as_ref()
            .ok_or("receipt")?;
        assert_eq!(receipt.cost_micro_usd, Some(amount));
        assert_eq!(receipt.cost_source, "configured_token_estimate");
        assert!(receipt.report.result.is_none() && receipt.report.output_rejection.is_none());
        let rejected = receipt
            .report
            .report_rejection
            .as_ref()
            .ok_or("report rejection")?;
        assert!(rejected.had_result);
        assert_eq!(
            rejected
                .usage
                .as_ref()
                .map(|usage| (usage.prompt_tokens, usage.completion_tokens)),
            Some((7, 3))
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(*records.0.lock().await, vec![(7, 3, false)]);
        assert!(harness.sent.lock().await.is_empty());
        session.disconnect().await;
        let replacement = recovery::harness_at(harness.store.lock().await.clone()).await;
        let request = recovery::request(&*replacement.store.lock().await, false)?;
        let (restored, executor) =
            recovery::restore(request, replacement.clone(), Vec::new()).await?;
        let after = restored.drive().await?;
        assert_eq!(
            after.run.as_ref().map(|run| run.status),
            Some(RunStatus::Failed)
        );
        let retained = after.root_turn().ok_or("turn")?.steps[0].attempts[0]
            .receipt
            .as_ref()
            .ok_or("receipt")?;
        assert_eq!(retained.report, receipt.report);
        assert_eq!(retained.cost_micro_usd, Some(amount));
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        assert!(replacement.sent.lock().await.is_empty());
        restored
            .release("release", restored.head().await.state_revision)
            .await?;
    }
    Ok(())
}
