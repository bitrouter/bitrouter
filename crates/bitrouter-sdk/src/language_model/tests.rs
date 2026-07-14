//! Pipeline integration tests for the `language_model` pipeline.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures::StreamExt;
use serde::Serialize;

use crate::caller::CallerContext;
use crate::error::{BitrouterError, Result};
use crate::event::PipelineEvent;
use crate::language_model::executor::MockResponse;
use crate::language_model::routing::PromptOverrides;
use crate::language_model::*;

// ===== test fixtures =====

fn target(provider: &str) -> RoutingTarget {
    RoutingTarget {
        provider_name: provider.to_string(),
        service_id: "test-model".to_string(),
        api_base: "https://example.invalid".to_string(),
        api_key: "k".to_string(),
        api_protocol: ApiProtocol::ChatCompletions,
        chat_token_limit_field: None,
        account_label: None,
        api_key_override: None,
        api_base_override: None,
        auth_scheme: Default::default(),
    }
}

fn routing_table(providers: &[&str]) -> Arc<StaticRoutingTable> {
    let rt = Arc::new(StaticRoutingTable::new());
    rt.insert("test-model", providers.iter().map(|p| target(p)).collect());
    rt
}

fn request() -> PipelineRequest {
    let prompt = Prompt {
        model: "test-model".to_string(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, "hi")],
        tools: Vec::new(),
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    };
    PipelineRequest::new("test-model", CallerContext::new("k1", "u1"), prompt)
}

fn request_for_model(model: &str) -> PipelineRequest {
    let mut request = request();
    request.model = model.to_string();
    request.prompt.model = model.to_string();
    request
}

#[derive(Serialize)]
struct TestRouteEvent;
impl PipelineEvent for TestRouteEvent {
    fn event_name(&self) -> &'static str {
        "test.route"
    }
}

// ===== test hooks =====

struct AllowHook;
#[async_trait]
impl PreRequestHook for AllowHook {
    async fn check(&self, _ctx: &mut PipelineContext) -> Result<HookDecision> {
        Ok(HookDecision::Allow)
    }
}

struct DenyHook;
#[async_trait]
impl PreRequestHook for DenyHook {
    async fn check(&self, _ctx: &mut PipelineContext) -> Result<HookDecision> {
        Ok(HookDecision::Deny(DenyReason::Unauthorized("nope".into())))
    }
}

/// Records that it ran, so we can prove a stage was / was not reached.
struct CountingPreHook(Arc<AtomicUsize>);
#[async_trait]
impl PreRequestHook for CountingPreHook {
    async fn check(&self, _ctx: &mut PipelineContext) -> Result<HookDecision> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(HookDecision::Allow)
    }
}

struct EmitRouteHook;
#[async_trait]
impl RouteHook for EmitRouteHook {
    async fn resolve(
        &self,
        _chain: &mut Vec<RoutingTarget>,
        ctx: &mut PipelineContext,
    ) -> Result<()> {
        ctx.emit(TestRouteEvent);
        Ok(())
    }
}

struct CountingRecorder(Arc<AtomicUsize>);
#[async_trait]
impl SettlementRecorder for CountingRecorder {
    async fn record(&self, _ctx: &mut SettlementContext) -> Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct RequestEndCountingObserver(Arc<AtomicUsize>);
#[async_trait]
impl ObserveHook for RequestEndCountingObserver {
    async fn after_phase(&self, _phase: Phase, _ctx: &PipelineContext) {}

    async fn on_stream_part(&self, _ctx: &StreamContext, _part: &StreamPart) {}

    async fn on_request_end(&self, _ctx: &PipelineContext, _outcome: &RequestOutcome) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Records the `(prompt_tokens, completion_tokens)` seen by every settlement
/// call so a test can assert what was actually billed.
struct UsageCapturingRecorder(Arc<std::sync::Mutex<Vec<(u64, u64)>>>);
#[async_trait]
impl SettlementRecorder for UsageCapturingRecorder {
    async fn record(&self, ctx: &mut SettlementContext) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .push((ctx.prompt_tokens, ctx.completion_tokens));
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SettlementSnapshot {
    prompt_tokens: u64,
    completion_tokens: u64,
    has_error: bool,
}

struct SettlementSnapshotRecorder(Arc<std::sync::Mutex<Vec<SettlementSnapshot>>>);
#[async_trait]
impl SettlementRecorder for SettlementSnapshotRecorder {
    async fn record(&self, ctx: &mut SettlementContext) -> Result<()> {
        self.0.lock().unwrap().push(SettlementSnapshot {
            prompt_tokens: ctx.prompt_tokens,
            completion_tokens: ctx.completion_tokens,
            has_error: ctx.error.is_some(),
        });
        Ok(())
    }
}

struct ProviderCapturingRecorder(Arc<std::sync::Mutex<Vec<(String, String)>>>);

#[async_trait]
impl SettlementRecorder for ProviderCapturingRecorder {
    async fn record(&self, ctx: &mut SettlementContext) -> Result<()> {
        let value = (ctx.provider_id.clone(), ctx.model_id.clone());
        match self.0.lock() {
            Ok(mut captured) => captured.push(value),
            Err(poisoned) => poisoned.into_inner().push(value),
        }
        Ok(())
    }
}

struct PresetAwareRoutingTable;

#[async_trait]
impl RoutingTable for PresetAwareRoutingTable {
    async fn resolve_model(&self, model: &str) -> Result<ModelResolution> {
        if model == "@adaptive:preferred" {
            Ok(ModelResolution {
                clean_model: "strong-model".into(),
                prefs: RoutingPrefs {
                    only: vec!["preferred-provider".into()],
                    ..RoutingPrefs::default()
                },
                overrides: PromptOverrides::default(),
                policy: Some("coding".into()),
            })
        } else {
            Ok(ModelResolution::passthrough(model))
        }
    }

    async fn route_chain(
        &self,
        model: &str,
        prefs: &RoutingPrefs,
        _caller: &CallerContext,
    ) -> Result<Vec<RoutingTarget>> {
        let provider = prefs
            .only
            .first()
            .map(String::as_str)
            .unwrap_or("default-provider");
        let mut selected = target(provider);
        selected.service_id = model.to_string();
        Ok(vec![selected])
    }

    fn list_models(&self) -> Vec<ModelInfo> {
        Vec::new()
    }

    fn model_info(&self, _model: &str) -> Option<ModelInfo> {
        None
    }

