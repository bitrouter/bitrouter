use super::*;
use bitrouter_orchestrator::core::accounting::work::{CostWorkKind, CostWorkState};
use bitrouter_sdk::language_model::native::NativeAttemptReport;
use bitrouter_sdk::language_model::native_accounting::{
    NativeCostBasis, NativeCostClaim, NativeCostEstimator, NativeCostObservation, NativeCostScope,
    NativeCostSource, NativeTokenCost, NativeTokenRates,
};
use bitrouter_sdk::language_model::types::UsageOrigin;

pub(super) struct FixtureCost;
impl NativeCostEstimator for FixtureCost {
    fn estimate(&self, report: &NativeAttemptReport) -> NativeTokenCost {
        if report.result.is_none() {
            return NativeTokenCost::unknown("fixture_failed_attempt");
        }
        NativeTokenCost::ConfiguredEstimate {
            micro_usd: 7,
            usage_origin: UsageOrigin::Estimated,
            normalized_usage: Default::default(),
            rates: NativeTokenRates {
                uncached_input: Some(1.0),
                cache_read: None,
                cache_write: None,
                output: Some(1.0),
            },
            pricing_version: "fixture-price".into(),
            pricing_provider: report.actual_provider.clone().unwrap_or_default(),
            pricing_model: report.actual_model.clone().unwrap_or_default(),
        }
    }
}

#[derive(Default)]
struct ClaimsSource {
    mode: AtomicUsize,
}

#[async_trait]
impl NativeCostSource for ClaimsSource {
    async fn read(
        &self,
        _: &CallerContext,
        ids: &[String],
    ) -> bitrouter_sdk::Result<Vec<NativeCostObservation>> {
        let mode = self.mode.load(Ordering::SeqCst);
        if mode == 1 {
            return Err(bitrouter_sdk::BitrouterError::internal(
                "fixture source unavailable",
            ));
        }
        let mut observations = ids
            .iter()
            .map(|request_id| NativeCostObservation {
                request_id: if mode == 2 {
                    "foreign-request".into()
                } else {
                    request_id.clone()
                },
                claims: vec![NativeCostClaim {
                    request_id: request_id.clone(),
                    source: "fixture_costs".into(),
                    bill_id: if mode == 5 {
                        "shared-bill".into()
                    } else {
                        request_id.clone()
                    },
                    basis: NativeCostBasis::Estimated,
                    scope: NativeCostScope::ModelTokens,
                    micro_usd: if mode == 4 { 12 } else { 11 },
                    evidence_sha256: format!("sha256:{}", "a".repeat(64)),
                    provider: "first".into(),
                    model: "fixture-model".into(),
                }],
                unknown_reason: Some("reported_cost_unavailable".into()),
            })
            .collect::<Vec<_>>();
        if mode == 3 {
            observations.extend(observations.clone());
        }
        Ok(observations)
    }
}

async fn sourced_session(
    harness: Arc<Harness>,
    source: Arc<dyn NativeCostSource>,
) -> Result<CoreSession, Box<dyn std::error::Error>> {
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first")]);
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(Arc::new(MockExecutor::new(vec![
                    output(vec![text("done")]),
                    output(vec![text("next")]),
                ])))
                .native_cost_estimator(Arc::new(FixtureCost))
                .native_cost_source(source);
        })
        .build()?;
    bind_app(Arc::new(app), harness).await
}

struct PendingSource {
    entered: Semaphore,
}

struct DelayedFailure {
    first: AtomicBool,
    entered: Semaphore,
    resume: Semaphore,
    returned: Semaphore,
}

#[async_trait]
impl NativeCostSource for DelayedFailure {
    async fn read(
        &self,
        _: &CallerContext,
        _: &[String],
    ) -> bitrouter_sdk::Result<Vec<NativeCostObservation>> {
        if self.first.swap(false, Ordering::SeqCst) {
            self.entered.add_permits(1);
            self.resume
                .acquire()
                .await
                .map_err(|_| bitrouter_sdk::BitrouterError::internal("fixture stopped"))?
                .forget();
            self.returned.add_permits(1);
        }
        Err(bitrouter_sdk::BitrouterError::internal(
            "fixture source unavailable",
        ))
    }
}

