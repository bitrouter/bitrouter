use super::*;
use bitrouter_sdk::language_model::native::NativeAttemptReport;
use bitrouter_sdk::language_model::native_accounting::{
    NativeCostEstimator, NativeTokenCost, NativeTokenRates,
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
    let again = session.drive().await?;
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
    session
        .start("next", session.head().await.state_revision, input())
        .await?;
    let state = session.snapshot().await;
    let run = state.run.as_ref().ok_or("missing next run")?;
    assert_eq!(run.model_attempts, 0);
    assert_eq!(
        run.token_accounting
            .as_ref()
            .and_then(|accounting| accounting.complete_estimate_micro_usd(0)),
        Some(0)
    );
    Ok(())
}
