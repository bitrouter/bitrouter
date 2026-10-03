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
    let receipt = state.root_turn().ok_or("turn")?.steps[0].attempts[0]
        .receipt
        .as_ref()
        .ok_or("receipt")?;
    assert!(receipt.report.result.is_none());
    assert_eq!(receipt.cost_micro_usd, Some(7));
    assert_eq!(receipt.usage_origin, Some(UsageOrigin::ProviderReported));
    let rejected = receipt
        .report
        .output_rejection
        .as_ref()
        .ok_or("rejection")?;
    assert_eq!(rejected.byte_limit, 1024 * 1024);
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
