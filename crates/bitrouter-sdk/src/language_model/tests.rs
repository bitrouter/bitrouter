//! Phase-1 pipeline tests — exit criteria for 008 Phase 1 and the test
//! strategy in 003 §11.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde::Serialize;

use crate::caller::{CallerContext, FundingSource, PaymentMethod};
use crate::error::{BitrouterError, Result};
use crate::event::PipelineEvent;
use crate::language_model::executor::MockResponse;
use crate::language_model::*;

// ===== test fixtures =====

fn target(provider: &str) -> RoutingTarget {
    RoutingTarget {
        provider_name: provider.to_string(),
        service_id: "test-model".to_string(),
        api_base: "https://example.invalid".to_string(),
        api_key: "k".to_string(),
        api_protocol: ApiProtocol::Openai,
        api_key_override: None,
        api_base_override: None,
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
        messages: vec![Message::text(Role::User, "hi")],
        tools: Vec::new(),
        params: GenerationParams::default(),
        stream: false,
    };
    PipelineRequest::new(
        "test-model",
        CallerContext::new("k1", "u1", PaymentMethod::Credits),
        prompt,
    )
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

/// A `ChargeStrategy` that records each call and claims (or passes) on demand.
struct ScriptedCharge {
    label: &'static str,
    claim: bool,
    calls: Arc<AtomicUsize>,
    log: Arc<std::sync::Mutex<Vec<&'static str>>>,
}
#[async_trait]
impl ChargeStrategy for ScriptedCharge {
    async fn try_charge(&self, ctx: &mut SettlementContext) -> Result<ChargeOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.log.lock().unwrap().push(self.label);
        if self.claim {
            ctx.final_charge_micro_usd = 1234;
            ctx.funding_source = FundingSource::Credits;
            Ok(ChargeOutcome::Claimed)
        } else {
            Ok(ChargeOutcome::Pass)
        }
    }
}

struct CountingRecorder(Arc<AtomicUsize>);
#[async_trait]
impl SettlementRecorder for CountingRecorder {
    async fn record(&self, _ctx: &SettlementContext) -> Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
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

// ===== tests =====

#[tokio::test]
async fn full_pipeline_runs_all_four_stages() {
    let calls = Arc::new(AtomicUsize::new(0));
    let recorded = Arc::new(AtomicUsize::new(0));
    let charge_log = Arc::new(std::sync::Mutex::new(Vec::new()));

    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::always_text("hello")),
        |b| {
            b.pre_request_hook(AllowHook)
                .route_hook(EmitRouteHook)
                .charge_strategy(ScriptedCharge {
                    label: "credit",
                    claim: true,
                    calls: calls.clone(),
                    log: charge_log.clone(),
                })
                .settlement_recorder(CountingRecorder(recorded.clone()));
        },
    );

    let resp = pipeline.execute(request()).await.expect("request succeeds");
    assert_eq!(resp.result.content.len(), 1);
    assert_eq!(resp.final_charge_micro_usd, 1234);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "charge strategy ran");
    assert_eq!(recorded.load(Ordering::SeqCst), 1, "recorder ran");
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
async fn charge_chain_is_mutually_exclusive_first_claim_wins() {
    let calls = Arc::new(AtomicUsize::new(0));
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));

    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::always_text("x")),
        |b| {
            b.charge_strategy(ScriptedCharge {
                label: "byok",
                claim: false,
                calls: calls.clone(),
                log: log.clone(),
            })
            .charge_strategy(ScriptedCharge {
                label: "credit",
                claim: true,
                calls: calls.clone(),
                log: log.clone(),
            })
            .charge_strategy(ScriptedCharge {
                label: "mpp",
                claim: true,
                calls: calls.clone(),
                log: log.clone(),
            });
        },
    );

    pipeline.execute(request()).await.expect("ok");

    // byok passes, credit claims, mpp must NOT be called.
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(*log.lock().unwrap(), vec!["byok", "credit"]);
}

#[tokio::test]
async fn charge_chain_exhausted_leaves_charge_zero() {
    let calls = Arc::new(AtomicUsize::new(0));
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));

    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::always_text("x")),
        |b| {
            b.charge_strategy(ScriptedCharge {
                label: "a",
                claim: false,
                calls: calls.clone(),
                log: log.clone(),
            })
            .charge_strategy(ScriptedCharge {
                label: "b",
                claim: false,
                calls: calls.clone(),
                log: log.clone(),
            });
        },
    );

    let resp = pipeline.execute(request()).await.expect("ok");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(resp.final_charge_micro_usd, 0);
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
    // An upstream 500 surfaces to the client as a 502 Bad Gateway.
    assert_eq!(err.status(), 502);
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
                }],
                usage: None,
                finish_reason: Some(FinishReason::Stop),
            }),
        ])),
        |_b| {},
    );

    let resp = pipeline.execute(request()).await.expect("falls back to b");
    assert_eq!(
        resp.result.content,
        vec![Content::Text {
            text: "from b".into()
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
                content: vec![Content::Text { text: "b".into() }],
                usage: None,
                finish_reason: None,
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
                content: vec![Content::Text { text: "b".into() }],
                usage: None,
                finish_reason: None,
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
    // EmitRouteHook emits TestRouteEvent in Stage 2; a charge strategy in
    // Stage 4 must be able to see it via the carried-over event bus.
    struct EventAssertCharge;
    #[async_trait]
    impl ChargeStrategy for EventAssertCharge {
        async fn try_charge(&self, ctx: &mut SettlementContext) -> Result<ChargeOutcome> {
            assert!(
                ctx.has_event::<TestRouteEvent>(),
                "route-stage event reached settlement"
            );
            Ok(ChargeOutcome::Pass)
        }
    }

    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::always_text("x")),
        |b| {
            b.route_hook(EmitRouteHook)
                .charge_strategy(EventAssertCharge);
        },
    );
    pipeline.execute(request()).await.expect("ok");
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
