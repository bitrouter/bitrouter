use super::*;
use bitrouter_sdk::language_model::hooks::{DenyReason, HookDecision, PreRequestHook};
use bitrouter_sdk::language_model::native::{
    InputTokenCounting, NativeEvidenceCommitment, NativeInputCount,
};

struct FailedPreparation {
    calls: AtomicUsize,
    hold: bool,
    fault: Option<Arc<reconnect::FaultPort>>,
    entered: Semaphore,
    resume: Semaphore,
}

struct PreparationHook(Arc<FailedPreparation>);

struct DenyOnce {
    calls: Arc<AtomicUsize>,
    fault: Arc<reconnect::FaultPort>,
}

#[async_trait]
impl PreRequestHook for DenyOnce {
    async fn check(&self, _: &mut PipelineContext) -> bitrouter_sdk::Result<HookDecision> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.fault.set_enabled(true);
            Ok(HookDecision::Deny(DenyReason::Forbidden(
                "request denied".into(),
            )))
        } else {
            Ok(HookDecision::Allow)
        }
    }
}

#[tokio::test]
async fn denied_preparation_survives_outcome_ack_loss_without_rechecking() -> TestResult {
    for kind in [
        "pre_resolution_hook",
        "router_preparation_hook",
        "pre_request_hook",
    ] {
        for committed in [false, true] {
            let harness = Arc::new(Harness::new(None, None));
            let fault = Arc::new(reconnect::FaultPort::new(
                harness.clone(),
                "preparation.work.outcome",
                committed,
            ));
            fault.set_enabled(false);
            let calls = Arc::new(AtomicUsize::new(0));
            let hook = DenyOnce {
                calls: calls.clone(),
                fault: fault.clone(),
            };
            let executor = Arc::new(LargeCount {
                counts: AtomicUsize::new(0),
                generations: AtomicUsize::new(0),
            });
            let table = StaticRoutingTable::new();
            table.insert("fixture-model", vec![target("first")]);
            let app = App::builder()
                .language_model(|builder| {
                    builder
                        .routing_table(Arc::new(table))
                        .executor(executor.clone());
                    match kind {
                        "pre_resolution_hook" => {
                            builder.pre_resolution_hook(hook);
                        }
                        "router_preparation_hook" => {
                            builder.router_preparation_hook(hook);
                        }
                        _ => {
                            builder.pre_request_hook(hook);
                        }
                    }
                })
                .build()?;
            let session = bind_app(Arc::new(app), fault).await?;
            session.start("input", 1, input()).await?;
            assert!(session.drive().await.is_err());
            reconnect::reconnect(&session, &harness).await?;
            // Capture a process replacement before in-process recovery can
            // settle the failed step and hide a missing durable denial.
            let durable = harness.store.lock().await.clone();
            let replacement = recovery::harness_at(durable).await;
            let request = recovery::request(&*replacement.store.lock().await, false)?;
            let (restored, restored_executor) = recovery::restore(
                request,
                replacement.clone(),
                vec![output(vec![text("must not execute")])],
            )
            .await?;
            let cold = restored.drive().await?;
            assert_eq!(
                cold.run.as_ref().map(|run| run.status),
                Some(RunStatus::Failed),
                "{kind}/{committed}"
            );
            assert_eq!(restored_executor.calls.load(Ordering::SeqCst), 0);
            assert!(replacement.sent.lock().await.is_empty());
            let done = session.drive().await?;
            assert_eq!(
                done.run.as_ref().map(|run| run.status),
                Some(RunStatus::Failed)
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1, "{kind}/{committed}");
            assert_eq!(executor.generations.load(Ordering::SeqCst), 0);
            let step = &done.root_turn().ok_or("turn")?.steps[0];
            assert_eq!(
                step.preparation_work
                    .last()
                    .and_then(|record| record.report.as_ref())
                    .and_then(|report| report.error_code.as_deref()),
                Some("permission_denied")
            );
            assert!(step.settled);
        }
    }
    Ok(())
}

#[async_trait]
impl PreRequestHook for PreparationHook {
    async fn check(&self, _: &mut PipelineContext) -> bitrouter_sdk::Result<HookDecision> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        self.0.entered.add_permits(1);
        if self.0.hold {
            self.0
                .resume
                .acquire()
                .await
                .map_err(|error| bitrouter_sdk::BitrouterError::internal(error.to_string()))?
                .forget();
        }
        if let Some(fault) = &self.0.fault {
            fault.set_enabled(true);
        }
        Err(bitrouter_sdk::BitrouterError::internal(
            "preparation\0\"".repeat(100_000),
        ))
    }
}