    async fn reload(&self) -> Result<()> {
        Ok(())
    }
}

struct CountingModelSelector(Arc<AtomicUsize>);

impl ModelSelector for CountingModelSelector {
    fn select(&self, policy: &str, ctx: &mut PipelineContext) -> Result<()> {
        assert_eq!(policy, "coding");
        self.0.fetch_add(1, Ordering::SeqCst);
        ctx.set_model("economy-model");
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct TimingSnapshot {
    provider_id: String,
    model_id: String,
    latency_ms: u64,
    generation_time_ms: u64,
    first_token_latency_ms: Option<u64>,
    first_token_kind: Option<timing::FirstTokenKind>,
    has_error: bool,
}

struct TimingSnapshotRecorder(Arc<std::sync::Mutex<Vec<TimingSnapshot>>>);

#[async_trait]
impl SettlementRecorder for TimingSnapshotRecorder {
    async fn record(&self, ctx: &mut SettlementContext) -> Result<()> {
        self.0.lock().unwrap().push(TimingSnapshot {
            provider_id: ctx.provider_id.clone(),
            model_id: ctx.model_id.clone(),
            latency_ms: ctx.latency_ms,
            generation_time_ms: ctx.generation_time_ms,
            first_token_latency_ms: ctx.first_token_latency_ms,
            first_token_kind: ctx.first_token_kind,
            has_error: ctx.error.is_some(),
        });
        Ok(())
    }
}

struct ContextCheckingExecutionHook {
    expected_request_id: String,
    seen: Arc<std::sync::Mutex<Vec<bool>>>,
}

#[async_trait]
impl ExecutionHook for ContextCheckingExecutionHook {
    async fn on_success(&self, _ctx: &PipelineContext, _result: &ExecutionResult) -> Result<()> {
        Ok(())
    }

    async fn on_failure(&self, ctx: &PipelineContext, _error: &BitrouterError) -> FallbackDecision {
        let context_preserved = ctx.request_id() == self.expected_request_id
            && ctx
                .headers()
                .get("x-test-context")
                .is_some_and(|value| value == "preserved")
            && ctx.has_event::<TestRouteEvent>();
        match self.seen.lock() {
            Ok(mut seen) => seen.push(context_preserved),
            Err(poisoned) => poisoned.into_inner().push(context_preserved),
        }
        FallbackDecision::TryNext
    }
}

struct HopEventRecorder(Arc<std::sync::Mutex<Vec<String>>>);

#[async_trait]
impl ObserveHook for HopEventRecorder {
    async fn after_phase(&self, _phase: Phase, _ctx: &PipelineContext) {}

    async fn on_hop_start(&self, _ctx: &PipelineContext, target: &RoutingTarget) {
        self.0
            .lock()
            .unwrap()
            .push(format!("start:{}", target.provider_name));
    }

    async fn on_hop_end(
        &self,
        _ctx: &PipelineContext,
        target: &RoutingTarget,
        outcome: HopOutcome<'_>,
    ) {
        let outcome = match outcome {
            HopOutcome::Generated(_) => "generated",
            HopOutcome::StreamStarted => "stream_started",
            HopOutcome::Failed(_) => "failed",
        };
        self.0
            .lock()
            .unwrap()
            .push(format!("end:{}:{outcome}", target.provider_name));
    }

    fn stream_interest(&self) -> StreamInterest {
        StreamInterest::none()
    }

    async fn on_stream_part(&self, _ctx: &StreamContext, _part: &StreamPart) {}

    async fn on_request_end(&self, _ctx: &PipelineContext, outcome: &RequestOutcome) {
        let outcome = match outcome {
            RequestOutcome::Completed => "completed",
            RequestOutcome::Failed(_) => "failed",
            RequestOutcome::ClientDisconnected => "disconnected",
        };
        self.0.lock().unwrap().push(format!("request:{outcome}"));
    }
}

struct DelayPreHook(std::time::Duration);

#[async_trait]
impl PreRequestHook for DelayPreHook {
    async fn check(&self, _ctx: &mut PipelineContext) -> Result<HookDecision> {
        tokio::time::sleep(self.0).await;
        Ok(HookDecision::Allow)
    }
}

struct StreamErrorExecutor {
    error: BitrouterError,
}

#[async_trait]
impl Executor for StreamErrorExecutor {
    async fn execute(
        &self,
        _target: &RoutingTarget,
        _prompt: &Prompt,
        _ctx: &PipelineContext,
    ) -> Result<ExecutionResult> {
        Err(self.error.clone())
    }

    async fn execute_stream(
        &self,
        _target: &RoutingTarget,
        _prompt: &Prompt,
        _ctx: &PipelineContext,
    ) -> Result<StreamPartStream> {
        Ok(Box::pin(futures::stream::iter(vec![Err(self
            .error
            .clone())])))
    }
}

/// A `StreamHook` that records `on_stream_end` outcomes and can rewrite / abort.
struct ScriptedStreamHook {
    interest: StreamInterest,
    mode: StreamMode,
    ended_with: Arc<std::sync::Mutex<Vec<String>>>,
}
#[derive(Clone, Copy)]
enum StreamMode {
    Pass,
    UppercaseText,
    AbortOnText,
}
#[async_trait]
impl StreamHook for ScriptedStreamHook {
    fn interest(&self) -> StreamInterest {
        self.interest
    }
    async fn on_part(&self, _ctx: &mut StreamContext, part: StreamPart) -> Result<StreamAction> {
        match self.mode {
            StreamMode::Pass => Ok(StreamAction::Pass),
            StreamMode::UppercaseText => match part {
                StreamPart::TextDelta { text } => {
                    Ok(StreamAction::Replace(vec![StreamPart::TextDelta {
                        text: text.to_uppercase(),
                    }]))
                }
                _ => Ok(StreamAction::Pass),
            },
            StreamMode::AbortOnText => match part {
                StreamPart::TextDelta { .. } => {
                    Ok(StreamAction::Abort(BitrouterError::bad_request("blocked")))
                }
                _ => Ok(StreamAction::Pass),
            },
        }
    }
    async fn on_stream_end(&self, _ctx: &mut StreamContext, outcome: &StreamOutcome) -> Result<()> {
        let label = match outcome {
            StreamOutcome::Completed => "completed",
            StreamOutcome::ClientDisconnected => "disconnected",
            StreamOutcome::Aborted(_) => "aborted",
            StreamOutcome::UpstreamError(_) => "upstream_error",
        };
        self.ended_with.lock().unwrap().push(label.to_string());
        Ok(())
    }
}

/// An `ObserveHook` whose every method panics — used to prove the pipeline
/// swallows observe-hook failures.
struct PanicObserveHook;
#[async_trait]
impl ObserveHook for PanicObserveHook {
    async fn after_phase(&self, _phase: Phase, _ctx: &PipelineContext) {
        panic!("observe hook panic in after_phase");
    }
    async fn on_stream_part(&self, _ctx: &StreamContext, _part: &StreamPart) {
        panic!("observe hook panic in on_stream_part");
    }
    async fn on_request_end(&self, _ctx: &PipelineContext, _outcome: &RequestOutcome) {
        panic!("observe hook panic in on_request_end");
    }
}

struct OutcomeRecordingObserveHook(Arc<std::sync::Mutex<Vec<&'static str>>>);

#[async_trait]
impl ObserveHook for OutcomeRecordingObserveHook {
    async fn after_phase(&self, _phase: Phase, _ctx: &PipelineContext) {}

    async fn on_stream_part(&self, _ctx: &StreamContext, _part: &StreamPart) {}

    async fn on_request_end(&self, _ctx: &PipelineContext, outcome: &RequestOutcome) {
        let label = match outcome {
            RequestOutcome::Completed => "completed",
            RequestOutcome::Failed(_) => "failed",
            RequestOutcome::ClientDisconnected => "disconnected",
        };
        self.0.lock().unwrap().push(label);
    }
}

fn pipeline_with(
    rt: Arc<StaticRoutingTable>,
    executor: Arc<dyn Executor>,
    configure: impl FnOnce(&mut PipelineBuilder),
) -> Arc<Pipeline> {
    let mut b = PipelineBuilder::new();
    b.routing_table(rt).executor(executor);
    configure(&mut b);
    Arc::new(b.build().expect("pipeline builds"))
}

struct RetryUpstreamRequestErrors;

impl FallbackPolicy for RetryUpstreamRequestErrors {
    fn classify(&self, error: &BitrouterError, _attempted: &RoutingTarget) -> FallbackDecision {
        match error {
            BitrouterError::UpstreamBadRequest { .. } | BitrouterError::Upstream { .. } => {
                FallbackDecision::TryNext
            }
            other => FallbackDecision::Fail(other.clone()),
        }
    }
}

// ===== tests =====

#[tokio::test]
async fn full_pipeline_runs_all_four_stages() {
    let recorded = Arc::new(AtomicUsize::new(0));

    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::always_text("hello")),
        |b| {
            b.pre_request_hook(AllowHook)
                .route_hook(EmitRouteHook)
                .settlement_recorder(CountingRecorder(recorded.clone()));
        },
    );

    let resp = pipeline.execute(request()).await.expect("request succeeds");
    assert_eq!(resp.result.content.len(), 1);
    assert_eq!(recorded.load(Ordering::SeqCst), 1, "recorder ran");
}

#[tokio::test]
async fn policy_selection_is_preset_scoped_and_preserves_routing_preferences() {
    let selected = Arc::new(AtomicUsize::new(0));
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(Arc::new(PresetAwareRoutingTable))
        .executor(Arc::new(MockExecutor::new(vec![
            MockResponse::Generate(gen_result(Vec::new())),
            MockResponse::Generate(gen_result(Vec::new())),
        ])))
        .model_selector(Arc::new(CountingModelSelector(selected.clone())))
        .settlement_recorder(ProviderCapturingRecorder(captured.clone()));
    let pipeline = builder.build().unwrap();

    pipeline
        .execute(request_for_model("@adaptive:preferred"))
        .await
        .unwrap();
    pipeline
        .execute(request_for_model("strong-model"))
        .await
        .unwrap();

    assert_eq!(selected.load(Ordering::SeqCst), 1);
    assert_eq!(
        captured.lock().unwrap().as_slice(),
        &[
            ("preferred-provider".into(), "economy-model".into()),
            ("default-provider".into(), "strong-model".into()),
        ]
    );
}

#[tokio::test]
async fn streaming_preflight_error_runs_settlement_and_observe_end() {
    let settlements = Arc::new(std::sync::Mutex::new(Vec::new()));
    let outcomes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::new(vec![MockResponse::Error(
            BitrouterError::UpstreamRateLimited {
                retry_after: Some(3),
            },
        )])),
        |builder| {
            builder
                .settlement_recorder(SettlementSnapshotRecorder(settlements.clone()))
                .observe_hook(OutcomeRecordingObserveHook(outcomes.clone()));
        },
    );

    let error = match pipeline.clone().execute_stream(stream_request()).await {
        Ok(_) => panic!("preflight rate limit must fail before opening a stream"),
        Err(error) => error,
    };
    assert!(matches!(error, BitrouterError::UpstreamRateLimited { .. }));
    assert_eq!(
        settlements.lock().unwrap().as_slice(),
        &[SettlementSnapshot {
            prompt_tokens: 0,
            completion_tokens: 0,
            has_error: true,
        }]
    );
    assert_eq!(outcomes.lock().unwrap().as_slice(), &["failed"]);
}

#[tokio::test]
async fn pre_request_deny_stops_pipeline() {
    let reached_route = Arc::new(AtomicUsize::new(0));
    let recorded = Arc::new(AtomicUsize::new(0));

    // A route hook would bump `reached_route`; a denied request must never run it.
    struct CountingRouteHook(Arc<AtomicUsize>);
    #[async_trait]
    impl RouteHook for CountingRouteHook {
        async fn resolve(
            &self,
            _chain: &mut Vec<RoutingTarget>,
            _ctx: &mut PipelineContext,
        ) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::always_text("x")),
        |b| {
            b.pre_request_hook(DenyHook)
                .route_hook(CountingRouteHook(reached_route.clone()))
                .settlement_recorder(CountingRecorder(recorded.clone()));
        },
    );

    let err = pipeline.execute(request()).await.unwrap_err();
    assert_eq!(err.status(), 401);
    assert_eq!(
        reached_route.load(Ordering::SeqCst),
        0,
        "route stage not reached after deny"
    );
}

#[tokio::test]
async fn settlement_recorders_run_in_registration_order() {
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    struct LabelledRecorder {
        label: &'static str,
        log: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }
    #[async_trait]
    impl SettlementRecorder for LabelledRecorder {
        async fn record(&self, _ctx: &mut SettlementContext) -> Result<()> {
            self.log.lock().unwrap().push(self.label);
            Ok(())
        }
    }

    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::always_text("x")),
        |b| {
            b.settlement_recorder(LabelledRecorder {
                label: "first",
                log: log.clone(),
            })
            .settlement_recorder(LabelledRecorder {
                label: "second",
                log: log.clone(),
            });
        },
    );

    pipeline.execute(request()).await.expect("ok");

    // Both recorders ran, in registration order.
    assert_eq!(*log.lock().unwrap(), vec!["first", "second"]);
}

#[tokio::test]
async fn settlement_recorder_runs_even_on_failure() {
    let recorded = Arc::new(AtomicUsize::new(0));

    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::new(vec![MockResponse::Error(
            BitrouterError::Upstream {
                status: 500,
                message: "boom".into(),
            },
        )])),
        |b| {
            b.settlement_recorder(CountingRecorder(recorded.clone()));
        },
    );

    let err = pipeline.execute(request()).await.unwrap_err();
    // Exhausting the only temporarily unavailable route is a 503.
    assert_eq!(err.status(), 503);
    assert_eq!(
        recorded.load(Ordering::SeqCst),
        1,
        "recorder runs for failed requests too"
    );
}

#[tokio::test]
async fn fallback_tries_next_on_5xx_then_succeeds() {
    let pipeline = pipeline_with(
        routing_table(&["a-provider", "b-provider"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Error(BitrouterError::Upstream {
                status: 503,
                message: "down".into(),
            }),
            MockResponse::Generate(GenerateResult {
                content: vec![Content::Text {
                    text: "from b".into(),
                    provider_metadata: Default::default(),
                }],
                usage: None,
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_details: None,
                provider_metadata: Default::default(),
            }),
        ])),
        |_b| {},
    );

    let resp = pipeline.execute(request()).await.expect("falls back to b");
    assert_eq!(
        resp.result.content,
        vec![Content::Text {
            text: "from b".into(),
            provider_metadata: Default::default(),
        }]
    );
}