#[tokio::test]
async fn monetary_claims_source_failure_preserves_output_during_provisional_signal_block()
-> TestResult {
    let source = Arc::new(DelayedFailure {
        first: AtomicBool::new(true),
        entered: Semaphore::new(0),
        resume: Semaphore::new(0),
        returned: Semaphore::new(0),
    });
    let harness = Arc::new(Harness::new(None, Some("collaboration.runtime")));
    let session = sourced_session(harness.clone(), source.clone()).await?;
    session.start("input", 1, input()).await?;
    let driver = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(2), source.entered.acquire())
        .await??
        .forget();
    let state = session.snapshot().await;
    let mut listing = Box::pin(session.collaborate(
        "list",
        session.head().await.state_revision,
        &state.agent_id,
        Action::List {},
    ));
    assert!(futures::poll!(listing.as_mut()).is_pending());
    assert_eq!(harness.seen.available_permits(), 1);
    let mut update =
        Box::pin(session.signals("signals", signal_update(&session, Vec::new()).await));
    // Establish the provisional block but leave this future suspended on inputs
    // until after model output is durable. The List ACK owns both serializers.
    assert!(futures::poll!(update.as_mut()).is_pending());
    source.resume.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), source.returned.acquire())
        .await??
        .forget();
    harness.hold_enabled.store(false, Ordering::SeqCst);
    harness.resume.add_permits(1);
    listing.await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if session
                .snapshot()
                .await
                .root_turn()
                .and_then(|turn| turn.steps.first())
                .is_some_and(|step| step.settled)
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    update.await?;
    let done = tokio::time::timeout(Duration::from_secs(3), driver).await???;
    assert_eq!(done.run.as_ref().ok_or("run")?.status, RunStatus::Completed);
    assert!(
        done.root_turn()
            .ok_or("turn")?
            .steps
            .iter()
            .all(|step| step.settled)
    );
    assert!(
        done.cost_work
            .values()
            .all(|ledger| ledger.charges.is_empty())
    );
    Ok(())
}

#[async_trait]
impl NativeCostSource for PendingSource {
    async fn read(
        &self,
        _: &CallerContext,
        _: &[String],
    ) -> bitrouter_sdk::Result<Vec<NativeCostObservation>> {
        self.entered.add_permits(1);
        std::future::pending().await
    }
}

#[tokio::test]
async fn monetary_claims_pending_source_allows_cancel_and_times_out() -> TestResult {
    let source = Arc::new(PendingSource {
        entered: Semaphore::new(0),
    });
    let session = sourced_session(Arc::new(Harness::new(None, None)), source.clone()).await?;
    let accepted = session.start("input", 1, input()).await?;
    let run_id = &accepted.assigned_ids["run_id"];
    let driver = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(2), source.entered.acquire())
        .await??
        .forget();
    let observed = session.snapshot().await;
    assert_eq!(observed.run.as_ref().ok_or("run")?.model_attempts, 1);
    assert!(observed.cost_work[run_id].charges.is_empty());
    tokio::time::timeout(
        Duration::from_secs(1),
        session.cancel_run("cancel", session.head().await.state_revision, run_id),
    )
    .await??;
    // The uncooperative source is never released: the bounded read must end and
    // discard the already received model output after the acknowledged cancel.
    let cancelled = tokio::time::timeout(Duration::from_secs(6), driver).await???;
    let run = cancelled.run.as_ref().ok_or("run")?;
    assert_eq!(run.status, RunStatus::Cancelled);
    assert!(run.final_answer.is_none());
    assert_eq!(run.model_attempts, 1);
    assert!(cancelled.cost_work[run_id].charges.is_empty());
    assert!(
        cancelled.cost_work[run_id]
            .work
            .values()
            .all(|work| !work.unknown_cost_reason.is_empty())
    );
    Ok(())
}