struct LargeCount {
    counts: AtomicUsize,
    generations: AtomicUsize,
}

#[async_trait]
impl Executor for LargeCount {
    async fn count_input_tokens(
        &self,
        _: &RoutingTarget,
        _: &Prompt,
        _: &PipelineContext,
    ) -> bitrouter_sdk::Result<NativeInputCount> {
        self.counts.fetch_add(1, Ordering::SeqCst);
        Ok(NativeInputCount::Counted {
            input_tokens: 0,
            request_sha256: "sha256:fixture".into(),
            source: "counter\0\"".repeat(100_000),
        })
    }
    async fn execute(
        &self,
        _: &RoutingTarget,
        _: &Prompt,
        _: &PipelineContext,
    ) -> bitrouter_sdk::Result<ExecutionResult> {
        self.generations.fetch_add(1, Ordering::SeqCst);
        Err(bitrouter_sdk::BitrouterError::internal(
            "generation must not run",
        ))
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
async fn oversized_auxiliary_failures_survive_ack_loss_without_reexecution() -> TestResult {
    for count in [false, true] {
        for committed in [None, Some(false), Some(true)] {
            let harness = Arc::new(Harness::new(None, None));
            let fault = committed.map(|committed| {
                Arc::new(reconnect::FaultPort::new(
                    harness.clone(),
                    if count {
                        "model.input_count.outcome"
                    } else {
                        "preparation.work.outcome"
                    },
                    committed,
                ))
            });
            if !count && let Some(fault) = &fault {
                fault.set_enabled(false);
            }
            let port: Arc<dyn HarnessPort> = match &fault {
                Some(fault) => fault.clone(),
                None => harness.clone(),
            };
            let hook = Arc::new(FailedPreparation {
                calls: AtomicUsize::new(0),
                hold: false,
                fault,
                entered: Semaphore::new(0),
                resume: Semaphore::new(0),
            });
            let executor = Arc::new(LargeCount {
                counts: AtomicUsize::new(0),
                generations: AtomicUsize::new(0),
            });
            let mut route = target("first");
            route.model_constraints.input_token_counting = Some(InputTokenCounting::Responses);
            route.model_constraints.token_limits.max_input_tokens = Some(500);
            let table = StaticRoutingTable::new();
            table.insert("fixture-model", vec![route.clone(), route]);
            let app = App::builder()
                .language_model(|builder| {
                    builder
                        .routing_table(Arc::new(table))
                        .executor(executor.clone());
                    if !count {
                        builder.pre_request_hook(PreparationHook(hook.clone()));
                    }
                })
                .build()?;
            let session = bind_app(Arc::new(app), port).await?;
            let mut task = input();
            task.limits = Some(Limits {
                checkpoint_bytes: 256 * 1024,
                ..Limits::default()
            });
            session.start("input", 1, task).await?;
            if committed.is_some() {
                assert!(session.drive().await.is_err());
                reconnect::reconnect(&session, &harness).await?;
            }
            let state = session.drive().await?;
            assert_eq!(
                state.run.as_ref().map(|run| run.status),
                Some(RunStatus::Failed)
            );
            let step = &state.root_turn().ok_or("turn")?.steps[0];
            assert_eq!(step.auxiliary_output_version, Some(1));
            if count {
                assert_eq!(executor.counts.load(Ordering::SeqCst), 1);
                let report = step.input_counts[0].report.as_ref().ok_or("count report")?;
                let rejected = report.report_rejection.as_ref().ok_or("count rejection")?;
                assert_eq!(rejected.input_tokens, Some(0));
                assert!(rejected.original.bytes > rejected.byte_limit);
                assert!(matches!(
                    report.outcome,
                    NativeInputCount::Unavailable { .. }
                ));
                if committed.is_none() {
                    reject_forged_count_recovery(&harness.store.lock().await.clone()).await?;
                }
            } else {
                assert_eq!(hook.calls.load(Ordering::SeqCst), 1);
                if committed.is_none() {
                    let diagnostic = step
                        .failure_diagnostic
                        .as_ref()
                        .ok_or("failure commitment")?;
                    let error =
                        bitrouter_sdk::BitrouterError::internal("preparation\0\"".repeat(100_000))
                            .to_string();
                    assert_eq!(*diagnostic, NativeEvidenceCommitment::capture(&error)?);
                    assert!(
                        state
                            .root_turn()
                            .ok_or("turn")?
                            .terminal_reason
                            .as_ref()
                            .is_some_and(|reason| reason.contains("commitment"))
                    );
                }
            }
            assert!(step.settled);
            assert_eq!(executor.generations.load(Ordering::SeqCst), 0);
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
    }
    Ok(())
}

#[tokio::test]
async fn pending_preparation_keeps_outcome_capacity_through_saturation_and_ack_loss() -> TestResult
{
    for committed in [false, true] {
        let harness = Arc::new(Harness::new(None, None));
        let fault = Arc::new(reconnect::FaultPort::new(
            harness.clone(),
            "preparation.work.outcome",
            committed,
        ));
        fault.set_enabled(false);
        let hook = Arc::new(FailedPreparation {
            calls: AtomicUsize::new(0),
            hold: true,
            fault: Some(fault.clone()),
            entered: Semaphore::new(0),
            resume: Semaphore::new(0),
        });
        let executor = Arc::new(LargeCount {
            counts: AtomicUsize::new(0),
            generations: AtomicUsize::new(0),
        });
        let table = StaticRoutingTable::new();
        table.insert("fixture-model", vec![target("first")]);
        let app = App::builder()
            .language_model(|builder| {
                builder
                    .routing_table(Arc::new(table))
                    .executor(executor.clone())
                    .pre_request_hook(PreparationHook(hook.clone()));
            })
            .build()?;
        let session = bind_app(Arc::new(app), fault.clone()).await?;
        let limits = Limits {
            checkpoint_bytes: 128 * 1024,
            input_bytes: 16 * 1024,
            active_models: 1,
            ..Limits::default()
        };
        let mut task = input();
        task.limits = Some(limits.clone());
        session.start("input", 1, task).await?;
        let driver = tokio::spawn({
            let session = session.clone();
            async move { session.drive().await }
        });
        tokio::time::timeout(Duration::from_secs(10), hook.entered.acquire())
            .await??
            .forget();
        let mut exhausted = false;
        for index in 0..512 {
            let mut update = signal_update(&session, Vec::new()).await;
            update
                .facts
                .insert("competing_state".into(), json!("x".repeat(8192)));
            match session
                .signals(&format!("fill-{index}-{}", "x".repeat(100)), update)
                .await
            {
                Ok(_) => {}
                Err(error) => {
                    assert_eq!(
                        (error.code, error.commit_status),
                        (ErrorCode::LimitExceeded, CommitStatus::NotCommitted)
                    );
                    exhausted = true;
                    break;
                }
            }
        }
        assert!(exhausted, "fixture did not saturate checkpoint admission");
        hook.resume.add_permits(1);
        assert!(
            tokio::time::timeout(Duration::from_secs(10), driver)
                .await??
                .is_err()
        );
        reconnect::reconnect(&session, &harness).await?;
        let done = session.drive().await?;
        assert_eq!(
            done.run.as_ref().map(|run| run.status),
            Some(RunStatus::Failed)
        );
        assert_eq!(
            done.run.as_ref().and_then(|run| run.resource_constraint),
            Some(bitrouter_orchestrator::core::session::ResourceConstraint::CheckpointCapacity)
        );
        let record = done.root_turn().ok_or("turn")?.steps[0]
            .preparation_work
            .last()
            .ok_or("preparation")?;
        assert_eq!(
            record
                .report
                .as_ref()
                .and_then(|report| report.error_code.as_deref()),
            Some("internal_error")
        );
        assert_eq!(hook.calls.load(Ordering::SeqCst), 1);
        assert_eq!(executor.generations.load(Ordering::SeqCst), 0);
        assert!(harness.sent.lock().await.is_empty());
        let proposals = fault.proposals.lock().await;
        let outcomes = proposals
            .iter()
            .filter(|batch| {
                batch.decode(&limits).is_ok_and(|payload| {
                    payload.events.iter().any(|event| {
                        event.kind == "preparation.work.outcome"
                            && event.payload["work"]["kind"] == "pre_request_hook"
                    })
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(outcomes.len(), if committed { 1 } else { 2 });
        if outcomes.len() == 2 {
            assert_eq!(outcomes[0], outcomes[1]);
        }
        drop(proposals);
        session
            .release("release", session.head().await.state_revision)
            .await?;
        for batch in &harness.store.lock().await.batches {
            batch.decode(&limits)?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn auxiliary_policy_preserves_legacy_and_rejects_forged_history() -> TestResult {
    let original = recovery::completed_store().await?;
    for mode in [
        "legacy",
        "unknown",
        "changed_in_journal",
        "forged_failure",
        "oversized_report",
    ] {
        let mut store = original.clone();
        tool_payloads::rewrite_last(&mut store, |payload| {
            let root = payload.checkpoint.state["agent_id"]
                .as_str()
                .map(str::to_owned);
            if let Some(root) = root {
                let step = &mut payload.checkpoint.state["agents"][root]["turn"]["steps"][0];
                match mode {
                    "legacy" | "changed_in_journal" => {
                        if let Some(step) = step.as_object_mut() {
                            step.remove("auxiliary_output_version");
                        }
                    }
                    "unknown" => step["auxiliary_output_version"] = json!(2),
                    "forged_failure" => {
                        step["failure_diagnostic"] = json!({"bytes":1,"sha256":"a".repeat(64)})
                    }
                    "oversized_report" => {
                        step["preparation_work"][0]["report"]["error_code"] =
                            json!("x".repeat(8192));
                    }
                    _ => {}
                }
            }
        })?;
        let harness = recovery::harness_at(store).await;
        let before = harness.store.lock().await.head.clone();
        let request =
            recovery::request(&*harness.store.lock().await, mode == "changed_in_journal")?;
        let restored = recovery::restore(request, harness.clone(), Vec::new()).await;
        if mode == "legacy" {
            let (session, executor) = restored?;
            assert!(
                session.snapshot().await.root_turn().ok_or("turn")?.steps[0]
                    .auxiliary_output_version
                    .is_none()
            );
            assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        } else {
            assert_eq!(
                restored
                    .err()
                    .ok_or("forged policy accepted")?
                    .downcast_ref::<CoreError>()
                    .map(|error| error.code),
                Some(ErrorCode::CheckpointConflict)
            );
            assert_eq!(harness.store.lock().await.head, before);
        }
    }
    Ok(())
}

async fn reject_forged_count_recovery(original: &DurableHarness) -> TestResult {
    for mode in [
        "legacy", "version", "limit", "bytes", "digest", "request", "fit",
    ] {
        let mut store = original.clone();
        tool_payloads::rewrite_last(&mut store, |payload| {
            let root = payload.checkpoint.state["agent_id"]
                .as_str()
                .map(str::to_owned);
            if let Some(root) = root {
                let step = &mut payload.checkpoint.state["agents"][root]["turn"]["steps"][0];
                if mode == "legacy" {
                    if let Some(step) = step.as_object_mut() {
                        step.remove("auxiliary_output_version");
                    }
                    return;
                }
                let report = &mut step["input_counts"][0]["report"];
                match mode {
                    "version" => report["report_rejection"]["version"] = json!(2),
                    "limit" => report["report_rejection"]["byte_limit"] = json!(1),
                    "bytes" => report["report_rejection"]["original"]["bytes"] = json!(1),
                    "digest" => {
                        report["report_rejection"]["original"]["sha256"] = json!("z".repeat(64))
                    }
                    "request" => report["request_id"] = json!("different-request"),
                    "fit" => {
                        report["outcome"] = json!({"status":"counted","input_tokens":0,"source":"forged","request_sha256":"sha256:forged"})
                    }
                    _ => {}
                }
            }
        })?;
        let harness = recovery::harness_at(store).await;
        let before = harness.store.lock().await.head.clone();
        let request = recovery::request(&*harness.store.lock().await, false)?;
        let failure = recovery::restore(request, harness.clone(), Vec::new())
            .await
            .err()
            .ok_or("forged count accepted")?;
        assert_eq!(
            failure.downcast_ref::<CoreError>().map(|error| error.code),
            Some(ErrorCode::CheckpointConflict),
            "{mode}"
        );
        assert_eq!(harness.store.lock().await.head, before);
    }
    Ok(())
}