#[tokio::test]
async fn fallback_tries_next_on_upstream_429_then_succeeds() {
    let pipeline = pipeline_with(
        routing_table(&["a-provider", "b-provider"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Error(BitrouterError::UpstreamRateLimited {
                retry_after: Some(30),
            }),
            MockResponse::Generate(GenerateResult {
                content: vec![Content::Text {
                    text: "from b".into(),
                    provider_metadata: Default::default(),
                }],
                usage: None,
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_details: None,
                provider_metadata: Default::default(),
            }),
        ])),
        |_b| {},
    );

    let response = pipeline.execute(request()).await.unwrap();
    assert_eq!(
        response.result.content,
        vec![Content::Text {
            text: "from b".into(),
            provider_metadata: Default::default(),
        }]
    );
}

#[tokio::test]
async fn exhausted_upstream_429s_use_the_earliest_retry_after() {
    let pipeline = pipeline_with(
        routing_table(&["a-provider", "b-provider", "c-provider"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Error(BitrouterError::UpstreamRateLimited {
                retry_after: Some(30),
            }),
            MockResponse::Error(BitrouterError::UpstreamRateLimited { retry_after: None }),
            MockResponse::Error(BitrouterError::UpstreamRateLimited {
                retry_after: Some(12),
            }),
        ])),
        |_b| {},
    );

    assert!(matches!(
        pipeline.execute(request()).await.unwrap_err(),
        BitrouterError::UpstreamRateLimited {
            retry_after: Some(12)
        }
    ));
}

#[tokio::test]
async fn default_fallback_stops_on_upstream_bad_request() {
    let pipeline = pipeline_with(
        routing_table(&["a-provider", "b-provider"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Error(BitrouterError::UpstreamBadRequest {
                error: serde_json::json!("provider rejected max_tokens"),
            }),
            MockResponse::Generate(gen_result(Vec::new())),
        ])),
        |_builder| {},
    );

    assert!(matches!(
        pipeline.execute(request()).await.unwrap_err(),
        BitrouterError::UpstreamBadRequest { .. }
    ));
}

#[tokio::test]
async fn custom_fallback_retries_upstream_bad_request_then_succeeds() {
    let pipeline = pipeline_with(
        routing_table(&["a-provider", "b-provider"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Error(BitrouterError::UpstreamBadRequest {
                error: serde_json::json!("provider rejected temperature"),
            }),
            MockResponse::Generate(gen_result(vec![Content::Text {
                text: "from b".into(),
                provider_metadata: Default::default(),
            }])),
        ])),
        |builder| {
            builder.fallback_policy(Arc::new(RetryUpstreamRequestErrors));
        },
    );

    let response = pipeline.execute(request()).await.unwrap();
    assert_eq!(
        response.result.content,
        vec![Content::Text {
            text: "from b".into(),
            provider_metadata: Default::default(),
        }]
    );
}

#[tokio::test]
async fn exhausted_upstream_bad_requests_preserve_the_first_diagnostic() {
    let pipeline = pipeline_with(
        routing_table(&["a-provider", "b-provider"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Error(BitrouterError::UpstreamBadRequest {
                error: serde_json::json!("first diagnostic"),
            }),
            MockResponse::Error(BitrouterError::UpstreamBadRequest {
                error: serde_json::json!("second diagnostic"),
            }),
        ])),
        |builder| {
            builder.fallback_policy(Arc::new(RetryUpstreamRequestErrors));
        },
    );

    match pipeline.execute(request()).await.unwrap_err() {
        BitrouterError::UpstreamBadRequest { error } => {
            assert_eq!(error, serde_json::json!("first diagnostic"));
        }
        other => panic!("expected UpstreamBadRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn exhausted_streaming_upstream_bad_requests_preserve_the_first_diagnostic() {
    let pipeline = pipeline_with(
        routing_table(&["a-provider", "b-provider"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Error(BitrouterError::UpstreamBadRequest {
                error: serde_json::json!("first streaming diagnostic"),
            }),
            MockResponse::Error(BitrouterError::UpstreamBadRequest {
                error: serde_json::json!("second streaming diagnostic"),
            }),
        ])),
        |builder| {
            builder.fallback_policy(Arc::new(RetryUpstreamRequestErrors));
        },
    );

    let error = match pipeline.clone().execute_stream(stream_request()).await {
        Ok(_) => panic!("bad requests must fail before opening a stream"),
        Err(error) => error,
    };
    match error {
        BitrouterError::UpstreamBadRequest { error } => {
            assert_eq!(error, serde_json::json!("first streaming diagnostic"));
        }
        other => panic!("expected UpstreamBadRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn mixed_upstream_bad_request_and_server_error_keep_last_error_semantics() {
    let pipeline = pipeline_with(
        routing_table(&["a-provider", "b-provider"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Error(BitrouterError::UpstreamBadRequest {
                error: serde_json::json!("bad parameters"),
            }),
            MockResponse::Error(BitrouterError::Upstream {
                status: 503,
                message: "maintenance".into(),
            }),
        ])),
        |builder| {
            builder.fallback_policy(Arc::new(RetryUpstreamRequestErrors));
        },
    );

    assert!(matches!(
        pipeline.execute(request()).await.unwrap_err(),
        BitrouterError::Upstream {
            status: 503,
            message
        } if message == "maintenance"
    ));
}

#[tokio::test]
async fn mixed_retryable_upstream_failures_become_service_unavailable() {
    let pipeline = pipeline_with(
        routing_table(&["a-provider", "b-provider"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Error(BitrouterError::UpstreamRateLimited {
                retry_after: Some(10),
            }),
            MockResponse::Error(BitrouterError::Upstream {
                status: 503,
                message: "maintenance".into(),
            }),
        ])),
        |_b| {},
    );
    assert!(matches!(
        pipeline.execute(request()).await.unwrap_err(),
        BitrouterError::UpstreamUnavailable
    ));
}

#[tokio::test]
async fn local_rate_limit_does_not_fall_back() {
    let pipeline = pipeline_with(
        routing_table(&["a-provider", "b-provider"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Error(BitrouterError::RateLimited {
                retry_after: Some(5),
            }),
            MockResponse::Generate(GenerateResult {
                content: vec![],
                usage: None,
                finish_reason: None,
                response_id: None,
                stop_details: None,
                provider_metadata: Default::default(),
            }),
        ])),
        |_b| {},
    );
    assert!(matches!(
        pipeline.execute(request()).await.unwrap_err(),
        BitrouterError::RateLimited {
            retry_after: Some(5)
        }
    ));
}

#[tokio::test]
async fn fallback_tries_next_on_payment_required_then_succeeds() {
    // The account-failover path: the first target is out of credits
    // (`PaymentRequired`), routing must drop to the next account, which
    // serves the request. A single `routing_table` with two providers
    // stands in for one provider's two accounts — the pipeline only
    // sees a 2-target chain either way.
    let pipeline = pipeline_with(
        routing_table(&["account-a", "account-b"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Error(BitrouterError::PaymentRequired(
                "Insufficient balance.".into(),
            )),
            MockResponse::Generate(GenerateResult {
                content: vec![Content::Text {
                    text: "from account b".into(),
                    provider_metadata: Default::default(),
                }],
                usage: None,
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_details: None,
                provider_metadata: Default::default(),
            }),
        ])),
        |_b| {},
    );

    let resp = pipeline
        .execute(request())
        .await
        .expect("credit exhaustion on the first account falls back to the second");
    assert_eq!(
        resp.result.content,
        vec![Content::Text {
            text: "from account b".into(),
            provider_metadata: Default::default(),
        }]
    );
}

#[tokio::test]
async fn fallback_does_not_retry_on_4xx() {
    let pipeline = pipeline_with(
        routing_table(&["a-provider", "b-provider"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Error(BitrouterError::Upstream {
                status: 400,
                message: "bad".into(),
            }),
            // b would succeed, but a 400 must not fall through to it.
            MockResponse::Generate(GenerateResult {
                content: vec![Content::Text {
                    text: "b".into(),
                    provider_metadata: Default::default(),
                }],
                usage: None,
                finish_reason: None,
                response_id: None,
                stop_details: None,
                provider_metadata: Default::default(),
            }),
        ])),
        |_b| {},
    );

    let err = pipeline.execute(request()).await.unwrap_err();
    // DefaultFallbackPolicy fails fast on a 4xx, preserving the original error;
    // it must not have fallen through to provider b.
    assert!(matches!(err, BitrouterError::Upstream { status: 400, .. }));
}

/// An ExecutionHook that just *observes* failures — it never votes Fail. The
/// presence of any such hook used to silently disable `FallbackPolicy::Fail`
/// because `classify_failure` skipped the policy when `execution_hooks` was
/// non-empty; this test pins the corrected behaviour.
struct ObserveOnlyExecutionHook;
#[async_trait::async_trait]
impl crate::language_model::ExecutionHook for ObserveOnlyExecutionHook {
    async fn on_success(
        &self,
        _ctx: &crate::language_model::PipelineContext,
        _result: &crate::language_model::ExecutionResult,
    ) -> crate::Result<()> {
        Ok(())
    }
    async fn on_failure(
        &self,
        _ctx: &crate::language_model::PipelineContext,
        _error: &BitrouterError,
    ) -> crate::language_model::FallbackDecision {
        crate::language_model::FallbackDecision::TryNext
    }
}

#[tokio::test]
async fn fallback_does_not_retry_on_4xx_even_with_observe_only_execution_hook() {
    // Same scenario as `fallback_does_not_retry_on_4xx`, but with an
    // observe-only ExecutionHook registered. Before the fix, registering ANY
    // execution hook caused `classify_failure` to skip the FallbackPolicy and
    // unconditionally `TryNext`, so the 4xx would silently fall through to
    // the second provider. The fix consults both hooks AND the policy.
    let pipeline = pipeline_with(
        routing_table(&["a-provider", "b-provider"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Error(BitrouterError::Upstream {
                status: 400,
                message: "bad".into(),
            }),
            MockResponse::Generate(GenerateResult {
                content: vec![Content::Text {
                    text: "b".into(),
                    provider_metadata: Default::default(),
                }],
                usage: None,
                finish_reason: None,
                response_id: None,
                stop_details: None,
                provider_metadata: Default::default(),
            }),
        ])),
        |b| {
            b.execution_hook(ObserveOnlyExecutionHook);
        },
    );

    let err = pipeline.execute(request()).await.unwrap_err();
    assert!(
        matches!(err, BitrouterError::Upstream { status: 400, .. }),
        "observe-only ExecutionHook must NOT disable FallbackPolicy::Fail on 4xx"
    );
}

#[tokio::test]
async fn observe_hook_panic_is_swallowed() {
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::always_text("hi")),
        |b| {
            b.observe_hook(PanicObserveHook);
        },
    );

    // Despite the observe hook panicking in every method, the request succeeds.
    let resp = pipeline
        .execute(request())
        .await
        .expect("request unaffected");
    assert_eq!(resp.result.content.len(), 1);
}

