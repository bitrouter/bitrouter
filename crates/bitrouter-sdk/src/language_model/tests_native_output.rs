use super::*;
use crate::language_model::native::{
    NativeAttemptReport, NativeExecutionControl, NativePlan, NativePlanAdmission, NativeRoute,
};
use crate::language_model::native_accounting::{
    NativeCostEstimator, NativeTokenCost, NativeTokenRates,
};
use crate::language_model::types::UsageOrigin;
use tokio::sync::Mutex;

struct Control {
    limit: Option<u64>,
    report_limit: Option<u64>,
    reports: Mutex<Vec<NativeAttemptReport>>,
}

#[async_trait]
impl NativeExecutionControl for Control {
    fn canonical_output_byte_limit(&self) -> Option<u64> {
        self.limit
    }
    fn attempt_report_byte_limit(&self, _: &str, _: &NativeRoute) -> Result<Option<u64>> {
        Ok(self.report_limit)
    }
    async fn plan(&self, plan: NativePlan) -> Result<NativePlanAdmission> {
        Ok(NativePlanAdmission {
            route_indices: (0..plan.routes.len() as u32).collect(),
        })
    }
    async fn before_attempt(&self, _: &str, _: u32) -> Result<()> {
        Ok(())
    }
    async fn after_attempt(&self, report: NativeAttemptReport) {
        self.reports.lock().await.push(report);
    }
}

#[derive(Clone, Default)]
struct Records(Arc<Mutex<Vec<UsageRecord>>>);

struct UsageRecord {
    provider: String,
    model: String,
    prompt_tokens: u64,
    completion_tokens: u64,
    raw: Option<serde_json::Value>,
    error: bool,
}

#[async_trait]
impl SettlementRecorder for Records {
    async fn record(&self, ctx: &mut SettlementContext) -> Result<()> {
        self.0.lock().await.push(UsageRecord {
            provider: ctx.provider_id.clone(),
            model: ctx.model_id.clone(),
            prompt_tokens: ctx.prompt_tokens,
            completion_tokens: ctx.completion_tokens,
            raw: ctx.raw_usage.clone(),
            error: ctx.error.is_some(),
        });
        Ok(())
    }
}

struct Estimator;
impl NativeCostEstimator for Estimator {
    fn estimate(&self, report: &NativeAttemptReport) -> NativeTokenCost {
        let Some(usage) = report
            .result
            .as_ref()
            .and_then(|result| result.usage.as_ref())
        else {
            return NativeTokenCost::unknown("usage_missing");
        };
        assert!(usage.raw.is_some(), "estimate sees the original usage");
        NativeTokenCost::ConfiguredEstimate {
            micro_usd: 7,
            usage_origin: usage.origin,
            normalized_usage: usage.normalized_buckets().unwrap_or_default(),
            rates: NativeTokenRates {
                uncached_input: Some(1.0),
                output: Some(1.0),
                cache_read: None,
                cache_write: None,
            },
            pricing_version: "fixture".into(),
            pricing_provider: "first".into(),
            pricing_model: "test-model".into(),
        }
    }
}

fn result() -> GenerateResult {
    GenerateResult {
        content: vec![Content::Text {
            text: "界\0\n\\\"".repeat(80),
            provider_metadata: Default::default(),
        }],
        usage: Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            reasoning_tokens: 2,
            origin: UsageOrigin::ProviderReported,
            raw: Some(Box::new(
                serde_json::json!({"prompt_tokens":10,"completion_tokens":5,"billing_extension":"retained"}),
            )),
            ..Default::default()
        }),
        finish_reason: Some(FinishReason::Stop),
        response_id: None,
        stop_details: None,
        provider_metadata: Default::default(),
    }
}

struct RejectingSuccessHook(Arc<AtomicUsize>);

#[async_trait]
impl ExecutionHook for RejectingSuccessHook {
    async fn on_success(&self, _: &PipelineContext, _: &ExecutionResult) -> Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(BitrouterError::internal("fixture hook rejected output"))
    }
    async fn on_failure(&self, _: &PipelineContext, _: &BitrouterError) -> FallbackDecision {
        self.0.fetch_add(1, Ordering::SeqCst);
        FallbackDecision::TryNext
    }
}