#[tokio::test]
async fn monetary_claims_pending_source_disconnects_promptly() -> TestResult {
    let source = Arc::new(PendingSource {
        entered: Semaphore::new(0),
    });
    let session = sourced_session(Arc::new(Harness::new(None, None)), source.clone()).await?;
    let accepted = session.start("input", 1, input()).await?;
    let driver = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(2), source.entered.acquire())
        .await??
        .forget();
    session.disconnect().await;
    assert!(
        tokio::time::timeout(Duration::from_secs(1), driver)
            .await??
            .is_err()
    );
    assert!(
        session.snapshot().await.cost_work[&accepted.assigned_ids["run_id"]]
            .charges
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn monetary_claims_bill_cannot_be_reassigned_to_a_successor_run() -> TestResult {
    let source = Arc::new(ClaimsSource::default());
    source.mode.store(5, Ordering::SeqCst);
    let session = sourced_session(Arc::new(Harness::new(None, None)), source).await?;
    let first = session.start("first", 1, input()).await?;
    let completed = session.drive().await?;
    let original = &completed.cost_work[&first.assigned_ids["run_id"]];
    assert_eq!(original.charges.len(), 1);
    let second = session
        .start("second", session.head().await.state_revision, input())
        .await?;
    let completed = session.drive().await?;
    assert_eq!(
        completed.run.as_ref().ok_or("run")?.status,
        RunStatus::Completed
    );
    assert!(
        completed.cost_work[&second.assigned_ids["run_id"]]
            .charges
            .is_empty()
    );
    assert_eq!(
        &completed.cost_work[&first.assigned_ids["run_id"]],
        original
    );
    let head = session.head().await;
    let error = session
        .refresh_costs("cross-run", &second.assigned_ids["run_id"])
        .await
        .err()
        .ok_or("accepted reused bill")?;
    assert_eq!(error.code, ErrorCode::OperationConflict);
    assert_eq!(session.head().await, head);
    assert_eq!(session.snapshot().await.cost_work, completed.cost_work);
    Ok(())
}

#[tokio::test]
async fn monetary_claims_require_ack_even_after_model_usage_is_durable() -> TestResult {
    for fail in [false, true] {
        let harness = Arc::new(Harness::new(
            fail.then_some("cost.observed"),
            (!fail).then_some("cost.observed"),
        ));
        let session = sourced_session(harness.clone(), Arc::new(ClaimsSource::default())).await?;
        session.start("input", 1, input()).await?;
        let driver = tokio::spawn({
            let session = session.clone();
            async move { session.drive().await }
        });
        if !fail {
            tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
                .await??
                .forget();
            let state = session.snapshot().await;
            let run = state.run.as_ref().ok_or("run")?;
            assert_eq!(
                run.token_accounting
                    .as_ref()
                    .and_then(|a| a.complete_estimate_micro_usd(run.model_attempts)),
                Some(7)
            );
            assert!(state.cost_work[&run.run_id].charges.is_empty());
            harness.hold_enabled.store(false, Ordering::SeqCst);
            harness.resume.add_permits(1);
        }
        let result = driver.await?;
        if fail {
            assert!(result.is_err());
        } else {
            assert_eq!(
                result?.run.map(|run| run.status),
                Some(RunStatus::Completed)
            );
        }
        let state = session.snapshot().await;
        let run = state.run.as_ref().ok_or("run")?;
        assert_eq!(
            state.cost_work[&run.run_id].charges.len(),
            usize::from(!fail)
        );
        assert_eq!(
            run.token_accounting
                .as_ref()
                .and_then(|a| a.complete_estimate_micro_usd(run.model_attempts)),
            Some(7)
        );
    }
    Ok(())
}

#[tokio::test]
async fn monetary_claims_reject_source_conflicts_and_retry_read_failures_without_reexecution()
-> TestResult {
    let source = Arc::new(ClaimsSource::default());
    source.mode.store(1, Ordering::SeqCst);
    let session = sourced_session(Arc::new(Harness::new(None, None)), source.clone()).await?;
    session.start("input", 1, input()).await?;
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    let run_id = done.run.as_ref().ok_or("run")?.run_id.clone();
    assert!(done.cost_work[&run_id].charges.is_empty());
    source.mode.store(0, Ordering::SeqCst);
    let receipt = session.refresh_costs("retry-source", &run_id).await?;
    assert_eq!(
        session.refresh_costs("retry-source", &run_id).await?,
        receipt
    );
    let good = session.snapshot().await;
    let head = session.head().await;
    assert_eq!(good.cost_work[&run_id].charges.len(), 1);
    for mode in [2, 3, 4] {
        source.mode.store(mode, Ordering::SeqCst);
        let error = session
            .refresh_costs(&format!("bad-{mode}"), &run_id)
            .await
            .err()
            .ok_or("accepted invalid source")?;
        assert_eq!(error.code, ErrorCode::OperationConflict);
        assert_eq!(session.head().await, head);
        assert_eq!(session.snapshot().await.cost_work, good.cost_work);
    }
    assert_eq!(
        session.snapshot().await.run.map(|run| run.model_attempts),
        Some(1)
    );
    Ok(())
}

async fn session(
    harness: Arc<Harness>,
    responses: Vec<MockResponse>,
    fallback: bool,
) -> Result<CoreSession, Box<dyn std::error::Error>> {
    let table = StaticRoutingTable::new();
    let mut routes = vec![target("first")];
    if fallback {
        routes.push(target("second"));
    }
    table.insert("fixture-model", routes);
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(Arc::new(MockExecutor::new(responses)))
                .native_cost_estimator(Arc::new(FixtureCost));
        })
        .build()?;
    bind_app(Arc::new(app), harness).await
}