// ===== StreamHook stage =====

fn stream_request() -> PipelineRequest {
    let mut req = request();
    req.prompt.stream = true;
    req
}

async fn collect_stream(
    stream: std::pin::Pin<Box<dyn futures_core::Stream<Item = Result<StreamPart>> + Send>>,
) -> Vec<Result<StreamPart>> {
    use futures::StreamExt;
    stream.collect().await
}

#[tokio::test]
async fn streamed_settlement_has_positive_canonical_timing() {
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::new(vec![MockResponse::Stream(vec![
            StreamPart::TextDelta {
                text: "hello".into(),
            },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
        ])])),
        |builder| {
            builder.settlement_recorder(TimingSnapshotRecorder(captured.clone()));
        },
    );

    let stream = pipeline
        .execute_stream(stream_request())
        .await
        .expect("stream starts");
    let parts = collect_stream(stream).await;
    assert!(parts.iter().all(Result::is_ok));

    let snapshots = captured.lock().unwrap();
    let snapshot = snapshots.first().expect("settlement snapshot");
    assert_eq!(snapshot.provider_id, "openai");
    assert_eq!(snapshot.model_id, "test-model");
    assert!(snapshot.latency_ms >= 1);
    assert!(snapshot.generation_time_ms >= 1);
    assert!(
        snapshot
            .first_token_latency_ms
            .is_some_and(|value| value >= 1)
    );
    assert_eq!(
        snapshot.first_token_kind,
        Some(timing::FirstTokenKind::Text)
    );
    assert!(!snapshot.has_error);
}

#[tokio::test]
async fn streamed_fallback_attributes_timing_to_successful_target() {
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(
        routing_table(&["first", "second"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Error(BitrouterError::UpstreamUnavailable),
            MockResponse::Stream(vec![
                StreamPart::ReasoningDelta {
                    text: "thinking".into(),
                },
                StreamPart::Finish {
                    reason: FinishReason::Stop,
                },
            ]),
        ])),
        |builder| {
            builder.settlement_recorder(TimingSnapshotRecorder(captured.clone()));
        },
    );

    let stream = pipeline
        .execute_stream(stream_request())
        .await
        .expect("fallback stream starts");
    let _parts = collect_stream(stream).await;

    let snapshots = captured.lock().unwrap();
    let snapshot = snapshots.first().expect("settlement snapshot");
    assert_eq!(snapshot.provider_id, "second");
    assert_eq!(snapshot.model_id, "test-model");
    assert!(snapshot.generation_time_ms >= 1);
    assert_eq!(
        snapshot.first_token_kind,
        Some(timing::FirstTokenKind::Reasoning)
    );
}

#[tokio::test]
async fn streamed_upstream_error_finalizes_timing_before_settlement() {
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(StreamErrorExecutor {
            error: BitrouterError::UpstreamUnavailable,
        }),
        |builder| {
            builder.settlement_recorder(TimingSnapshotRecorder(captured.clone()));
        },
    );

    let stream = pipeline
        .execute_stream(stream_request())
        .await
        .expect("stream starts before body error");
    let _parts = collect_stream(stream).await;

    let snapshots = captured.lock().unwrap();
    let snapshot = snapshots.first().expect("settlement snapshot");
    assert!(snapshot.latency_ms >= 1);
    assert!(snapshot.generation_time_ms >= 1);
    assert!(snapshot.has_error);
}

#[tokio::test]
async fn streamed_hook_abort_finalizes_timing_before_settlement() {
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let ended = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::new(vec![MockResponse::Stream(vec![
            StreamPart::ToolCallDelta {
                id: "call-1".into(),
                name: Some("lookup".into()),
                arguments: "{}".into(),
            },
            StreamPart::TextDelta {
                text: "blocked".into(),
            },
        ])])),
        |builder| {
            builder
                .stream_hook(ScriptedStreamHook {
                    interest: StreamInterest::all(),
                    mode: StreamMode::AbortOnText,
                    ended_with: ended.clone(),
                })
                .settlement_recorder(TimingSnapshotRecorder(captured.clone()));
        },
    );

    let stream = pipeline
        .execute_stream(stream_request())
        .await
        .expect("stream starts");
    let _parts = collect_stream(stream).await;

    let snapshots = captured.lock().unwrap();
    let snapshot = snapshots.first().expect("settlement snapshot");
    assert!(snapshot.latency_ms >= 1);
    assert!(snapshot.generation_time_ms >= 1);
    assert_eq!(
        snapshot.first_token_kind,
        Some(timing::FirstTokenKind::Tool)
    );
    assert!(snapshot.has_error);
}

#[tokio::test]
async fn dropped_stream_finalizes_timing_before_detached_settlement() {
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::new(vec![MockResponse::Stream(vec![
            StreamPart::TextDelta { text: "hi".into() },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
        ])])),
        |builder| {
            builder.settlement_recorder(TimingSnapshotRecorder(captured.clone()));
        },
    );

    let stream = pipeline
        .clone()
        .execute_stream(stream_request())
        .await
        .expect("stream starts");
    drop(stream);
    pipeline.drain_pending_settlements().await;

    let snapshots = captured.lock().unwrap();
    let snapshot = snapshots.first().expect("detached settlement snapshot");
    assert!(snapshot.latency_ms >= 1);
    assert!(snapshot.generation_time_ms >= 1);
    assert!(!snapshot.has_error);
}

#[tokio::test]
async fn non_stream_latency_includes_pre_request_pipeline_time() {
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::always_text("hello")),
        |builder| {
            builder
                .pre_request_hook(DelayPreHook(std::time::Duration::from_millis(20)))
                .settlement_recorder(TimingSnapshotRecorder(captured.clone()));
        },
    );

    pipeline.execute(request()).await.expect("request succeeds");

    let snapshots = captured.lock().unwrap();
    let snapshot = snapshots.first().expect("settlement snapshot");
    assert!(snapshot.latency_ms >= 20);
    assert!(snapshot.generation_time_ms >= 1);
}

#[tokio::test]
async fn stream_on_stream_end_fires_on_completion() {
    let ended = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::new(vec![MockResponse::Stream(vec![
            StreamPart::TextDelta { text: "hi".into() },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
        ])])),
        |b| {
            b.stream_hook(ScriptedStreamHook {
                interest: StreamInterest::all(),
                mode: StreamMode::Pass,
                ended_with: ended.clone(),
            });
        },
    );

    let stream = pipeline.execute_stream(stream_request()).await.expect("ok");
    let parts = collect_stream(stream).await;
    assert!(parts.iter().all(|p| p.is_ok()));
    assert_eq!(*ended.lock().unwrap(), vec!["completed"]);
}

#[tokio::test]
async fn drain_awaits_pending_disconnect_settlements() {
    // The client drops the stream before it completes — the guard's Drop
    // detaches a settlement task onto the pipeline's JoinSet. drain_pending_
    // settlements must await it (otherwise SIGTERM could lose the receipt).
    let ended = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::new(vec![MockResponse::Stream(vec![
            StreamPart::TextDelta { text: "hi".into() },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
        ])])),
        |b| {
            b.stream_hook(ScriptedStreamHook {
                interest: StreamInterest::all(),
                mode: StreamMode::Pass,
                ended_with: ended.clone(),
            });
        },
    );

    let stream = pipeline
        .clone()
        .execute_stream(stream_request())
        .await
        .expect("ok");
    // Drop the stream WITHOUT polling to completion → Drop runs, detaches a
    // settlement task with the ClientDisconnected outcome.
    drop(stream);

    let drained = pipeline.drain_pending_settlements().await;
    assert!(
        drained >= 1,
        "drain must have awaited at least the one disconnect-settlement task; got {drained}"
    );
    assert_eq!(
        *ended.lock().unwrap(),
        vec!["disconnected"],
        "the settlement task ran and the StreamHook saw ClientDisconnected"
    );
}

#[tokio::test]
async fn early_stream_drop_runs_every_settlement_recorder() {
    struct LabelledRecorder {
        label: &'static str,
        recorded: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl SettlementRecorder for LabelledRecorder {
        async fn record(&self, _ctx: &mut SettlementContext) -> Result<()> {
            tokio::task::yield_now().await;
            self.recorded.lock().unwrap().push(self.label);
            Ok(())
        }
    }

    let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::new(vec![MockResponse::Stream(vec![
            StreamPart::TextDelta { text: "hi".into() },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
        ])])),
        |builder| {
            builder
                .settlement_recorder(LabelledRecorder {
                    label: "adequacy",
                    recorded: recorded.clone(),
                })
                .settlement_recorder(LabelledRecorder {
                    label: "metering",
                    recorded: recorded.clone(),
                });
        },
    );

    let mut stream = pipeline
        .clone()
        .execute_stream(stream_request())
        .await
        .expect("stream starts");
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.unwrap().is_ok());
    drop(stream);

    pipeline.drain_pending_settlements().await;
    assert_eq!(
        recorded.lock().unwrap().as_slice(),
        &["adequacy", "metering"]
    );
}

#[tokio::test]
async fn disconnect_during_inline_settlement_does_not_cancel_remaining_recorders() {
    struct BlockingRecorder {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        recorded: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl SettlementRecorder for BlockingRecorder {
        async fn record(&self, _ctx: &mut SettlementContext) -> Result<()> {
            self.recorded.lock().unwrap().push("first-started");
            self.started.notify_one();
            self.release.notified().await;
            self.recorded.lock().unwrap().push("first-finished");
            Ok(())
        }
    }

    struct FinalRecorder(Arc<std::sync::Mutex<Vec<&'static str>>>);

    #[async_trait]
    impl SettlementRecorder for FinalRecorder {
        async fn record(&self, _ctx: &mut SettlementContext) -> Result<()> {
            self.0.lock().unwrap().push("second");
            Ok(())
        }
    }

    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::new(vec![MockResponse::Stream(vec![
            StreamPart::TextDelta { text: "hi".into() },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
        ])])),
        |builder| {
            builder
                .settlement_recorder(BlockingRecorder {
                    started: started.clone(),
                    release: release.clone(),
                    recorded: recorded.clone(),
                })
                .settlement_recorder(FinalRecorder(recorded.clone()));
        },
    );

    let mut stream = pipeline
        .clone()
        .execute_stream(stream_request())
        .await
        .expect("stream starts");
    assert!(stream.next().await.unwrap().is_ok());

    // The terminal part settles before it is yielded. Poll it on a separate
    // task so we can cancel the response-body future while settlement blocks.
    let poller = tokio::spawn(async move { stream.next().await });
    tokio::time::timeout(std::time::Duration::from_secs(1), started.notified())
        .await
        .expect("first settlement recorder should start");
    poller.abort();
    tokio::time::timeout(std::time::Duration::from_secs(1), poller)
        .await
        .expect("stream poller should abort promptly")
        .expect_err("stream poller should be cancelled");
    // Preserve a permit if the detached finalizer has not polled `notified()`
    // yet. `notify_waiters()` would lose the wake-up in that scheduling gap.
    release.notify_one();

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        pipeline.drain_pending_settlements(),
    )
    .await
    .expect("detached settlement should finish after release");
    assert_eq!(
        recorded.lock().unwrap().as_slice(),
        &["first-started", "first-finished", "second"]
    );
}

