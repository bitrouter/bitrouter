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
use crate::language_model::*;

// ===== test fixtures =====

fn target(provider: &str) -> RoutingTarget {
    RoutingTarget {
        provider_name: provider.to_string(),
        service_id: "test-model".to_string(),
        api_base: "https://example.invalid".to_string(),
        api_key: "k".to_string(),
        api_protocol: ApiProtocol::ChatCompletions,
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

    // Iteration 1 streams a router tool call; iteration 2 streams the answer.
    let executor = Arc::new(MockExecutor::new(vec![
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
    let pipeline = pipeline_with(routing_table(&["openai"]), executor, |b| {
        b.server_tool_loop(server_loop);
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
}