#[tokio::test]
async fn estimate_becomes_visible_only_with_outcome_ack_and_is_not_recounted() -> TestResult {
    let harness = Arc::new(Harness::new(None, Some("model.attempt.outcome")));
    let session = session(harness.clone(), vec![output(vec![text("done")])], false).await?;
    session.start("input", 1, input()).await?;
    let driver = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    let state = session.snapshot().await;
    let run = state.run.as_ref().ok_or("missing run")?;
    let accounting = run.token_accounting.as_ref().ok_or("missing accounting")?;
    let ledger = &state.cost_work[&run.run_id];
    let attempt = ledger
        .work
        .values()
        .find(|work| work.kind == CostWorkKind::ProviderAttempt)
        .ok_or("attempt cost")?;
    assert_eq!(attempt.state, CostWorkState::IntentRecorded);
    assert!(attempt.token_estimate.is_none());
    assert_eq!(attempt.unknown_cost_reason, "cost_not_reported");
    assert_eq!(accounting.known_subtotal_micro_usd, Some(0));
    assert_eq!(accounting.pending_attempts(run.model_attempts), 1);
    assert_eq!(
        accounting.complete_estimate_micro_usd(run.model_attempts),
        None
    );
    harness.hold_enabled.store(false, Ordering::SeqCst);
    harness.resume.add_permits(1);
    let state = driver.await??;
    let run = state.run.as_ref().ok_or("missing run")?;
    let accounting = run.token_accounting.as_ref().ok_or("missing accounting")?;
    assert_eq!(
        accounting.complete_estimate_micro_usd(run.model_attempts),
        Some(7)
    );
    let attempt = state.cost_work[&run.run_id]
        .work
        .values()
        .find(|work| work.kind == CostWorkKind::ProviderAttempt)
        .ok_or("attempt cost")?;
    assert_eq!(attempt.state, CostWorkState::OutcomeRecorded);
    assert_eq!(
        attempt
            .token_estimate
            .as_ref()
            .and_then(NativeTokenCost::estimated_micro_usd),
        Some(7)
    );
    assert_eq!(attempt.unknown_cost_reason, "cost_not_reported");
    let again = session.drive().await?;
    assert_eq!(again.cost_work, state.cost_work);
    assert_eq!(
        again
            .run
            .as_ref()
            .and_then(|run| run.token_accounting.as_ref()),
        Some(accounting)
    );
    // A legacy checkpoint cannot invent a complete zero-cost ledger.
    let mut legacy = serde_json::to_value(run)?;
    if let Some(object) = legacy.as_object_mut() {
        object.remove("token_accounting");
    }
    let restored: bitrouter_orchestrator::core::session::RootRun = serde_json::from_value(legacy)?;
    assert!(restored.token_accounting.is_none());
    let mut legacy = serde_json::to_value(&state)?;
    legacy
        .as_object_mut()
        .ok_or("snapshot object")?
        .remove("cost_work");
    let restored: SessionSnapshot = serde_json::from_value(legacy)?;
    assert!(restored.cost_work.is_empty());
    Ok(())
}