#[tokio::test]
async fn dropped_stream_still_fires_request_end_observers() {
    let observed_end = Arc::new(AtomicUsize::new(0));
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::new(vec![MockResponse::Stream(vec![
            StreamPart::TextDelta { text: "hi".into() },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
        ])])),
        |b| {
            b.observe_hook(RequestEndCountingObserver(observed_end.clone()));
        },
    );

    let stream = pipeline
        .clone()
        .execute_stream(stream_request())
        .await
        .expect("ok");
    drop(stream);

    let drained = pipeline.drain_pending_settlements().await;
    assert!(
        drained >= 1,
        "stream drop must detach a settlement task that can be drained"
    );
    assert_eq!(
        observed_end.load(Ordering::SeqCst),
        1,
        "request-end observers must run for disconnected streams"
    );
}

#[tokio::test]
async fn disconnect_before_usage_bills_estimated_output() {
    // v0 #463 / cloud #251 audit P0: if the client disconnects mid-stream
    // before the upstream `Usage` frame arrives, the request must still
    // bill — otherwise a hostile client drains a long generation, hangs up
    // just before the trailing usage chunk, and pays $0.
    let captured: Arc<std::sync::Mutex<Vec<(u64, u64)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::new(vec![MockResponse::Stream(vec![
            // ~30 chars of text delta — no Usage / Finish part.
            StreamPart::TextDelta {
                text: "the quick brown fox jumps over.".into(),
            },
        ])])),
        |b| {
            b.settlement_recorder(UsageCapturingRecorder(captured.clone()));
        },
    );

    let mut stream = pipeline
        .clone()
        .execute_stream(stream_request())
        .await
        .expect("ok");
    // Simulate: the client received the text delta, then disconnected
    // *before* the upstream's terminal usage / finish chunk would arrive.
    let first = stream.next().await.expect("at least one part");
    assert!(first.is_ok(), "the text delta surfaced cleanly");
    drop(stream);

    let drained = pipeline.drain_pending_settlements().await;
    assert!(
        drained >= 1,
        "settlement task must have been detached + drained"
    );

    let entries = captured.lock().unwrap().clone();
    assert_eq!(entries.len(), 1, "exactly one settlement recorded");
    let (prompt, completion) = entries[0];
    // Prompt tokens are now seeded into the `StreamContext` and billed on
    // disconnect even though the upstream usage frame never arrived: the
    // request prompt is `"hi"` (2 chars), so ceil(2/4) = 1 token.
    assert_eq!(prompt, 1, "prompt-token estimate billed on disconnect");
    assert!(
        completion >= 1,
        "completion-token estimate must be non-zero ({completion}); ~31 chars / 4 ≈ 8 tokens"
    );
}

#[tokio::test]
async fn upstream_stream_error_is_not_reclassified_as_client_disconnect_when_dropped() {
    let ended = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured: Arc<std::sync::Mutex<Vec<SettlementSnapshot>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let upstream_error = BitrouterError::Upstream {
        status: 401,
        message: "upstream auth failed".into(),
    };
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(StreamErrorExecutor {
            error: upstream_error,
        }),
        |b| {
            b.stream_hook(ScriptedStreamHook {
                interest: StreamInterest::all(),
                mode: StreamMode::Pass,
                ended_with: ended.clone(),
            })
            .settlement_recorder(SettlementSnapshotRecorder(captured.clone()));
        },
    );

    let mut stream = pipeline
        .clone()
        .execute_stream(stream_request())
        .await
        .expect("stream starts");
    let first = stream.next().await.expect("upstream error item");
    assert!(first.is_err(), "the upstream error surfaced to the client");

    drop(stream);
    let _ = pipeline.drain_pending_settlements().await;

    assert_eq!(
        *ended.lock().unwrap(),
        vec!["upstream_error"],
        "dropping after receiving the error must not overwrite the terminal outcome"
    );
    assert_eq!(
        captured.lock().unwrap().as_slice(),
        &[SettlementSnapshot {
            prompt_tokens: 0,
            completion_tokens: 0,
            has_error: true,
        }],
        "failed streams without usage must settle as failed and unbillable"
    );
}

#[tokio::test]
async fn stream_on_stream_end_fires_on_upstream_error() {
    let ended = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::new(vec![MockResponse::Stream(vec![])])),
        |b| {
            b.stream_hook(ScriptedStreamHook {
                interest: StreamInterest::all(),
                mode: StreamMode::Pass,
                ended_with: ended.clone(),
            });
        },
    );
    // empty stream => Completed (clean end with no parts). Use a hook-driven
    // abort to exercise a non-Completed path instead.
    let stream = pipeline.execute_stream(stream_request()).await.expect("ok");
    let _ = collect_stream(stream).await;
    assert_eq!(*ended.lock().unwrap(), vec!["completed"]);
}

#[tokio::test]
async fn stream_abort_fires_on_stream_end_with_aborted() {
    let ended = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::new(vec![MockResponse::Stream(vec![
            StreamPart::TextDelta {
                text: "secret".into(),
            },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
        ])])),
        |b| {
            b.stream_hook(ScriptedStreamHook {
                interest: StreamInterest::all(),
                mode: StreamMode::AbortOnText,
                ended_with: ended.clone(),
            });
        },
    );

    let stream = pipeline.execute_stream(stream_request()).await.expect("ok");
    let parts = collect_stream(stream).await;
    // last yielded item is the abort error
    assert!(parts.last().unwrap().is_err());
    assert_eq!(*ended.lock().unwrap(), vec!["aborted"]);
}

#[tokio::test]
async fn stream_interest_filter_skips_uninterested_hook() {
    // A hook interested only in Usage must not see TextDelta. We prove it by
    // using UppercaseText mode but Usage-only interest: text passes through
    // unchanged.
    let ended = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::new(vec![MockResponse::Stream(vec![
            StreamPart::TextDelta {
                text: "lower".into(),
            },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
        ])])),
        |b| {
            b.stream_hook(ScriptedStreamHook {
                interest: StreamInterest::none().with_usage(),
                mode: StreamMode::UppercaseText,
                ended_with: ended.clone(),
            });
        },
    );

    let stream = pipeline.execute_stream(stream_request()).await.expect("ok");
    let parts = collect_stream(stream).await;
    let texts: Vec<String> = parts
        .into_iter()
        .filter_map(|p| match p.ok()? {
            StreamPart::TextDelta { text } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["lower"], "text untouched by Usage-only hook");
}

#[tokio::test]
async fn stream_hooks_chain_rewrites() {
    // Hook A uppercases text; hook B (Pass) sees A's rewritten output.
    let ended_a = Arc::new(std::sync::Mutex::new(Vec::new()));
    let ended_b = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::new(vec![MockResponse::Stream(vec![
            StreamPart::TextDelta {
                text: "hello".into(),
            },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
        ])])),
        |b| {
            b.stream_hook(ScriptedStreamHook {
                interest: StreamInterest::all(),
                mode: StreamMode::UppercaseText,
                ended_with: ended_a.clone(),
            })
            .stream_hook(ScriptedStreamHook {
                interest: StreamInterest::all(),
                mode: StreamMode::Pass,
                ended_with: ended_b.clone(),
            });
        },
    );

    let stream = pipeline.execute_stream(stream_request()).await.expect("ok");
    let parts = collect_stream(stream).await;
    let texts: Vec<String> = parts
        .into_iter()
        .filter_map(|p| match p.ok()? {
            StreamPart::TextDelta { text } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["HELLO"]);
    // both hooks saw the stream end
    assert_eq!(*ended_a.lock().unwrap(), vec!["completed"]);
    assert_eq!(*ended_b.lock().unwrap(), vec!["completed"]);
}

#[tokio::test]
async fn route_hook_event_is_visible_downstream() {
    // EmitRouteHook emits TestRouteEvent in Stage 2; a settlement recorder in
    // Stage 4 must be able to see it via the carried-over event bus.
    struct EventAssertRecorder;
    #[async_trait]
    impl SettlementRecorder for EventAssertRecorder {
        async fn record(&self, ctx: &mut SettlementContext) -> Result<()> {
            assert!(
                ctx.has_event::<TestRouteEvent>(),
                "route-stage event reached settlement"
            );
            Ok(())
        }
    }

    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::always_text("x")),
        |b| {
            b.route_hook(EmitRouteHook)
                .settlement_recorder(EventAssertRecorder);
        },
    );
    pipeline.execute(request()).await.expect("ok");
}

#[tokio::test]
async fn settlement_recorder_can_emit_event_for_later_recorders() {
    // Option A (#529): `record(&mut ctx)` lets a recorder emit events that a
    // later-registered recorder — and, via `absorb_settlement`, downstream
    // observe hooks — can read.
    #[derive(Serialize)]
    struct SettleEmit {
        tag: u32,
    }
    impl PipelineEvent for SettleEmit {
        fn event_name(&self) -> &'static str {
            "test.settle_emit"
        }
    }

    struct EmitRecorder;
    #[async_trait]
    impl SettlementRecorder for EmitRecorder {
        async fn record(&self, ctx: &mut SettlementContext) -> Result<()> {
            ctx.emit(SettleEmit { tag: 42 });
            Ok(())
        }
    }

    let seen = Arc::new(AtomicUsize::new(0));
    struct AssertRecorder(Arc<AtomicUsize>);
    #[async_trait]
    impl SettlementRecorder for AssertRecorder {
        async fn record(&self, ctx: &mut SettlementContext) -> Result<()> {
            if let Some(e) = ctx.get_event::<SettleEmit>() {
                self.0.store(e.tag as usize, Ordering::SeqCst);
            }
            Ok(())
        }
    }

    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::always_text("x")),
        |b| {
            b.settlement_recorder(EmitRecorder)
                .settlement_recorder(AssertRecorder(seen.clone()));
        },
    );
    pipeline.execute(request()).await.expect("ok");
    assert_eq!(
        seen.load(Ordering::SeqCst),
        42,
        "a later settlement recorder sees the event emitted by an earlier one"
    );
}