#[tokio::test]
async fn canonical_output_rejection_settles_before_fallible_success_hooks()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let calls = Arc::new(AtomicUsize::new(0));
    let control = Arc::new(Control {
        limit: Some(1),
        report_limit: None,
        reports: Mutex::new(Vec::new()),
    });
    let records = Records::default();
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(routing_table(&["first", "fallback"]))
        .executor(Arc::new(MockExecutor::new(vec![MockResponse::Generate(
            result(),
        )])))
        .execution_hook(RejectingSuccessHook(calls.clone()))
        .settlement_recorder(records.clone());
    let pipeline = Arc::new(builder.build()?);
    assert!(
        pipeline
            .clone()
            .execute_native_controlled(request(), control.clone())
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(control.reports.lock().await.len(), 1);
    let records = records.0.lock().await;
    assert_eq!(records.len(), 1);
    assert_eq!(
        (records[0].prompt_tokens, records[0].completion_tokens),
        (10, 5)
    );
    assert!(records[0].raw.is_some());
    assert!(records[0].error);
    Ok(())
}

#[tokio::test]
async fn canonical_output_limit_counts_json_and_keeps_original_settlement()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let result = result();
    let bytes = serde_json::to_vec(&result)?.len() as u64;
    for limit in [Some(0), Some(bytes - 1), Some(bytes), Some(bytes + 1), None] {
        let control = Arc::new(Control {
            limit,
            report_limit: None,
            reports: Mutex::new(Vec::new()),
        });
        let records = Records::default();
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routing_table(&["first", "fallback"]))
            .executor(Arc::new(MockExecutor::new(vec![
                MockResponse::Generate(result.clone()),
                MockResponse::Generate(result.clone()),
            ])))
            .native_cost_estimator(Arc::new(Estimator))
            .settlement_recorder(records.clone());
        let pipeline = Arc::new(builder.build()?);
        let outcome = pipeline
            .clone()
            .execute_native_controlled(request(), control.clone())
            .await;
        let rejected = limit.is_some_and(|limit| limit < bytes);
        assert_eq!(
            outcome.is_err(),
            rejected,
            "limit {limit:?}: {:?}",
            outcome.as_ref().err()
        );
        let reports = control.reports.lock().await;
        assert_eq!(reports.len(), 1, "rejection never dispatches fallback");
        let report = &reports[0];
        assert_eq!(report.result.is_none(), rejected);
        assert_eq!(report.token_cost.estimated_micro_usd(), Some(7));
        assert_eq!(report.output_rejection.is_some(), rejected);
        if let Some(rejection) = &report.output_rejection {
            assert_eq!(Some(rejection.byte_limit), limit);
            assert_eq!(
                rejection.usage.as_ref().map(|usage| (
                    usage.prompt_tokens,
                    usage.completion_tokens,
                    usage.origin
                )),
                Some((10, 5, UsageOrigin::ProviderReported))
            );
            let encoded = serde_json::to_string(report)?;
            assert!(!encoded.contains("billing_extension"));
            assert!(!encoded.contains('界'));
        }
        let records = records.0.lock().await;
        assert_eq!(records.len(), 1);
        assert_eq!(
            (
                records[0].prompt_tokens,
                records[0].completion_tokens,
                records[0].error
            ),
            (10, 5, rejected)
        );
        assert_eq!(
            records[0]
                .raw
                .as_ref()
                .and_then(|value| value.get("billing_extension"))
                .and_then(serde_json::Value::as_str),
            Some("retained")
        );
    }
    Ok(())
}

#[tokio::test]
async fn canonical_output_without_usage_stays_unknown()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut result = result();
    result.usage = None;
    let control = Arc::new(Control {
        limit: Some(1),
        report_limit: None,
        reports: Mutex::new(Vec::new()),
    });
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(routing_table(&["first"]))
        .executor(Arc::new(MockExecutor::new(vec![MockResponse::Generate(
            result,
        )])))
        .native_cost_estimator(Arc::new(Estimator));
    assert!(
        Arc::new(builder.build()?)
            .execute_native_controlled(request(), control.clone())
            .await
            .is_err()
    );
    let reports = control.reports.lock().await;
    assert_eq!(reports.len(), 1);
    assert!(
        reports[0]
            .output_rejection
            .as_ref()
            .is_some_and(|rejection| rejection.usage.is_none())
    );
    assert!(reports[0].token_cost.estimated_micro_usd().is_none());
    Ok(())
}

struct ActualIdentityExecutor {
    calls: AtomicUsize,
    large_identity: bool,
}

#[async_trait]
impl Executor for ActualIdentityExecutor {
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> Result<ExecutionResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut execution = MockExecutor::new(vec![MockResponse::Generate(result())])
            .execute(target, prompt, ctx)
            .await?;
        execution.provider_id = if self.large_identity {
            "actual\0\"provider".repeat(4096)
        } else {
            "actual-provider".into()
        };
        execution.model_id = "actual-model".into();
        Ok(execution)
    }
    async fn execute_stream(
        &self,
        _: &RoutingTarget,
        _: &Prompt,
        _: &PipelineContext,
    ) -> Result<StreamPartStream> {
        Err(BitrouterError::internal("unexpected stream"))
    }
}