#[tokio::test]
async fn failed_outcome_ack_retains_pending_cost_even_after_successful_generation() -> TestResult {
    let harness = Arc::new(Harness::new(Some("model.attempt.outcome"), None));
    let session = session(harness, vec![output(vec![text("done")])], false).await?;
    session.start("input", 1, input()).await?;
    assert!(session.drive().await.is_err());
    let state = session.snapshot().await;
    let run = state.run.as_ref().ok_or("missing run")?;
    let accounting = run.token_accounting.as_ref().ok_or("missing accounting")?;
    assert_eq!(accounting.known_attempts, 0);
    let ledger = &state.cost_work[&run.run_id];
    assert_eq!(
        ledger
            .work
            .values()
            .filter(|work| work.kind == CostWorkKind::ProviderAttempt
                && work.state == CostWorkState::IntentRecorded
                && work.token_estimate.is_none())
            .count(),
        1
    );
    let persisted: SessionSnapshot = serde_json::from_slice(&serde_json::to_vec(&state)?)?;
    assert_eq!(persisted.cost_work, state.cost_work);
    assert_eq!(accounting.pending_attempts(run.model_attempts), 1);
    assert_eq!(
        accounting.complete_estimate_micro_usd(run.model_attempts),
        None
    );
    Ok(())
}

#[tokio::test]
async fn unknown_failed_fallback_keeps_known_subtotal_incomplete() -> TestResult {
    let session = session(
        Arc::new(Harness::new(None, None)),
        vec![
            MockResponse::Error(bitrouter_sdk::BitrouterError::Upstream {
                status: 503,
                message: "retry".into(),
            }),
            output(vec![text("done")]),
        ],
        true,
    )
    .await?;
    session.start("input", 1, input()).await?;
    let state = session.drive().await?;
    let run = state.run.as_ref().ok_or("missing run")?;
    let accounting = run.token_accounting.as_ref().ok_or("missing accounting")?;
    assert_eq!(run.model_attempts, 2);
    assert_eq!(accounting.known_subtotal_micro_usd, Some(7));
    assert_eq!(accounting.unknown_attempts, 1);
    let attempts: Vec<_> = state.cost_work[&run.run_id]
        .work
        .values()
        .filter(|work| work.kind == CostWorkKind::ProviderAttempt)
        .collect();
    assert_eq!(attempts.len(), 2);
    assert!(
        attempts
            .iter()
            .all(|work| work.state == CostWorkState::OutcomeRecorded)
    );
    assert_eq!(
        attempts
            .iter()
            .filter(|work| work
                .token_estimate
                .as_ref()
                .and_then(NativeTokenCost::estimated_micro_usd)
                .is_none())
            .count(),
        1
    );
    assert_eq!(accounting.pending_attempts(run.model_attempts), 0);
    assert_eq!(
        accounting.complete_estimate_micro_usd(run.model_attempts),
        None
    );
    Ok(())
}