#[tokio::test]
async fn counting_pre_hook_runs_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::always_text("x")),
        |b| {
            b.pre_request_hook(CountingPreHook(calls.clone()));
        },
    );
    pipeline.execute(request()).await.expect("ok");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn executor_rejects_response_format_on_unsupported_outbound() {
    // A custom outbound adapter that doesn't override `supports_response_format`
    // (so it defaults to `false`) must cause the executor to return a 400
    // rather than silently dropping the structured-output schema upstream.
    use crate::language_model::executor::HttpExecutor;
    use crate::language_model::protocol::{
        OutboundAdapter, OutboundDispatch, StreamDecoder, Transport,
    };
    use crate::language_model::types::ResponseFormat;

    struct FakeAdapter;
    impl OutboundAdapter for FakeAdapter {
        fn protocol(&self) -> ApiProtocol {
            ApiProtocol::Custom("fake".into())
        }
        fn render_request(&self, _: &Prompt) -> Result<serde_json::Value> {
            unreachable!("gate must fire before render_request")
        }
        fn parse_response(&self, _: serde_json::Value) -> Result<GenerateResult> {
            unreachable!()
        }
        fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
            unreachable!()
        }
        // intentionally leaves `supports_response_format` at the default `false`
    }

    struct FakeTransport;
    #[async_trait]
    impl Transport for FakeTransport {
        fn protocol(&self) -> ApiProtocol {
            ApiProtocol::Custom("fake".into())
        }
        fn endpoint_url(&self, _: &RoutingTarget, _: bool) -> String {
            "http://example.invalid".into()
        }
        async fn authorise(
            &self,
            r: reqwest::Request,
            _: &RoutingTarget,
        ) -> Result<reqwest::Request> {
            Ok(r)
        }
    }

    let mut dispatch = OutboundDispatch::empty();
    dispatch.register(Arc::new(FakeAdapter), Arc::new(FakeTransport));
    let executor =
        HttpExecutor::with_dispatch(Default::default(), dispatch).expect("build executor");

    let target = RoutingTarget {
        provider_name: "fake".into(),
        service_id: "m".into(),
        api_base: "http://example.invalid".into(),
        api_key: "k".into(),
        api_protocol: ApiProtocol::Custom("fake".into()),
        chat_token_limit_field: None,
        account_label: None,
        api_key_override: None,
        api_base_override: None,
        auth_scheme: Default::default(),
    };
    let prompt = Prompt {
        model: "m".into(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, "hi")],
        tools: vec![],
        params: GenerationParams::default(),
        response_format: Some(ResponseFormat::JsonSchema {
            name: None,
            description: None,
            strict: None,
            schema: serde_json::json!({"type": "object"}),
        }),
        tool_choice: None,
        stream: false,
    };
    let req = PipelineRequest::new("m", CallerContext::new("k", "u"), prompt.clone());
    let ctx = PipelineContext::new(req);
    let err = executor.execute(&target, &prompt, &ctx).await.unwrap_err();
    match err {
        BitrouterError::BadRequest { message } => {
            assert!(
                message.contains("response_format"),
                "error must mention response_format, got: {message}"
            );
        }
        other => panic!("expected BadRequest, got {other:?}"),
    }
}

struct AuthRecoveryApplier {
    refreshes: Arc<AtomicUsize>,
    seen_rejected_auth: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl AuthApplier for AuthRecoveryApplier {
    async fn apply(
        &self,
        mut request: reqwest::Request,
        _target: &RoutingTarget,
    ) -> Result<reqwest::Request> {
        let token = if self.refreshes.load(Ordering::SeqCst) == 0 {
            "stale"
        } else {
            "fresh"
        };
        request.headers_mut().insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        Ok(request)
    }

    async fn refresh_after_unauthorized(
        &self,
        _target: &RoutingTarget,
        rejected_authorization: Option<&reqwest::header::HeaderValue>,
    ) -> Result<bool> {
        if let Some(value) = rejected_authorization.and_then(|v| v.to_str().ok()) {
            self.seen_rejected_auth
                .lock()
                .unwrap()
                .push(value.to_string());
        }
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }
}

fn spawn_auth_retry_server(
    responses: Vec<(&'static str, &'static str)>,
    seen_auths: Arc<std::sync::Mutex<Vec<String>>>,
) -> String {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for (status, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            let mut buf = [0_u8; 1024];
            let header_end = loop {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break bytes.len();
                }
                bytes.extend_from_slice(&buf[..n]);
                if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let header_text = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = header_text
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            let total_len = header_end + content_length;
            while bytes.len() < total_len {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&buf[..n]);
            }
            let request = String::from_utf8_lossy(&bytes[..header_end]);
            let auth = request
                .lines()
                .find_map(|line| line.strip_prefix("authorization: "))
                .or_else(|| {
                    request
                        .lines()
                        .find_map(|line| line.strip_prefix("Authorization: "))
                })
                .unwrap_or("")
                .to_string();
            seen_auths.lock().unwrap().push(auth);
            let content_type = if body.starts_with("data: ") {
                "text/event-stream"
            } else {
                "application/json"
            };
            write!(
                stream,
                "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    format!("http://{addr}")
}

fn auth_retry_target(api_base: String) -> RoutingTarget {
    RoutingTarget {
        provider_name: "retry-provider".into(),
        service_id: "test-model".into(),
        api_base,
        api_key: String::new(),
        api_protocol: ApiProtocol::ChatCompletions,
        chat_token_limit_field: None,
        account_label: None,
        api_key_override: None,
        api_base_override: None,
        auth_scheme: Default::default(),
    }
}

fn auth_retry_executor(
    refreshes: Arc<AtomicUsize>,
    rejected: Arc<std::sync::Mutex<Vec<String>>>,
) -> HttpExecutor {
    let mut auth = AuthAppliers::new();
    auth.register(
        "retry-provider",
        Arc::new(AuthRecoveryApplier {
            refreshes,
            seen_rejected_auth: rejected,
        }),
    );
    HttpExecutor::with_dispatch_and_auth(Default::default(), Default::default(), auth)
        .expect("executor")
}

#[tokio::test]
async fn http_executor_refreshes_auth_and_retries_non_streaming_401_once() {
    let seen_auths = Arc::new(std::sync::Mutex::new(Vec::new()));
    let api_base = spawn_auth_retry_server(
        vec![
            (
                "401 Unauthorized",
                r#"{"error":{"code":"token_revoked","message":"revoked"}}"#,
            ),
            (
                "200 OK",
                r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
            ),
        ],
        seen_auths.clone(),
    );
    let refreshes = Arc::new(AtomicUsize::new(0));
    let rejected = Arc::new(std::sync::Mutex::new(Vec::new()));
    let executor = auth_retry_executor(refreshes.clone(), rejected.clone());
    let target = auth_retry_target(api_base);
    let req = request();
    let ctx = PipelineContext::new(req.clone());

    let result = executor.execute(&target, &req.prompt, &ctx).await.unwrap();

    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(
        *seen_auths.lock().unwrap(),
        vec!["Bearer stale".to_string(), "Bearer fresh".to_string()]
    );
    assert_eq!(*rejected.lock().unwrap(), vec!["Bearer stale".to_string()]);
    match result.result.content.as_slice() {
        [Content::Text { text, .. }] => assert_eq!(text, "ok"),
        other => panic!("expected one text block, got {other:?}"),
    }
}

#[tokio::test]
async fn http_executor_refreshes_auth_and_retries_streaming_401_once() {
    let seen_auths = Arc::new(std::sync::Mutex::new(Vec::new()));
    let stream_body = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
        "data: [DONE]\n\n"
    );
    let api_base = spawn_auth_retry_server(
        vec![
            (
                "401 Unauthorized",
                r#"{"error":{"code":"token_revoked","message":"revoked"}}"#,
            ),
            ("200 OK", stream_body),
        ],
        seen_auths.clone(),
    );
    let refreshes = Arc::new(AtomicUsize::new(0));
    let rejected = Arc::new(std::sync::Mutex::new(Vec::new()));
    let executor = auth_retry_executor(refreshes.clone(), rejected.clone());
    let target = auth_retry_target(api_base);
    let req = stream_request();
    let ctx = PipelineContext::new(req.clone());

    let mut stream = executor
        .execute_stream(&target, &req.prompt, &ctx)
        .await
        .unwrap();
    let mut text = String::new();
    while let Some(part) = stream.next().await {
        if let StreamPart::TextDelta { text: delta } = part.unwrap() {
            text.push_str(&delta);
        }
    }

    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(
        *seen_auths.lock().unwrap(),
        vec!["Bearer stale".to_string(), "Bearer fresh".to_string()]
    );
    assert_eq!(*rejected.lock().unwrap(), vec!["Bearer stale".to_string()]);
    assert_eq!(text, "ok");
}

// ===== non-streaming client-disconnect billing (OpenRouter parity) =====

/// An executor whose `execute` blocks on a gate until the test releases it, so
/// the test can deterministically drop the request future *while the upstream
/// call is still in flight* — the exact shape of a mid-request client
/// disconnect.
struct GatedExecutor {
    gate: Arc<tokio::sync::Notify>,
    usage: (u64, u64),
}
#[async_trait]
impl Executor for GatedExecutor {
    async fn execute(
        &self,
        target: &RoutingTarget,
        _prompt: &Prompt,
        _ctx: &PipelineContext,
    ) -> Result<ExecutionResult> {
        self.gate.notified().await;
        let (prompt_tokens, completion_tokens) = self.usage;
        Ok(ExecutionResult {
            provider_id: target.provider_name.clone(),
            model_id: target.service_id.clone(),
            account_label: target.account_label.clone(),
            result: GenerateResult {
                content: vec![Content::Text {
                    text: "done".into(),
                    provider_metadata: Default::default(),
                }],
                usage: Some(Usage {
                    prompt_tokens,
                    completion_tokens,
                    ..Default::default()
                }),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_details: None,
                provider_metadata: Default::default(),
            },
            latency_ms: 1,
            generation_time_ms: 1,
            server_tool_calls: Vec::new(),
        })
    }
    async fn execute_stream(
        &self,
        _target: &RoutingTarget,
        _prompt: &Prompt,
        _ctx: &PipelineContext,
    ) -> Result<StreamPartStream> {
        Err(BitrouterError::internal(
            "GatedExecutor: streaming not used",
        ))
    }
}

#[tokio::test]
async fn nonstream_execute_detached_returns_full_usage_when_connected() {
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::always_text("hello")),
        |b| {
            b.settlement_recorder(UsageCapturingRecorder(captured.clone()));
        },
    );

    let resp = pipeline
        .clone()
        .execute_detached(request())
        .await
        .expect("connected request succeeds");
    assert_eq!(resp.result.content.len(), 1);
    // `always_text` reports usage (prompt=10, completion=5).
    assert_eq!(captured.lock().unwrap().clone(), vec![(10, 5)]);
}

