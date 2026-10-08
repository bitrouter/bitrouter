//! Native Decisions lifecycle and operation applicability through the public SDK.
#![cfg(feature = "server")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use bitrouter_ai::classifier::ClassifierRequest;
use bitrouter_ai::protocol::decisions::DecisionsCodec;
use bitrouter_ai::types::{ApiProtocol, ModelOperation, UsageOrigin};
use bitrouter_sdk::App;
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::config::{Config, ConfigRoutingTable};
use bitrouter_sdk::error::{BitrouterError, Result};
use bitrouter_sdk::extension::request_check::{ContentFragmentKind, Decision, Input};
use bitrouter_sdk::model_call::builder::PipelineBuilder;
use bitrouter_sdk::model_call::context::PipelineContext;
use bitrouter_sdk::model_call::executor::{HttpExecutor, MockExecutor, MockResponse};
use bitrouter_sdk::model_call::hooks::{FallbackDecision, HookDecision, PreRequestHook};
use bitrouter_sdk::model_call::operations::{HookStage, OperationScope};
use bitrouter_sdk::model_call::request_checks::{
    CheckerFailure, CheckerResult, RequestCheckBinding, RequestCheckerRunner,
};
use bitrouter_sdk::model_call::routing::{
    FallbackPolicy, ModelInfo, ModelResolution, RouterRequestIdentity, RoutingPrefs, RoutingTable,
    StaticRoutingTable,
};
use bitrouter_sdk::model_call::settlement::{SettlementContext, SettlementRecorder};
use bitrouter_sdk::model_call::types::{PipelineRequest, RoutingTarget};
use bitrouter_sdk::server::{AppState, build_router};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc, oneshot};
use tower::ServiceExt;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn request() -> TestResult<ClassifierRequest> {
    Ok(DecisionsCodec::parse_request(json!({
        "model":"test", "input":"private evidence", "safety_identifier":"private-safety",
        "questions":[{"type":"predicate", "name":"q", "instructions":"Evaluate the evidence"}]
    }))?)
}

fn native_response(valid: bool) -> Value {
    json!({"model":"native-test", "answers":[{"type":"predicate", "name":"q", "probability":if valid {0.7} else {2.0}, "extra_answer":true}],
        "usage":{"input_tokens":10,"output_tokens":0,"total_tokens":10,
            "input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},
            "output_tokens_details":{"reasoning_tokens":0}}, "extra_envelope":"preserved"})
}

fn target(base: &str, provider: &str, protocol: ApiProtocol) -> RoutingTarget {
    RoutingTarget {
        provider_name: provider.into(),
        service_id: "native-test".into(),
        api_base: base.into(),
        api_key: "fixture-secret".into(),
        api_protocol: protocol,
        chat_token_limit_field: None,
        chat_supports_store: None,
        chat_supports_stream_options: None,
        chat_google_extensions: false,
        reasoning_effort: None,
        account_label: None,
        api_key_override: None,
        api_base_override: None,
        auth_scheme: Default::default(),
        headers: Vec::new(),
    }
}

fn builder(targets: Vec<RoutingTarget>) -> TestResult<PipelineBuilder> {
    let routes = Arc::new(StaticRoutingTable::new());
    routes.insert("test", targets);
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(routes)
        .executor(Arc::new(HttpExecutor::with_defaults()?))
        .served_operations(OperationScope::Both);
    Ok(builder)
}

fn router(pipeline: Arc<bitrouter_sdk::model_call::pipeline::Pipeline>) -> Router {
    build_router(AppState {
        model_call: pipeline,
        mcp: None,
        skip_auth: true,
        metrics_renderer: None,
        prompt_transforms: Vec::new(),
    })
}

async fn post_decision(router: Router, body: Value) -> TestResult<(StatusCode, HeaderMap, Value)> {
    post_classifier(router, "/v1/decisions", body).await
}

async fn post_classifier(
    router: Router,
    endpoint: &str,
    body: Value,
) -> TestResult<(StatusCode, HeaderMap, Value)> {
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(endpoint)
                .header("content-type", "application/json")
                .header("x-bitrouter-request-id", "decision-request")
                .body(Body::from(serde_json::to_vec(&body)?))?,
        )
        .await?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await?)?;
    Ok((status, headers, body))
}

#[derive(Clone)]
struct Upstream {
    requests: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    replies: Arc<Vec<(StatusCode, Value)>>,
    accepted: mpsc::UnboundedSender<()>,
    release: Option<Arc<Notify>>,
}

async fn upstream(
    State(state): State<Upstream>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let index = {
        let mut requests = state
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = requests.len();
        requests.push((headers, body));
        index
    };
    let _ = state.accepted.send(());
    if let Some(release) = &state.release {
        release.notified().await;
    }
    let reply = state
        .replies
        .get(index)
        .or_else(|| state.replies.last())
        .cloned()
        .unwrap_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":"missing fixture reply"}),
        ));
    (reply.0, Json(reply.1))
}

struct Fixture {
    base: String,
    state: Upstream,
    accepted: mpsc::UnboundedReceiver<()>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<std::result::Result<(), std::io::Error>>>,
}