struct ReportEstimator {
    amount: Option<u64>,
    large_metadata: bool,
}

impl NativeCostEstimator for ReportEstimator {
    fn estimate(&self, report: &NativeAttemptReport) -> NativeTokenCost {
        assert!(
            report
                .actual_provider
                .as_deref()
                .is_some_and(|id| id.starts_with("actual"))
        );
        assert_eq!(report.actual_model.as_deref(), Some("actual-model"));
        let cost = Estimator.estimate(report);
        match (self.amount, cost) {
            (
                Some(amount),
                NativeTokenCost::ConfiguredEstimate {
                    usage_origin,
                    normalized_usage,
                    rates,
                    pricing_provider,
                    pricing_model,
                    ..
                },
            ) => NativeTokenCost::ConfiguredEstimate {
                micro_usd: amount,
                usage_origin,
                normalized_usage,
                rates,
                pricing_version: if self.large_metadata {
                    "price\0\"".repeat(4096)
                } else {
                    "v1".into()
                },
                pricing_provider,
                pricing_model,
            },
            _ => NativeTokenCost::unknown("unknown\0\"".repeat(4096)),
        }
    }
}

#[tokio::test]
async fn complete_report_rejection_preserves_original_settlement_and_numeric_cost()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    for canonical_limit in [None, Some(1)] {
        for (amount, large_identity, large_metadata) in [
            (None, false, false),
            (Some(0), false, true),
            (Some(7), true, false),
        ] {
            let control = Arc::new(Control {
                limit: canonical_limit,
                report_limit: Some(16 * 1024),
                reports: Mutex::new(Vec::new()),
            });
            let executor = Arc::new(ActualIdentityExecutor {
                calls: AtomicUsize::new(0),
                large_identity,
            });
            let hook_calls = Arc::new(AtomicUsize::new(0));
            let records = Records::default();
            let mut builder = PipelineBuilder::new();
            builder
                .routing_table(routing_table(&["first", "fallback"]))
                .executor(executor.clone())
                .native_cost_estimator(Arc::new(ReportEstimator {
                    amount,
                    large_metadata,
                }))
                .execution_hook(RejectingSuccessHook(hook_calls.clone()))
                .settlement_recorder(records.clone());
            assert!(
                Arc::new(builder.build()?)
                    .execute_native_controlled(request(), control.clone())
                    .await
                    .is_err()
            );
            assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
            assert_eq!(hook_calls.load(Ordering::SeqCst), 0);
            let reports = control.reports.lock().await;
            assert_eq!(reports.len(), 1);
            let report = &reports[0];
            assert!(serde_json::to_vec(report)?.len() <= 16 * 1024);
            assert!(report.result.is_none());
            assert_eq!(report.output_rejection.is_some(), canonical_limit.is_some());
            assert_eq!(report.token_cost.estimated_micro_usd(), amount);
            let rejection = report.report_rejection.as_ref().ok_or("report rejection")?;
            assert!(rejection.had_result && rejection.original.bytes > rejection.byte_limit);
            assert!(rejection.actual_provider.is_some() && rejection.actual_model.is_some());
            assert_eq!(
                rejection
                    .usage
                    .as_ref()
                    .map(|usage| (usage.prompt_tokens, usage.completion_tokens)),
                Some((10, 5))
            );
            assert!(report.actual_provider.is_none() && report.actual_model.is_none());
            let settled = records.0.lock().await;
            assert_eq!(settled.len(), 1);
            assert_eq!(
                (settled[0].prompt_tokens, settled[0].completion_tokens),
                (10, 5)
            );
            assert!(settled[0].raw.is_some() && settled[0].error);
            assert_eq!(settled[0].model, "actual-model");
            assert_eq!(
                settled[0].provider,
                if large_identity {
                    "actual\0\"provider".repeat(4096)
                } else {
                    "actual-provider".into()
                }
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn insufficient_report_envelope_prevents_provider_dispatch()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let control = Arc::new(Control {
        limit: None,
        report_limit: Some(1),
        reports: Mutex::new(Vec::new()),
    });
    let executor = Arc::new(ActualIdentityExecutor {
        calls: AtomicUsize::new(0),
        large_identity: true,
    });
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(routing_table(&["first", "fallback"]))
        .executor(executor.clone());
    let error = Arc::new(builder.build()?)
        .execute_native_controlled(request(), control.clone())
        .await
        .err()
        .ok_or("small envelope accepted")?;
    assert!(error.to_string().contains("cannot hold rejection evidence"));
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert!(control.reports.lock().await.is_empty());
    Ok(())
}