#[tokio::test]
async fn nonstream_disconnect_still_runs_to_completion_and_bills_full() {
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let gate = Arc::new(tokio::sync::Notify::new());
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(GatedExecutor {
            gate: gate.clone(),
            usage: (7, 11),
        }),
        |b| {
            b.settlement_recorder(UsageCapturingRecorder(captured.clone()));
        },
    );

    // Start the request, then drop the handler future before it resolves —
    // exactly what axum does to the handler when the client disconnects. The
    // upstream call is gated, so the detached task cannot have finished yet.
    let mut fut = Box::pin(pipeline.clone().execute_detached(request()));
    assert!(
        futures::poll!(fut.as_mut()).is_pending(),
        "detached task spawned but gated; handler future still pending"
    );
    drop(fut); // client disconnected

    // The request must still run to completion and settle the real full usage.
    gate.notify_one();
    let drained = pipeline.drain_pending_settlements().await;
    assert!(drained >= 1, "the detached execution was awaited on drain");

    assert_eq!(
        captured.lock().unwrap().clone(),
        vec![(7, 11)],
        "non-stream disconnect bills the full upstream usage, not zero"
    );
}

// ===== server-side tool loop wiring =====

fn empty_server_tool_loop() -> Arc<server_tools::loop_controller::ServerToolLoop> {
    Arc::new(server_tools::loop_controller::ServerToolLoop::new(
        server_tools::toolset::ToolsetRegistry::new(Vec::new()),
        server_tools::config::ServerToolLoopConfig::default(),
        Arc::new(server_tools::approval::AllowAll),
    ))
}

struct SearchTool;

#[async_trait]
impl server_tools::toolset::RouterToolset for SearchTool {
    async fn list_tools(&self, _ctx: &server_tools::toolset::ToolContext) -> Result<Vec<Tool>> {
        Ok(vec![Tool::Function {
            name: "search".into(),
            description: None,
            parameters: serde_json::json!({"type": "object"}),
            strict: None,
            provider_metadata: Default::default(),
        }])
    }

    async fn call_tool(
        &self,
        _name: &str,
        _arguments: &str,
        _ctx: &server_tools::toolset::ToolContext,
    ) -> Result<ToolResultOutput> {
        Ok(ToolResultOutput::Text {
            value: "result".into(),
        })
    }

    fn owns(&self, name: &str) -> bool {
        name == "search"
    }
}

fn search_server_tool_loop() -> Arc<server_tools::loop_controller::ServerToolLoop> {
    Arc::new(server_tools::loop_controller::ServerToolLoop::new(
        server_tools::toolset::ToolsetRegistry::new(vec![Arc::new(SearchTool)]),
        server_tools::config::ServerToolLoopConfig::default(),
        Arc::new(server_tools::approval::AllowAll),
    ))
}

#[tokio::test]
async fn server_tool_streaming_preflight_respects_fail_decision() {
    let pipeline = pipeline_with(
        routing_table(&["a-provider", "b-provider"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Error(BitrouterError::bad_request("invalid request")),
            MockResponse::Stream(vec![StreamPart::Finish {
                reason: FinishReason::Stop,
            }]),
        ])),
        |builder| {
            builder.server_tool_loop(empty_server_tool_loop());
        },
    );

    let result = pipeline.execute_stream(stream_request()).await;
    assert!(matches!(result, Err(BitrouterError::BadRequest { .. })));
}

#[tokio::test]
async fn server_tool_streaming_preflight_aggregates_rate_limits() {
    let pipeline = pipeline_with(
        routing_table(&["a-provider", "b-provider"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Error(BitrouterError::UpstreamRateLimited {
                retry_after: Some(12),
            }),
            MockResponse::Error(BitrouterError::UpstreamRateLimited {
                retry_after: Some(30),
            }),
        ])),
        |builder| {
            builder.server_tool_loop(empty_server_tool_loop());
        },
    );

    let result = pipeline.execute_stream(stream_request()).await;
    assert!(matches!(
        result,
        Err(BitrouterError::UpstreamRateLimited {
            retry_after: Some(12)
        })
    ));
}

#[tokio::test]
async fn server_tool_streaming_preflight_aggregates_mixed_availability_failures() {
    let pipeline = pipeline_with(
        routing_table(&["a-provider", "b-provider"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Error(BitrouterError::UpstreamRateLimited {
                retry_after: Some(12),
            }),
            MockResponse::Error(BitrouterError::Upstream {
                status: 503,
                message: "maintenance".into(),
            }),
        ])),
        |builder| {
            builder.server_tool_loop(empty_server_tool_loop());
        },
    );

    let result = pipeline.execute_stream(stream_request()).await;
    assert!(matches!(result, Err(BitrouterError::UpstreamUnavailable)));
}

#[tokio::test]
async fn server_tool_streaming_fallback_settles_the_winning_target()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(
        routing_table(&["a-provider", "b-provider"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Error(BitrouterError::UpstreamRateLimited {
                retry_after: Some(12),
            }),
            MockResponse::Stream(vec![StreamPart::Finish {
                reason: FinishReason::Stop,
            }]),
        ])),
        |builder| {
            builder
                .server_tool_loop(empty_server_tool_loop())
                .settlement_recorder(ProviderCapturingRecorder(captured.clone()));
        },
    );

    let stream = pipeline.execute_stream(stream_request()).await?;
    let parts = collect_stream(stream).await;
    assert!(parts.into_iter().all(|part| part.is_ok()));
    let captured = match captured.lock() {
        Ok(captured) => captured.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    assert_eq!(
        captured.as_slice(),
        &[("b-provider".to_string(), "test-model".to_string())]
    );
    Ok(())
}

#[tokio::test]
async fn server_tool_streaming_fallback_preserves_request_context()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let request_id = "stable-request-id".to_string();
    let pipeline = pipeline_with(
        routing_table(&["a-provider", "b-provider"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Error(BitrouterError::UpstreamRateLimited {
                retry_after: Some(12),
            }),
            MockResponse::Stream(vec![StreamPart::Finish {
                reason: FinishReason::Stop,
            }]),
        ])),
        |builder| {
            builder
                .route_hook(EmitRouteHook)
                .execution_hook(ContextCheckingExecutionHook {
                    expected_request_id: request_id.clone(),
                    seen: seen.clone(),
                })
                .server_tool_loop(empty_server_tool_loop());
        },
    );
    let mut request = stream_request();
    request.request_id = request_id;
    request.headers.insert(
        "x-test-context",
        http::HeaderValue::from_static("preserved"),
    );

    let stream = pipeline.execute_stream(request).await?;
    let parts = collect_stream(stream).await;
    assert!(parts.into_iter().all(|part| part.is_ok()));
    let seen = match seen.lock() {
        Ok(seen) => seen.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    assert_eq!(seen.as_slice(), &[true]);
    Ok(())
}

#[tokio::test]
async fn server_tool_streaming_settles_the_final_turn_winner()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(
        routing_table(&["a-provider", "b-provider"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Stream(vec![
                StreamPart::ToolCallDelta {
                    id: "c1".into(),
                    name: Some("search".into()),
                    arguments: "{}".into(),
                },
                StreamPart::Finish {
                    reason: FinishReason::ToolCalls,
                },
            ]),
            MockResponse::Error(BitrouterError::UpstreamRateLimited {
                retry_after: Some(12),
            }),
            MockResponse::Stream(vec![StreamPart::Finish {
                reason: FinishReason::Stop,
            }]),
        ])),
        |builder| {
            builder
                .server_tool_loop(search_server_tool_loop())
                .settlement_recorder(ProviderCapturingRecorder(captured.clone()));
        },
    );

    let stream = pipeline.execute_stream(stream_request()).await?;
    let parts = collect_stream(stream).await;
    assert!(parts.into_iter().all(|part| part.is_ok()));
    let captured = match captured.lock() {
        Ok(captured) => captured.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    assert_eq!(
        captured.as_slice(),
        &[("b-provider".to_string(), "test-model".to_string())]
    );
    Ok(())
}

fn router_tool_call() -> Content {
    Content::ToolCall {
        id: "c1".to_string(),
        name: "search".to_string(),
        arguments: "{}".to_string(),
        provider_executed: false,
        dynamic: false,
        provider_metadata: Default::default(),
    }
}

fn gen_result(content: Vec<Content>) -> GenerateResult {
    GenerateResult {
        content,
        usage: Some(Usage {
            prompt_tokens: 3,
            completion_tokens: 2,
            ..Default::default()
        }),
        finish_reason: Some(FinishReason::ToolCalls),
        response_id: None,
        stop_details: None,
        provider_metadata: Default::default(),
    }
}

#[tokio::test]
async fn server_tool_loop_resolves_a_router_tool_call() {
    use crate::language_model::server_tools::approval::AllowAll;
    use crate::language_model::server_tools::config::ServerToolLoopConfig;
    use crate::language_model::server_tools::loop_controller::ServerToolLoop;
    use crate::language_model::server_tools::toolset::{
        RouterToolset, ToolContext, ToolsetRegistry,
    };

    struct OneTool;
    #[async_trait]
    impl RouterToolset for OneTool {
        async fn list_tools(&self, _c: &ToolContext) -> Result<Vec<Tool>> {
            Ok(vec![Tool::Function {
                name: "search".to_string(),
                description: None,
                parameters: serde_json::json!({ "type": "object" }),
                strict: None,
                provider_metadata: Default::default(),
            }])
        }
        async fn call_tool(
            &self,
            _name: &str,
            _arguments: &str,
            _c: &ToolContext,
        ) -> Result<ToolResultOutput> {
            Ok(ToolResultOutput::Text {
                value: "tool ran".to_string(),
            })
        }
        fn owns(&self, name: &str) -> bool {
            name == "search"
        }
    }

    let executor = Arc::new(MockExecutor::new(vec![
        MockResponse::Generate(gen_result(vec![router_tool_call()])),
        MockResponse::Generate(gen_result(vec![Content::Text {
            text: "final answer".to_string(),
            provider_metadata: Default::default(),
        }])),
    ]));
    let server_loop = Arc::new(ServerToolLoop::new(
        ToolsetRegistry::new(vec![Arc::new(OneTool)]),
        ServerToolLoopConfig::default(),
        Arc::new(AllowAll),
    ));
    let pipeline = pipeline_with(routing_table(&["openai"]), executor, |b| {
        b.server_tool_loop(server_loop);
    });

    let resp = pipeline.execute(request()).await.expect("request succeeds");
    // The router tool call was executed by the loop; the caller sees the final
    // text after two upstream calls, with usage summed across both.
    assert!(
        matches!(&resp.result.content[0], Content::Text { text, .. } if text == "final answer")
    );
    assert_eq!(resp.result.usage.unwrap().prompt_tokens, 6);
}