impl Fixture {
    async fn start(replies: Vec<(StatusCode, Value)>, gated: bool) -> TestResult<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}/v1", listener.local_addr()?);
        let (accepted_tx, accepted) = mpsc::unbounded_channel();
        let state = Upstream {
            requests: Arc::new(Mutex::new(Vec::new())),
            replies: Arc::new(replies),
            accepted: accepted_tx,
            release: gated.then(|| Arc::new(Notify::new())),
        };
        let (shutdown, shutdown_rx) = oneshot::channel();
        let router = Router::new()
            .route("/v1/decisions", post(upstream))
            .route("/v1/systemone", post(upstream))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
        });
        Ok(Self {
            base,
            state,
            accepted,
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }
    fn requests(&self) -> Vec<(HeaderMap, Value)> {
        self.state
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    async fn stop(mut self) -> TestResult {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            tokio::time::timeout(Duration::from_secs(10), task).await???;
        }
        Ok(())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[derive(Debug)]
struct Evidence {
    operation: ModelOperation,
    input_tokens: u64,
    origin: UsageOrigin,
    failed: bool,
    protocol: Option<ApiProtocol>,
    availability: Option<bitrouter_ai::types::UsageAvailability>,
}
struct Record(mpsc::UnboundedSender<Evidence>);
#[async_trait]
impl SettlementRecorder for Record {
    async fn record(&self, ctx: &mut SettlementContext) -> Result<()> {
        self.0
            .send(Evidence {
                operation: ctx.operation,
                input_tokens: ctx.prompt_tokens,
                origin: ctx.usage_origin,
                failed: ctx.error.is_some(),
                availability: ctx.usage_availability.clone(),
                protocol: ctx
                    .target
                    .as_ref()
                    .map(|target| target.api_protocol.clone()),
            })
            .map_err(|_| BitrouterError::internal("test settlement receiver closed"))
    }
}
struct Count(Arc<AtomicUsize>);
#[async_trait]
impl PreRequestHook for Count {
    async fn check(&self, _ctx: &mut PipelineContext) -> Result<HookDecision> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(HookDecision::Allow)
    }
}
struct RetryAll;
impl FallbackPolicy for RetryAll {
    fn classify(&self, _error: &BitrouterError, _target: &RoutingTarget) -> FallbackDecision {
        FallbackDecision::TryNext
    }
}

fn systemone_request() -> Value {
    json!({"model":"test","state":"Build passed.","questions":{"original/key":{"type":"noul","instructions":"Did the build pass?"}}})
}

fn systemone_response(valid: bool) -> Value {
    json!({"model":"native-test","answers":{"original/key":{"type":"noul","noul":if valid {0.8} else {1.5}}},"usage":{"input_tokens":12,"output_tokens":3}})
}

#[tokio::test]
async fn native_systemone_gateway_retains_map_identity_and_partial_usage() -> TestResult {
    let fixture = Fixture::start(vec![(StatusCode::OK, systemone_response(true))], false).await?;
    let (sender, mut settled) = mpsc::unbounded_channel();
    let mut builder = builder(vec![target(
        &fixture.base,
        "typesafe-fixture",
        ApiProtocol::SystemOne,
    )])?;
    builder.settlement_recorder_for(Record(sender), OperationScope::Both);
    let pipeline = Arc::new(builder.build()?);
    let (status, _, body) = post_classifier(
        router(pipeline.clone()),
        "/v1/systemone",
        systemone_request(),
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, systemone_response(true));
    assert_eq!(fixture.requests().len(), 1);
    assert_eq!(fixture.requests()[0].1["model"], "native-test");
    let evidence = settled.recv().await.ok_or("missing settlement")?;
    assert_eq!(evidence.operation, ModelOperation::Classification);
    assert_eq!(evidence.protocol, Some(ApiProtocol::SystemOne));
    assert_eq!(evidence.input_tokens, 12);
    assert!(
        !evidence
            .availability
            .ok_or("missing availability")?
            .cache_read
    );
    assert!(!evidence.failed);
    pipeline.drain_required_pending_settlements().await?;
    assert!(settled.try_recv().is_err());
    fixture.stop().await
}

#[tokio::test]
async fn systemone_caller_uses_decisions_upstream_and_destination_confidence() -> TestResult {
    let response = json!({"model":"native-test","answers":[{"type":"choice","name":null,"choice":"pass","probabilities":[{"value":"pass","probability":0.6},{"value":"fail","probability":0.4}],"confidence":0.95}],"usage":{"input_tokens":12,"output_tokens":0,"total_tokens":12,"input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}});
    let fixture = Fixture::start(vec![(StatusCode::OK, response)], false).await?;
    let (sender, mut settled) = mpsc::unbounded_channel();
    let mut builder = builder(vec![target(
        &fixture.base,
        "openai-fixture",
        ApiProtocol::Decisions,
    )])?;
    builder.settlement_recorder_for(Record(sender), OperationScope::Both);
    let pipeline = Arc::new(builder.build()?);
    let request = json!({"model":"test","state":"Build passed.","questions":{"client-choice":{"type":"choice","instructions":"Choose status.","criteria":{"pass":null,"fail":null}}}});
    let (status, _, body) =
        post_classifier(router(pipeline.clone()), "/v1/systemone", request).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["answers"]["client-choice"]["choice"], "pass");
    let confidence = body["answers"]["client-choice"]["confidence"]
        .as_f64()
        .ok_or("missing confidence")?;
    assert!((confidence - 0.2).abs() < 1e-10);
    assert_eq!(body["usage"], json!({"input_tokens":12,"output_tokens":0}));
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1["input"], "Build passed.");
    assert!(requests[0].1["questions"].is_array());
    assert!(requests[0].1.get("state").is_none());
    let evidence = settled.recv().await.ok_or("missing settlement")?;
    assert_eq!(evidence.protocol, Some(ApiProtocol::Decisions));
    assert!(!evidence.failed);
    pipeline.drain_required_pending_settlements().await?;
    fixture.stop().await
}

#[tokio::test]
async fn systemone_refusal_fails_delivery_and_cannot_enter_fallback() -> TestResult {
    let refusal = json!({"model":"native-test","answers":[{"type":"refusal","name":null}],"usage":{"input_tokens":12,"output_tokens":0,"total_tokens":12,"input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}});
    let primary = Fixture::start(vec![(StatusCode::OK, refusal)], false).await?;
    let fallback = Fixture::start(vec![(StatusCode::OK, native_response(true))], false).await?;
    let (sender, mut settled) = mpsc::unbounded_channel();
    let mut builder = builder(vec![
        target(&primary.base, "a-primary", ApiProtocol::Decisions),
        target(&fallback.base, "z-fallback", ApiProtocol::Decisions),
    ])?;
    builder
        .fallback_policy(Arc::new(RetryAll))
        .settlement_recorder_for(Record(sender), OperationScope::Both);
    let pipeline = Arc::new(builder.build()?);
    let (status, _, _) = post_classifier(
        router(pipeline.clone()),
        "/v1/systemone",
        systemone_request(),
    )
    .await?;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(primary.requests().len(), 1);
    assert!(fallback.requests().is_empty());
    let evidence = settled.recv().await.ok_or("missing settlement")?;
    assert_eq!(evidence.input_tokens, 12);
    assert!(evidence.failed);
    pipeline.drain_required_pending_settlements().await?;
    assert!(settled.try_recv().is_err());
    primary.stop().await?;
    fallback.stop().await
}

#[tokio::test]
async fn systemone_malformed_completion_preserves_unknown_breakdowns_without_retry() -> TestResult {
    let primary = Fixture::start(vec![(StatusCode::OK, systemone_response(false))], false).await?;
    let fallback = Fixture::start(vec![(StatusCode::OK, systemone_response(true))], false).await?;
    let (sender, mut settled) = mpsc::unbounded_channel();
    let mut builder = builder(vec![
        target(&primary.base, "a-primary", ApiProtocol::SystemOne),
        target(&fallback.base, "z-fallback", ApiProtocol::SystemOne),
    ])?;
    builder
        .fallback_policy(Arc::new(RetryAll))
        .settlement_recorder_for(Record(sender), OperationScope::Both);
    let pipeline = Arc::new(builder.build()?);
    let (status, _, _) = post_classifier(
        router(pipeline.clone()),
        "/v1/systemone",
        systemone_request(),
    )
    .await?;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(primary.requests().len(), 1);
    assert!(fallback.requests().is_empty());
    let evidence = settled.recv().await.ok_or("missing settlement")?;
    assert_eq!(evidence.input_tokens, 12);
    assert!(
        !evidence
            .availability
            .ok_or("missing availability")?
            .reasoning
    );
    assert!(evidence.failed);
    pipeline.drain_required_pending_settlements().await?;
    assert!(settled.try_recv().is_err());
    primary.stop().await?;
    fallback.stop().await
}

#[tokio::test]
async fn unresolved_decisions_usage_projection_excludes_systemone_before_io() -> TestResult {
    let fixture = Fixture::start(vec![(StatusCode::OK, systemone_response(true))], false).await?;
    let pipeline = Arc::new(
        builder(vec![target(
            &fixture.base,
            "fixture",
            ApiProtocol::SystemOne,
        )])?
        .build()?,
    );
    let request = json!({"model":"test","input":"Build passed.","questions":[{"type":"predicate","instructions":"Did the build pass?"}]});
    let (status, _, _) = post_decision(router(pipeline), request).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(fixture.requests().is_empty());
    fixture.stop().await
}

#[tokio::test]
async fn classifier_discovery_identifies_operation_and_native_protocol() -> TestResult {
    let pipeline = Arc::new(
        builder(vec![target(
            "https://fixture.invalid/v1",
            "fixture",
            ApiProtocol::SystemOne,
        )])?
        .build()?,
    );
    let response = router(pipeline)
        .oneshot(Request::builder().uri("/v1/models").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await?)?;
    assert_eq!(body["data"][0]["operations"], json!(["classification"]));
    assert_eq!(body["data"][0]["api_protocols"], json!(["systemone"]));
    Ok(())
}

#[tokio::test]
async fn typed_classifier_considers_alternate_protocol_for_same_provider() -> TestResult {
    use bitrouter_ai::classifier::{ClassifierInput, ClassifierQuestion};
    let fixture = Fixture::start(vec![(StatusCode::OK,json!({"model":"native-test","answers":{"q0":{"type":"noul","noul":0.8}},"usage":{"input_tokens":12,"output_tokens":3}}))],false).await?;
    let config: Config = serde_json::from_value(
        json!({"providers":{"fixture":{"api_base":fixture.base,"api_key":"fixture-secret","active":true,"api_protocol":[{"*": ["decisions","systemone"]}],"models":[{"id":"test","provider_model_id":"native-test"}]}}}),
    )?;
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(Arc::new(ConfigRoutingTable::from_config(config)))
        .executor(Arc::new(HttpExecutor::with_defaults()?))
        .served_operations(OperationScope::Both);
    let pipeline = builder.build()?;
    let request = ClassifierRequest {
        model: "test".into(),
        source_protocol: None,
        input: ClassifierInput::Structured(json!({"build":"passed"})),
        questions: vec![ClassifierQuestion::Predicate {
            criteria: None,
            instructions: Some("Check?".into()),
            name: None,
            key: None,
        }],
        safety_identifier: None,
    };
    let result = pipeline
        .execute(PipelineRequest::new_classification(
            "test",
            CallerContext::local(),
            request,
        ))
        .await?;
    assert_eq!(
        result
            .result
            .classification()
            .ok_or("missing classifier result")?
            .protocol,
        ApiProtocol::SystemOne
    );
    assert_eq!(fixture.requests().len(), 1);
    assert_eq!(fixture.requests()[0].1["state"], json!({"build":"passed"}));
    fixture.stop().await
}

#[tokio::test]
async fn custom_executor_malformed_native_success_keeps_usage_and_cannot_retry() -> TestResult {
    let request = request()?;
    let valid = DecisionsCodec::parse_response(native_response(true), &request)?;
    let mut invalid = valid.clone();
    invalid.usage.prompt_tokens = 1_000_000;
    match invalid.answers.first_mut() {
        Some(bitrouter_ai::classifier::ClassifierAnswer::Predicate { probability, .. }) => {
            *probability = 2.0;
        }
        _ => return Err("missing predicate fixture".into()),
    }
    let (sender, mut settled) = mpsc::unbounded_channel();
    let mut builder = builder(vec![
        target(
            "https://fixture.invalid/v1",
            "primary",
            ApiProtocol::Decisions,
        ),
        target(
            "https://fixture.invalid/v1",
            "fallback",
            ApiProtocol::Decisions,
        ),
    ])?;
    builder
        .executor(Arc::new(MockExecutor::new(vec![
            MockResponse::Classification(invalid),
            MockResponse::Classification(valid),
        ])))
        .fallback_policy(Arc::new(RetryAll))
        .settlement_recorder_for(Record(sender), OperationScope::Both);
    let pipeline = builder.build()?;
    let outcome = pipeline
        .execute(PipelineRequest::new_classification(
            "test",
            CallerContext::local(),
            request,
        ))
        .await;
    assert!(matches!(
        outcome,
        Err(BitrouterError::UpstreamInvalidResponse { .. })
    ));
    let evidence = settled.recv().await.ok_or("missing settlement")?;
    assert_eq!(evidence.input_tokens, 10);
    assert_eq!(evidence.origin, UsageOrigin::ProviderReported);
    assert!(evidence.failed);
    assert!(settled.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn native_gateway_projects_selected_model_and_preserves_wire() -> TestResult {
    let fixture = Fixture::start(vec![(StatusCode::OK, native_response(true))], false).await?;
    let (settled_tx, mut settled) = mpsc::unbounded_channel();
    let mut builder = builder(vec![target(
        &fixture.base,
        "openai-test",
        ApiProtocol::Decisions,
    )])?;
    let generation_count = Arc::new(AtomicUsize::new(0));
    let shared_count = Arc::new(AtomicUsize::new(0));
    builder.pre_resolution_hook(Count(generation_count.clone()));
    builder.pre_request_hook_for(Count(shared_count.clone()), OperationScope::Both);
    builder.require_hook::<Count>(HookStage::PreRequest, OperationScope::Both);
    builder.settlement_recorder_for(Record(settled_tx), OperationScope::Both);
    let pipeline = Arc::new(builder.build()?);
    let (status, headers, body) = post_decision(
        router(pipeline.clone()),
        DecisionsCodec::render_request(&request()?)?,
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("x-bitrouter-request-id")
            .and_then(|value| value.to_str().ok()),
        Some("decision-request")
    );
    assert_eq!(body, native_response(true));
    assert_eq!(generation_count.load(Ordering::SeqCst), 0);
    assert_eq!(shared_count.load(Ordering::SeqCst), 1);
    let evidence = settled.recv().await.ok_or("settlement missing")?;
    assert_eq!(evidence.operation, ModelOperation::Classification);
    assert_eq!(evidence.input_tokens, 10);
    assert_eq!(evidence.origin, UsageOrigin::ProviderReported);
    assert!(!evidence.failed);
    assert_eq!(evidence.protocol, Some(ApiProtocol::Decisions));
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1["model"], "native-test");
    assert_eq!(requests[0].1["safety_identifier"], "private-safety");
    assert_eq!(
        requests[0]
            .0
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer fixture-secret")
    );
    assert_eq!(
        requests[0]
            .0
            .get("x-bitrouter-request-id")
            .and_then(|value| value.to_str().ok()),
        Some("decision-request")
    );
    assert!(settled.try_recv().is_err());
    pipeline.drain_required_pending_settlements().await?;
    fixture.stop().await
}

#[tokio::test]
async fn malformed_completed_output_settles_once_without_custom_fallback() -> TestResult {
    let fixture = Fixture::start(
        vec![
            (StatusCode::OK, native_response(false)),
            (StatusCode::OK, native_response(true)),
        ],
        false,
    )
    .await?;
    let mut builder = builder(vec![
        target(&fixture.base, "a", ApiProtocol::Decisions),
        target(&fixture.base, "b", ApiProtocol::Decisions),
    ])?;
    let (tx, mut settlements) = mpsc::unbounded_channel();
    builder
        .fallback_policy(Arc::new(RetryAll))
        .settlement_recorder_for(Record(tx), OperationScope::Both);
    let pipeline = Arc::new(builder.build()?);
    let (status, _, body) = post_decision(
        router(pipeline.clone()),
        DecisionsCodec::render_request(&request()?)?,
    )
    .await?;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let message = body.to_string();
    assert!(!message.contains("private evidence"));
    assert!(!message.contains("private-safety"));
    let evidence = settlements
        .recv()
        .await
        .ok_or("failure settlement missing")?;
    assert!(evidence.failed);
    assert_eq!(evidence.input_tokens, 10);
    assert_eq!(evidence.origin, UsageOrigin::ProviderReported);
    assert_eq!(evidence.protocol, Some(ApiProtocol::Decisions));
    assert_eq!(fixture.requests().len(), 1);
    assert!(settlements.try_recv().is_err());
    pipeline.drain_required_pending_settlements().await?;
    fixture.stop().await
}

#[tokio::test]
async fn provider_status_failure_can_advance_before_completion() -> TestResult {
    let fixture = Fixture::start(
        vec![
            (
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error":"unavailable"}),
            ),
            (StatusCode::OK, native_response(true)),
        ],
        false,
    )
    .await?;
    let pipeline = Arc::new(
        builder(vec![
            target(&fixture.base, "a", ApiProtocol::Decisions),
            target(&fixture.base, "b", ApiProtocol::Decisions),
        ])?
        .build()?,
    );
    let response = pipeline
        .execute(PipelineRequest::new_classification(
            "test",
            CallerContext::local(),
            request()?,
        ))
        .await?;
    assert!(response.result.classification().is_some());
    assert_eq!(fixture.requests().len(), 2);
    pipeline.drain_required_pending_settlements().await?;
    fixture.stop().await
}

#[tokio::test]
async fn unsupported_request_and_generation_target_do_not_dispatch() -> TestResult {
    let fixture = Fixture::start(vec![(StatusCode::OK, native_response(true))], false).await?;
    let pipeline = Arc::new(
        builder(vec![target(
            &fixture.base,
            "a",
            ApiProtocol::ChatCompletions,
        )])?
        .build()?,
    );
    let mut unsupported = DecisionsCodec::render_request(&request()?)?;
    unsupported["stream"] = json!(false);
    let (status, _, body) = post_decision(router(pipeline.clone()), unsupported).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!body.to_string().contains("private evidence"));
    let (status, _, _) = post_decision(
        router(pipeline.clone()),
        DecisionsCodec::render_request(&request()?)?,
    )
    .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(fixture.requests().is_empty());
    let streamed = pipeline
        .clone()
        .execute_stream(PipelineRequest::new_classification(
            "test",
            CallerContext::local(),
            request()?,
        ))
        .await;
    assert!(matches!(streamed, Err(BitrouterError::BadRequest { .. })));
    pipeline.drain_required_pending_settlements().await?;
    fixture.stop().await
}

#[test]
fn builder_requires_explicit_host_coverage() -> TestResult {
    let routes = Arc::new(StaticRoutingTable::new());
    for install_generation in [false, true] {
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes.clone())
            .executor(Arc::new(MockExecutor::new(Vec::new())))
            .served_operations(OperationScope::Both)
            .require_hook::<Count>(HookStage::PreRequest, OperationScope::Both);
        if install_generation {
            builder.pre_request_hook(Count(Arc::new(AtomicUsize::new(0))));
        }
        assert!(builder.build().is_err());
    }
    Ok(())
}

#[tokio::test]
async fn config_filters_operation_before_native_preference_or_provider_pin() -> TestResult {
    let config: Config = serde_json::from_value(json!({"providers":{
        "native":{"active":true,"api_base":"https://fixture.invalid/v1","api_protocol":[{"*": ["decisions","responses","chat_completions"]}],"models":[{"id":"test"}]},
        "generation":{"active":true,"api_base":"https://fixture.invalid/v1","api_protocol":[{"*":"chat_completions"}],"models":[{"id":"test"}]}
    }}))?;
    let table = ConfigRoutingTable::from_config(config);
    let prefs = RoutingPrefs {
        operation: ModelOperation::Classification,
        inbound_protocol: Some(ApiProtocol::ChatCompletions),
        ..Default::default()
    };
    let chain = table
        .route_chain("test", &prefs, &CallerContext::local())
        .await?;
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].api_protocol, ApiProtocol::Decisions);
    assert!(matches!(
        table
            .route_chain("generation:test", &prefs, &CallerContext::local())
            .await,
        Err(BitrouterError::BadRequest { .. })
    ));
    let chain = table
        .route_chain(
            "native:test",
            &RoutingPrefs::default(),
            &CallerContext::local(),
        )
        .await?;
    assert_eq!(chain[0].api_protocol, ApiProtocol::Responses);
    Ok(())
}

struct BoundTable {
    resolution: ModelResolution,
    routes: StaticRoutingTable,
}
#[async_trait]
impl RoutingTable for BoundTable {
    async fn resolve_model(&self, _model: &str) -> Result<ModelResolution> {
        Ok(self.resolution.clone())
    }
    async fn route_chain(
        &self,
        model: &str,
        prefs: &RoutingPrefs,
        caller: &CallerContext,
    ) -> Result<Vec<RoutingTarget>> {
        self.routes.route_chain(model, prefs, caller).await
    }
    fn list_models(&self) -> Vec<ModelInfo> {
        self.routes.list_models()
    }

    fn model_info(&self, model: &str) -> Option<ModelInfo> {
        self.routes.model_info(model)
    }

    async fn reload(&self) -> Result<()> {
        self.routes.reload().await
    }
}

struct LivePrices {
    routes: StaticRoutingTable,
    lookups: Arc<AtomicUsize>,
}
#[async_trait]
impl RoutingTable for LivePrices {
    async fn route_chain(
        &self,
        model: &str,
        prefs: &RoutingPrefs,
        caller: &CallerContext,
    ) -> Result<Vec<RoutingTarget>> {
        self.routes.route_chain(model, prefs, caller).await
    }
    fn list_models(&self) -> Vec<ModelInfo> {
        self.routes.list_models()
    }
    fn model_info(&self, model: &str) -> Option<ModelInfo> {
        self.routes.model_info(model)
    }
    async fn reload(&self) -> Result<()> {
        self.routes.reload().await
    }
    fn usage_pricing(
        &self,
        _model: &str,
        _target: &RoutingTarget,
    ) -> Option<bitrouter_sdk::model_call::stream::UsagePricing> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        Some(bitrouter_sdk::model_call::stream::UsagePricing {
            base: bitrouter_sdk::model_call::stream::UsagePricingBracket {
                input_micro_usd_per_token: Some(1.0),
                output_micro_usd_per_token: Some(100.0),
                ..Default::default()
            },
            ..Default::default()
        })
    }
}

struct FrozenProjection(Option<bitrouter_sdk::model_call::stream::UsagePricing>);
#[async_trait]
impl bitrouter_sdk::model_call::hooks::RouteHook for FrozenProjection {
    async fn after_resolve(
        &self,
        chain: &[RoutingTarget],
        ctx: &mut PipelineContext,
    ) -> Result<()> {
        let target = chain
            .first()
            .ok_or_else(|| BitrouterError::internal("missing test target"))?;
        ctx.emit(bitrouter_sdk::model_call::stream::UsagePricingSnapshot {
            target: bitrouter_sdk::model_call::stream::PricingTargetKey::from_target(target),
            pricing: self.0.clone(),
        });
        Ok(())
    }
}

#[tokio::test]
async fn stream_usage_uses_frozen_rates_and_unknown_disables_live_lookup() -> TestResult {
    use bitrouter_ai::types::{FinishReason, Message, Prompt, Role, StreamPart, Usage};
    use bitrouter_sdk::model_call::stream::{UsagePricing, UsagePricingBracket};
    use futures::TryStreamExt;
    for pricing in [
        Some(UsagePricing {
            base: UsagePricingBracket {
                input_micro_usd_per_token: Some(10.0),
                output_micro_usd_per_token: Some(1.0),
                ..Default::default()
            },
            ..Default::default()
        }),
        None,
    ] {
        let (expected_input, expected_output) = if pricing.is_some() {
            (100, 1)
        } else {
            (10, 10)
        };
        let lookups = Arc::new(AtomicUsize::new(0));
        let table = LivePrices {
            routes: StaticRoutingTable::new(),
            lookups: lookups.clone(),
        };
        table.routes.insert(
            "test",
            vec![target(
                "https://fixture.invalid/v1",
                "fixture",
                ApiProtocol::ChatCompletions,
            )],
        );
        let executor = MockExecutor::new(vec![MockResponse::Stream(vec![
            StreamPart::Usage {
                usage: Usage {
                    prompt_tokens: 100,
                    completion_tokens: 1,
                    origin: UsageOrigin::ProviderReported,
                    ..Default::default()
                },
            },
            StreamPart::Usage {
                usage: Usage {
                    prompt_tokens: 10,
                    completion_tokens: 10,
                    origin: UsageOrigin::ProviderReported,
                    ..Default::default()
                },
            },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
        ])]);
        let (sender, mut settled) = mpsc::unbounded_channel();
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(Arc::new(table))
            .executor(Arc::new(executor))
            .route_hook(FrozenProjection(pricing))
            .settlement_recorder(Record(sender));
        let pipeline = Arc::new(builder.build()?);
        let prompt = Prompt {
            model: "test".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message::text(Role::User, "test")],
            tools: Vec::new(),
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: true,
        };
        let parts = pipeline
            .clone()
            .execute_stream(PipelineRequest::new("test", CallerContext::local(), prompt))
            .await?
            .try_collect::<Vec<_>>()
            .await?;
        let usage = parts
            .iter()
            .find_map(|part| match part {
                StreamPart::Usage { usage } => Some(usage),
                _ => None,
            })
            .ok_or("missing usage")?;
        assert_eq!(usage.prompt_tokens, expected_input);
        assert_eq!(usage.completion_tokens, expected_output);
        let evidence = tokio::time::timeout(Duration::from_secs(2), settled.recv())
            .await?
            .ok_or("missing settlement")?;
        assert_eq!(evidence.input_tokens, expected_input);
        assert_eq!(lookups.load(Ordering::SeqCst), 0);
        pipeline.drain_required_pending_settlements().await?;
    }
    Ok(())
}
struct Checker {
    operations: OperationScope,
    inputs: mpsc::UnboundedSender<Input>,
}
#[async_trait]
impl RequestCheckerRunner for Checker {
    fn supports_operation(
        &self,
        _binding: &RequestCheckBinding,
        operation: ModelOperation,
    ) -> bool {
        self.operations.contains(operation)
    }
    async fn check(
        &self,
        _binding: RequestCheckBinding,
        input: Input,
    ) -> std::result::Result<CheckerResult, CheckerFailure> {
        let _ = self.inputs.send(input);
        Ok(CheckerResult {
            decision: Decision::Allow,
            revision: "fixture-v1".into(),
        })
    }
}
fn bound_builder(
    max_bytes: u64,
    operations: OperationScope,
) -> TestResult<(
    PipelineBuilder,
    mpsc::UnboundedReceiver<Input>,
    Arc<AtomicUsize>,
)> {
    let mut resolution = ModelResolution::passthrough("test");
    resolution.router = Some(RouterRequestIdentity {
        router_id: "fixture".into(),
        original_selector: "test".into(),
        binding_digest: "fixture-router".into(),
    });
    resolution.request_checks.push(RequestCheckBinding {
        checker_id: "fixture".into(),
        binding_digest: "fixture-check".into(),
        max_input_bytes: max_bytes,
        timeout_ms: 1000,
    });
    let routes = StaticRoutingTable::new();
    routes.insert(
        "test",
        vec![target(
            "https://fixture.invalid/v1",
            "a",
            ApiProtocol::Decisions,
        )],
    );
    let req = request()?;
    let result = DecisionsCodec::parse_response(native_response(true), &req)?;
    let (tx, rx) = mpsc::unbounded_channel();
    let count = Arc::new(AtomicUsize::new(0));
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(Arc::new(BoundTable { resolution, routes }))
        .executor(Arc::new(MockExecutor::new(vec![
            MockResponse::Classification(result),
        ])))
        .served_operations(OperationScope::Both)
        .request_checker_runner(Arc::new(Checker {
            operations,
            inputs: tx,
        }))
        .router_preparation_hook_for(Count(count.clone()), OperationScope::Both);
    Ok((builder, rx, count))
}