#[tokio::test]
async fn child_followups_keep_retired_turn_costs_and_new_root_run_resets_total() -> TestResult {
    let session = session(
        Arc::new(Harness::new(None, None)),
        (0..12).map(|_| output(vec![text("done")])).collect(),
        false,
    )
    .await?;
    let root = session.start("input", 1, input()).await?.assigned_ids["agent_id"].clone();
    let child = session
        .collaborate(
            "spawn",
            session.head().await.state_revision,
            &root,
            Action::Spawn {
                task: work("child"),
            },
        )
        .await?
        .assigned_ids["agent_id"]
        .clone();
    let mut followup = work("followup");
    followup.fresh_context = false;
    session
        .collaborate(
            "followup",
            session.head().await.state_revision,
            &root,
            Action::Followup {
                agent_id: child.clone(),
                task: followup,
            },
        )
        .await?;
    let state = session.drive().await?;
    let run = state.run.as_ref().ok_or("missing run")?;
    let accounting = run.token_accounting.as_ref().ok_or("missing accounting")?;
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(accounting.known_attempts, run.model_attempts);
    assert_eq!(
        accounting.complete_estimate_micro_usd(run.model_attempts),
        Some(u64::from(run.model_attempts) * 7)
    );
    let retained: usize = state
        .agents
        .values()
        .filter_map(|agent| agent.turn.as_ref())
        .flat_map(|turn| &turn.steps)
        .map(|step| step.attempts.len())
        .sum();
    assert!(
        retained < run.model_attempts as usize,
        "a child turn must have retired"
    );
    let old_run_id = run.run_id.clone();
    let old_cost_work = state.cost_work[&old_run_id].clone();
    let attempts: Vec<_> = old_cost_work
        .work
        .values()
        .filter(|work| work.kind == CostWorkKind::ProviderAttempt)
        .collect();
    assert_eq!(attempts.len(), run.model_attempts as usize);
    assert!(attempts.iter().all(|work| {
        work.token_estimate
            .as_ref()
            .and_then(NativeTokenCost::estimated_micro_usd)
            == Some(7)
    }));
    session
        .start("next", session.head().await.state_revision, input())
        .await?;
    let state = session.snapshot().await;
    let run = state.run.as_ref().ok_or("missing next run")?;
    assert_eq!(state.cost_work[&old_run_id], old_cost_work);
    assert!(state.cost_work[&run.run_id].work.is_empty());
    assert_eq!(run.model_attempts, 0);
    assert_eq!(
        run.token_accounting
            .as_ref()
            .and_then(|accounting| accounting.complete_estimate_micro_usd(0)),
        Some(0)
    );
    Ok(())
}

#[tokio::test]
async fn preparation_rejection_keeps_unknown_cost_without_provider_attempt() -> TestResult {
    let session = session(Arc::new(Harness::new(None, None)), Vec::new(), false).await?;
    let mut task = input();
    task.model = "unconfigured-model".into();
    session.start("input", 1, task).await?;
    let state = session.drive().await?;
    let run = state.run.as_ref().ok_or("run")?;
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.model_attempts, 0);
    let ledger = &state.cost_work[&run.run_id];
    assert!(ledger.work.values().all(|work| matches!(
        work.kind,
        CostWorkKind::Preparation | CostWorkKind::PreparationCallback
    )));
    let preparation = ledger
        .work
        .values()
        .find(|work| work.kind == CostWorkKind::Preparation)
        .ok_or("preparation")?;
    assert_eq!(preparation.kind, CostWorkKind::Preparation);
    assert_eq!(preparation.state, CostWorkState::OutcomeRecorded);
    assert_eq!(preparation.unknown_cost_reason, "cost_not_reported");
    assert!(preparation.token_estimate.is_none());
    assert!(preparation.elapsed_ms.is_none());
    Ok(())
}

#[tokio::test]
async fn tool_cost_exposure_is_durable_and_duplicate_result_does_not_add_work() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let session = session(
        harness.clone(),
        vec![output(vec![call("read")]), output(vec![text("done")])],
        false,
    )
    .await?;
    session.start("input", 1, input()).await?;
    let waiting = session.drive().await?;
    let run_id = &waiting.run.as_ref().ok_or("run")?.run_id;
    let command = harness.sent.lock().await[0].clone();
    let work = &waiting.cost_work[run_id].work[&command.attempt_id];
    assert_eq!(work.kind, CostWorkKind::WorkspaceTool);
    assert_eq!(work.state, CostWorkState::IntentRecorded);
    assert_eq!(work.unknown_cost_reason, "harness_cost_not_reported");
    assert_eq!(work.agent_id, command.agent_id);
    let result = result(&command);
    session.tool_result("result", result.clone()).await?;
    let first = session.snapshot().await;
    session.tool_result("duplicate", result).await?;
    let duplicate = session.snapshot().await;
    assert_eq!(duplicate.cost_work, first.cost_work);
    let done = session.drive().await?;
    let work = &done.cost_work[run_id].work[&command.attempt_id];
    assert_eq!(work.state, CostWorkState::OutcomeRecorded);
    assert_eq!(work.unknown_cost_reason, "harness_cost_not_reported");
    let store = harness.store.lock().await;
    let persisted: SessionSnapshot = serde_json::from_value(
        store
            .batches
            .last()
            .ok_or("checkpoint")?
            .decode(&store.limits)?
            .checkpoint
            .state,
    )?;
    assert_eq!(persisted.cost_work, done.cost_work);
    Ok(())
}