#[tokio::test]
async fn fusion_declaration_is_advertised_and_executed_end_to_end() {
    // Exercises the full metadata-threading path: the declaration hook parses a
    // bitrouter:fusion declaration and stashes the resolved config; the loop
    // snapshots it into the ToolContext; the FusionToolset advertises + runs the
    // engine; the analysis feeds back and the model writes the final answer.
    use crate::language_model::server_tools::approval::AllowAll;
    use crate::language_model::server_tools::config::ServerToolLoopConfig;
    use crate::language_model::server_tools::declarations::ServerToolDeclarationsHook;
    use crate::language_model::server_tools::fusion::FusionToolset;
    use crate::language_model::server_tools::loop_controller::ServerToolLoop;
    use crate::language_model::server_tools::nested::{NestedOutcome, NestedRequest, NestedRunner};
    use crate::language_model::server_tools::toolset::{ToolContext, ToolsetRegistry};

    // Scripted nested runner: the judge (response_format set) returns analysis
    // JSON; a panel member returns a plain answer.
    struct ScriptedRunner;
    #[async_trait]
    impl NestedRunner for ScriptedRunner {
        async fn run(
            &self,
            req: NestedRequest,
            _c: &ToolContext,
        ) -> std::result::Result<NestedOutcome, String> {
            let text = if req.response_format.is_some() {
                "{\"consensus\":[\"agreed point\"]}".to_string()
            } else {
                "panel answer".to_string()
            };
            Ok(NestedOutcome {
                model: req.model,
                text,
                usage: Default::default(),
            })
        }
    }

    // A bare bitrouter:fusion declaration, named `fusion` so the loop strips it
    // before upstream (empty args → single-member panel on the request model).
    let mut req = request();
    req.prompt.tools.push(Tool::ProviderDefined {
        id: "bitrouter.fusion".to_string(),
        name: "fusion".to_string(),
        args: serde_json::json!({}),
        provider_metadata: Default::default(),
    });

    let server_loop = Arc::new(ServerToolLoop::new(
        ToolsetRegistry::new(vec![Arc::new(FusionToolset::new(Arc::new(ScriptedRunner)))]),
        ServerToolLoopConfig::default(),
        Arc::new(AllowAll),
    ));

    // Upstream: turn 1 calls the `fusion` tool; turn 2 writes the final answer.
    let executor = Arc::new(MockExecutor::new(vec![
        MockResponse::Generate(gen_result(vec![Content::ToolCall {
            id: "c1".to_string(),
            name: "fusion".to_string(),
            arguments: "{\"prompt\":\"deliberate this\"}".to_string(),
            provider_executed: false,
            dynamic: false,
            provider_metadata: Default::default(),
        }])),
        MockResponse::Generate(gen_result(vec![Content::Text {
            text: "final answer".to_string(),
            provider_metadata: Default::default(),
        }])),
    ]));

    let pipeline = pipeline_with(routing_table(&["openai"]), executor, |b| {
        b.pre_request_hook(ServerToolDeclarationsHook);
        b.server_tool_loop(server_loop);
    });

    let resp = pipeline.execute(req).await.expect("request succeeds");
    assert!(
        matches!(&resp.result.content[0], Content::Text { text, .. } if text == "final answer")
    );
    // Two upstream turns ran (the fusion tool was executed and fed back).
    assert_eq!(resp.result.usage.unwrap().prompt_tokens, 6);
}

#[tokio::test]
async fn without_loop_a_tool_call_turn_is_returned_unchanged() {
    // No server_tool_loop configured: the pipeline stays single-shot and hands
    // the model's tool-call turn straight back to the caller (only one upstream
    // call is consumed).
    let executor = Arc::new(MockExecutor::new(vec![
        MockResponse::Generate(gen_result(vec![router_tool_call()])),
        MockResponse::Generate(gen_result(vec![Content::Text {
            text: "unused".to_string(),
            provider_metadata: Default::default(),
        }])),
    ]));
    let pipeline = pipeline_with(routing_table(&["openai"]), executor, |_b| {});

    let resp = pipeline.execute(request()).await.expect("request succeeds");
    assert!(matches!(&resp.result.content[0], Content::ToolCall { .. }));
}

#[tokio::test]
async fn server_tool_loop_streams_router_tool_activity() {
    use crate::language_model::server_tools::approval::AllowAll;
    use crate::language_model::server_tools::config::ServerToolLoopConfig;
    use crate::language_model::server_tools::loop_controller::ServerToolLoop;
    use crate::language_model::server_tools::toolset::{
        RouterToolset, ToolContext, ToolsetRegistry,
    };

    struct OneTool;
    #[async_trait]
    impl RouterToolset for OneTool {
        async fn list_tools(&self, _c: &ToolContext) -> Result<Vec<Tool>> {
            Ok(vec![Tool::Function {
                name: "search".to_string(),
                description: None,
                parameters: serde_json::json!({ "type": "object" }),
                strict: None,
                provider_metadata: Default::default(),
            }])
        }
        async fn call_tool(
            &self,
            _n: &str,
            _a: &str,
            _c: &ToolContext,
        ) -> Result<ToolResultOutput> {
            Ok(ToolResultOutput::Text {
                value: "ran".to_string(),
            })
        }
        fn owns(&self, name: &str) -> bool {
            name == "search"
        }
        fn server_name(&self) -> Option<&str> {
            Some("docs")
        }
    }

    // Each iteration fails over from `first` to `second`. Nested streaming
    // turns must retain hop observability and final target attribution.
    let executor = Arc::new(MockExecutor::new(vec![
        MockResponse::Error(BitrouterError::UpstreamUnavailable),
        MockResponse::Stream(vec![
            StreamPart::TextDelta {
                text: "searching ".to_string(),
            },
            StreamPart::ToolCallDelta {
                id: "c1".to_string(),
                name: Some("search".to_string()),
                arguments: "{}".to_string(),
            },
            StreamPart::Finish {
                reason: FinishReason::ToolCalls,
            },
        ]),
        MockResponse::Error(BitrouterError::UpstreamUnavailable),
        MockResponse::Stream(vec![
            StreamPart::TextDelta {
                text: "answer".to_string(),
            },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
        ]),
    ]));
    let server_loop = Arc::new(ServerToolLoop::new(
        ToolsetRegistry::new(vec![Arc::new(OneTool)]),
        ServerToolLoopConfig::default(),
        Arc::new(AllowAll),
    ));
    let timings = Arc::new(std::sync::Mutex::new(Vec::new()));
    let hops = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(routing_table(&["first", "second"]), executor, |b| {
        b.server_tool_loop(server_loop)
            .settlement_recorder(TimingSnapshotRecorder(timings.clone()))
            .observe_hook(HopEventRecorder(hops.clone()));
    });

    let mut stream = pipeline
        .execute_stream(stream_request())
        .await
        .expect("stream starts");
    let mut parts = Vec::new();
    while let Some(item) = stream.next().await {
        parts.push(item.expect("stream part ok"));
    }

    // The router tool ran server-side, surfaced as ServerToolCall + Result...
    assert!(
        parts
            .iter()
            .any(|p| matches!(p, StreamPart::ServerToolCall { name, .. } if name == "search"))
    );
    assert!(
        parts
            .iter()
            .any(|p| matches!(p, StreamPart::ServerToolResult { .. }))
    );
    // ...the raw router ToolCallDelta was suppressed...
    assert!(
        !parts
            .iter()
            .any(|p| matches!(p, StreamPart::ToolCallDelta { .. }))
    );
    // ...and both turns' text streamed through to one continuous answer.
    let text: String = parts
        .iter()
        .filter_map(|p| match p {
            StreamPart::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "searching answer");

    let snapshots = timings.lock().unwrap();
    let snapshot = snapshots.first().expect("server-tool settlement timing");
    assert_eq!(snapshot.provider_id, "second");
    assert!(snapshot.latency_ms >= 1);
    assert!(snapshot.generation_time_ms >= 1);
    assert!(
        snapshot
            .first_token_latency_ms
            .is_some_and(|value| value >= 1)
    );

    assert_eq!(
        *hops.lock().unwrap(),
        vec![
            "start:first",
            "end:first:failed",
            "start:second",
            "end:second:stream_started",
            "start:first",
            "end:first:failed",
            "start:second",
            "end:second:stream_started",
            "request:completed",
        ],
        "every nested upstream attempt is observable"
    );
}

#[tokio::test]
async fn server_tool_stream_handshake_failure_still_settles_and_observes_end() {
    use crate::language_model::server_tools::approval::AllowAll;
    use crate::language_model::server_tools::config::ServerToolLoopConfig;
    use crate::language_model::server_tools::loop_controller::ServerToolLoop;
    use crate::language_model::server_tools::toolset::ToolsetRegistry;

    let server_loop = Arc::new(ServerToolLoop::new(
        ToolsetRegistry::new(Vec::new()),
        ServerToolLoopConfig::default(),
        Arc::new(AllowAll),
    ));
    let timings = Arc::new(std::sync::Mutex::new(Vec::new()));
    let hops = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = pipeline_with(
        routing_table(&["first", "second"]),
        Arc::new(MockExecutor::new(vec![
            MockResponse::Error(BitrouterError::UpstreamUnavailable),
            MockResponse::Error(BitrouterError::UpstreamUnavailable),
        ])),
        |builder| {
            builder
                .server_tool_loop(server_loop)
                .settlement_recorder(TimingSnapshotRecorder(timings.clone()))
                .observe_hook(HopEventRecorder(hops.clone()));
        },
    );

    let error = match pipeline.execute_stream(stream_request()).await {
        Ok(_) => panic!("all nested upstream handshakes should fail"),
        Err(error) => error,
    };
    assert!(matches!(error, BitrouterError::UpstreamUnavailable));

    let snapshots = timings.lock().unwrap();
    let snapshot = snapshots.first().expect("failed stream still settles");
    assert!(snapshot.has_error);
    assert!(snapshot.latency_ms >= 1);
    assert_eq!(
        *hops.lock().unwrap(),
        vec![
            "start:first",
            "end:first:failed",
            "start:second",
            "end:second:failed",
            "request:failed",
        ]
    );
}