#[tokio::test]
async fn incompatible_bound_checker_fails_before_preparation_or_callback() -> TestResult {
    let (builder, mut inputs, count) = bound_builder(1024, OperationScope::Generation)?;
    let error = builder
        .build()?
        .execute(PipelineRequest::new_classification(
            "test",
            CallerContext::local(),
            request()?,
        ))
        .await;
    assert!(matches!(error, Err(BitrouterError::BadRequest { .. })));
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(inputs.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn decisions_projection_covers_names_and_preserves_boolean_choice_identity() -> TestResult {
    let (mut builder, mut inputs, _) = bound_builder(1024, OperationScope::Both)?;
    let req = DecisionsCodec::parse_request(
        json!({"model":"test","input":[{"role":"user","content":[{"type":"input_text","text":"evidence"},{"type":"input_image","image_url":"data:image/png;base64,eA=="}]}],"safety_identifier":"excluded-safety","questions":[{"type":"choice","name":"name","instructions":"instruction","choices":[{"value":true,"description":"description"},{"value":"true"}]},{"type":"score","name":"rubric","instructions":"score instruction","levels":[{"label":"low","description":"low criterion"},{"label":"high","description":"high criterion"}]}]}),
    )?;
    let response = json!({"model":"native-test","answers":[{"type":"choice","name":"name","choice":true,"confidence":0.9,"probabilities":[{"value":true,"probability":0.8},{"value":"true","probability":0.2}]},{"type":"score","name":"rubric","score":0.25,"confidence":0.55,"probabilities":[{"value":0,"label":"low","probability":0.75},{"value":1,"label":"high","probability":0.25}]}],"usage":native_response(true)["usage"]});
    builder.executor(Arc::new(MockExecutor::new(vec![
        MockResponse::Classification(DecisionsCodec::parse_response(response, &req)?),
    ])));
    let pipeline = builder.build()?;
    pipeline
        .execute(PipelineRequest::new_classification(
            "test",
            CallerContext::local(),
            req,
        ))
        .await?;
    let input = inputs.recv().await.ok_or("checker was not invoked")?;
    assert_eq!(input.operation, ModelOperation::Classification);
    assert_eq!(input.coverage.excluded_media_fragments, 1);
    let kinds = input
        .content
        .iter()
        .map(|fragment| fragment.kind)
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            ContentFragmentKind::ClassifierEvidence,
            ContentFragmentKind::ClassifierQuestionName,
            ContentFragmentKind::ClassifierInstructions,
            ContentFragmentKind::ClassifierBooleanChoice,
            ContentFragmentKind::ClassifierChoiceDescription,
            ContentFragmentKind::ClassifierStringChoice,
            ContentFragmentKind::ClassifierQuestionName,
            ContentFragmentKind::ClassifierInstructions,
            ContentFragmentKind::ClassifierLevelLabel,
            ContentFragmentKind::ClassifierLevelDescription,
            ContentFragmentKind::ClassifierLevelLabel,
            ContentFragmentKind::ClassifierLevelDescription
        ]
    );
    assert!(!format!("{input:?}").contains("excluded-safety"));
    assert_eq!(input.content[3].text, input.content[5].text);
    assert_eq!(
        input
            .content
            .iter()
            .skip(6)
            .filter_map(|fragment| fragment.text.as_deref())
            .collect::<Vec<_>>(),
        vec![
            "rubric",
            "score instruction",
            "low",
            "low criterion",
            "high",
            "high criterion"
        ]
    );
    pipeline.drain_required_pending_settlements().await?;
    let (builder, mut inputs, _) = bound_builder(1, OperationScope::Both)?;
    assert!(matches!(
        builder
            .build()?
            .execute(PipelineRequest::new_classification(
                "test",
                CallerContext::local(),
                request()?
            ))
            .await,
        Err(BitrouterError::BadRequest { .. })
    ));
    assert!(inputs.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn client_disconnect_and_shutdown_join_admitted_work_and_settlement() -> TestResult {
    let mut fixture = Fixture::start(vec![(StatusCode::OK, native_response(true))], true).await?;
    let routes = Arc::new(StaticRoutingTable::new());
    routes.insert(
        "test",
        vec![target(&fixture.base, "a", ApiProtocol::Decisions)],
    );
    let executor = Arc::new(HttpExecutor::with_defaults()?);
    let (tx, mut settled) = mpsc::unbounded_channel();
    let app = App::builder()
        .skip_auth(true)
        .model_call(|lm| {
            lm.routing_table(routes)
                .executor(executor)
                .served_operations(OperationScope::Both)
                .settlement_recorder_for(Record(tx), OperationScope::Both);
        })
        .build()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let gateway = tokio::spawn(async move {
        app.serve_listener_with_router_wrapper_and_shutdown(listener, |router| router, async {
            let _ = shutdown_rx.await;
        })
        .await
    });
    let body = DecisionsCodec::render_request(&request()?)?;
    let client = tokio::spawn(async move {
        reqwest::Client::builder()
            .no_proxy()
            .build()?
            .post(format!("http://{addr}/v1/decisions"))
            .json(&body)
            .send()
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), fixture.accepted.recv())
        .await?
        .ok_or("upstream was not admitted")?;
    client.abort();
    let cancelled = client.await;
    assert!(cancelled.is_err_and(|error| error.is_cancelled()));
    shutdown_tx
        .send(())
        .map_err(|_| "shutdown receiver closed")?;
    assert!(!gateway.is_finished());
    assert!(settled.try_recv().is_err());
    fixture
        .state
        .release
        .as_ref()
        .ok_or("fixture was not gated")?
        .notify_one();
    let evidence = tokio::time::timeout(Duration::from_secs(10), settled.recv())
        .await?
        .ok_or("settlement lost after disconnect")?;
    assert_eq!(evidence.input_tokens, 10);
    assert!(!evidence.failed);
    tokio::time::timeout(Duration::from_secs(10), gateway).await???;
    assert_eq!(fixture.requests().len(), 1);
    assert!(settled.try_recv().is_err());
    fixture.stop().await
}

#[test]
fn protocol_tariff_overrides_are_independent_and_usage_pricing_uses_the_wire() -> TestResult {
    let config: Config = serde_json::from_value(json!({"providers":{"native":{
        "active":true,"api_base":"https://fixture.invalid/v1",
        "models":[{"id":"test","provider_model_id":"native-test",
            "pricing":{"input_micro_usd_per_token":2.0,"output_micro_usd_per_token":9.0,"cache_read_micro_usd_per_token":0.2},
            "pricing_by_protocol":{
                "responses":{"input_micro_usd_per_token":0.5},
                "decisions":{"input_micro_usd_per_token":0.1,"cache_read_micro_usd_per_token":0.0,"cache_write_micro_usd_per_token":0.0,"output_micro_usd_per_token":0.0}
            }}]
    }}}))?;
    let model = config
        .providers
        .get("native")
        .and_then(|provider| provider.models.first())
        .ok_or("model missing")?;
    let generation = model
        .pricing_for(&ApiProtocol::ChatCompletions)
        .ok_or("generation pricing missing")?;
    assert_eq!(generation.output_micro_usd_per_token, Some(9.0));
    let response = model
        .pricing_for(&ApiProtocol::Responses)
        .ok_or("response tariff missing")?;
    assert_eq!(response.input_micro_usd_per_token, Some(0.5));
    assert_eq!(response.output_micro_usd_per_token, None);
    assert_eq!(response.cache_read_micro_usd_per_token, None);
    let mut without_native = model.clone();
    without_native
        .pricing_by_protocol
        .remove(&ApiProtocol::Decisions);
    assert!(
        without_native
            .pricing_for(&ApiProtocol::Decisions)
            .is_none()
    );
    let table = ConfigRoutingTable::from_config(config);
    let prices = table
        .usage_pricing(
            "test",
            &target(
                "https://fixture.invalid/v1",
                "native",
                ApiProtocol::Responses,
            ),
        )
        .ok_or("stream pricing missing")?;
    assert_eq!(prices.base.input_micro_usd_per_token, Some(0.5));
    assert_eq!(prices.base.output_micro_usd_per_token, None);
    Ok(())
}

#[tokio::test]
async fn sdk_live_usage_prices_reject_mismatched_declared_profiles() -> TestResult {
    let config: Config = serde_json::from_value(json!({"providers":{"fixture":{
        "active":true,"api_base":"https://api.openai.com/v1", "api_protocol":[{"*":"responses"}],
        "models":[{"id":"test","pricing_by_protocol":{"responses":{"endpoint_profile":"openai_global",
            "input_micro_usd_per_token":0.1,"output_micro_usd_per_token":0}}}]
    }}}))?;
    let table = ConfigRoutingTable::from_config(config);
    let mut chain = table
        .route_chain("test", &RoutingPrefs::default(), &CallerContext::local())
        .await?;
    let target = chain.first_mut().ok_or("missing route")?;
    assert!(table.usage_pricing("test", target).is_some());
    target.api_base_override = Some("https://eu.api.openai.com/v1".into());
    assert!(table.usage_pricing("test", target).is_none());
    target.api_base_override = Some("https://unknown.invalid/v1".into());
    assert!(table.usage_pricing("test", target).is_none());
    Ok(())
}

#[tokio::test]
async fn classifier_endpoint_identity_cannot_bypass_usage_admission() -> TestResult {
    let fixture = Fixture::start(vec![(StatusCode::OK, native_response(true))], false).await?;
    let pipeline = builder(vec![target(&fixture.base, "a", ApiProtocol::Decisions)])?.build()?;
    let mut caller_request =
        PipelineRequest::new_classification("test", CallerContext::local(), request()?);
    caller_request.inbound_protocol = Some(ApiProtocol::SystemOne);
    assert!(matches!(
        pipeline.execute(caller_request).await,
        Err(BitrouterError::BadRequest { .. })
    ));
    assert!(fixture.requests().is_empty());
    fixture.stop().await
}

#[test]
fn semantic_scope_and_checker_fragments_read_legacy_names() -> TestResult {
    assert_eq!(
        serde_json::from_value::<OperationScope>(json!("decisions"))?,
        OperationScope::Classification
    );
    assert_eq!(
        serde_json::to_value(OperationScope::Classification)?,
        json!("classification")
    );
    assert_eq!(
        serde_json::from_value::<ContentFragmentKind>(json!("decision_evidence"))?,
        ContentFragmentKind::ClassifierEvidence
    );
    assert_eq!(
        serde_json::to_value(ContentFragmentKind::ClassifierEvidence)?,
        json!("classifier_evidence")
    );
    Ok(())
}

#[tokio::test]
async fn successful_classifier_headers_with_broken_body_never_replay_work() -> TestResult {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}/v1", listener.local_addr()?);
    let primary = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut request = Vec::new();
        loop {
            let mut buffer = [0; 4096];
            let read = stream.read(&mut buffer).await?;
            if read == 0 || request.len() > 64 * 1024 {
                return Err(std::io::Error::other("incomplete fixture request"));
            }
            request.extend_from_slice(&buffer[..read]);
            if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]);
                let length = headers
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    .ok_or_else(|| std::io::Error::other("missing fixture content length"))?;
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n{\"model\":").await?;
        stream.shutdown().await
    });
    let fallback = Fixture::start(vec![(StatusCode::OK, systemone_response(true))], false).await?;
    let (sender, mut settled) = mpsc::unbounded_channel();
    let mut builder = builder(vec![
        target(&base, "a-primary", ApiProtocol::SystemOne),
        target(&fallback.base, "z-fallback", ApiProtocol::SystemOne),
    ])?;
    builder
        .fallback_policy(Arc::new(RetryAll))
        .settlement_recorder_for(Record(sender), OperationScope::Both);
    let pipeline = Arc::new(builder.build()?);
    let (status, _, _) = post_classifier(
        router(pipeline.clone()),
        "/v1/systemone",
        systemone_request(),
    )
    .await?;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(fallback.requests().is_empty());
    pipeline.drain_required_pending_settlements().await?;
    let settlement = settled.recv().await.ok_or("missing failed settlement")?;
    assert!(settlement.failed);
    assert_eq!(settlement.protocol, Some(ApiProtocol::SystemOne));
    assert!(settled.try_recv().is_err());
    tokio::time::timeout(Duration::from_secs(10), primary).await???;
    fallback.stop().await
}
