//! Real HTTP sends must be preceded by shared intent and retain truthful facts.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use bitrouter_ai::{
    gateway::execution::{AttemptState, ExecutionState, TerminalOutcome},
    types::{ApiProtocol, ModelOperation, Prompt},
};
use futures::StreamExt;

use super::super::{
    admitted::SharedAdmission,
    authentication,
    authority::{Authority, Revision},
    credentials::Credentials,
    crypto::DeploymentKey,
    engine::{Admission, Engine, TenantLimits},
    ownership::{Registry, Submission},
    publication::{Configuration, CredentialBinding},
    store::{Kind, Store},
};
use crate::{
    auth::{
        db::{NewApiKey, insert_api_key, upsert_user},
        keys::GeneratedKey,
    },
    gateway::{
        caller::CallerContext,
        model::{client::HttpTimeouts, protocol::OutboundDispatch},
        pipeline::{
            context::PipelineContext,
            executor::{Executor, HttpExecutor},
            types::{PipelineInput, PipelineRequest, RoutingTarget},
        },
        providers::auth::{credentials::Credential, store::CredentialKey},
    },
};

mod delivery;
mod ingress;

#[derive(Clone)]
struct Upstream {
    engine: Engine,
    key_hash: String,
    calls: Arc<AtomicUsize>,
    fenced: Arc<AtomicUsize>,
}

async fn upstream(State(state): State<Upstream>, uri: Uri, headers: HeaderMap) -> Response {
    state.calls.fetch_add(1, Ordering::SeqCst);
    let execution = headers
        .get("x-bitrouter-request-id")
        .and_then(|value| value.to_str().ok());
    if let Some(id) = execution
        && let Ok(view) = state.engine.query(&state.key_hash, id).await
        && matches!(
            view.attempts.last().map(|attempt| &attempt.state),
            Some(AttemptState::Dispatched)
        )
    {
        state.fenced.fetch_add(1, Ordering::SeqCst);
    } else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    match uri.path().split('/').nth(1) {
        Some("redirect") => (StatusCode::TEMPORARY_REDIRECT, [("location", "/ok/chat/completions")]).into_response(),
        Some("reject") => (StatusCode::TOO_MANY_REQUESTS, "quota").into_response(),
        Some("malformed") => (StatusCode::OK, "not-json").into_response(),
        Some("responses-failed") | Some("responses-missing-id") | Some("responses-not-terminal") => {
            let mut body = failed_body();
            if uri.path().starts_with("/responses-missing-id/") { body["id"] = serde_json::Value::Null; }
            if uri.path().starts_with("/responses-not-terminal/") { body["status"] = serde_json::json!("in_progress"); }
            axum::Json(body).into_response()
        }
        Some("decisions") | Some("invalid-decisions") => axum::Json(serde_json::json!({
            "model":"fixture", "answers":[{"type":"predicate","name":"q","probability": if uri.path().starts_with("/invalid-decisions/") {2.0} else {0.7}}],
            "usage":{"input_tokens":10,"output_tokens":0,"total_tokens":10,"input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}
        })).into_response(),
        Some("stream-error") => ([("content-type", "text/event-stream")], "event: error\ndata: {\"error\":{\"type\":\"rate_limit_error\",\"message\":\"fixture failure\"}}\n\n").into_response(),
        Some("chat-usage-error") => {
            let partial = serde_json::json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":1,"total_tokens":8}});
            let failed = serde_json::json!({"error":{"type":"server_error","message":"fixture failure"},"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}});
            ([("content-type", "text/event-stream")], format!("data: {partial}\n\ndata: {failed}\n\n")).into_response()
        }
        Some("responses-usage-error") => {
            let failed = serde_json::json!({"type":"response.failed","response":failed_body()});
            ([("content-type", "text/event-stream")], format!("event: response.failed\ndata: {failed}\n\n")).into_response()
        }
        Some("chat-batch") | Some("messages-batch") => {
            let data = if uri.path().starts_with("/chat-batch/") {
                let body = serde_json::json!({"id":"batch-fixture","choices":[{"index":0,"delta":{"content":"batch"},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}});
                format!("data: {body}\n\n")
            } else {
                let start = serde_json::json!({"type":"message_start","message":{"id":"batch-fixture","usage":{"input_tokens":7,"output_tokens":0}}});
                let terminal = serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}});
                format!("event: message_start\ndata: {start}\n\nevent: message_delta\ndata: {terminal}\n\n")
            };
            let mut response = Response::new(Body::from_stream(futures::stream::once(async move { Ok::<_, std::io::Error>(data) }).chain(futures::stream::pending())));
            response.headers_mut().insert("content-type", http::HeaderValue::from_static("text/event-stream"));
            response
        }
        Some("responses-drop") => {
            let data = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"response-fixture\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":7,\"output_tokens\":3,\"total_tokens\":10}}}\n\n";
            let mut response = Response::new(Body::from_stream(futures::stream::once(async move { Ok::<_, std::io::Error>(data) }).chain(futures::stream::pending())));
            response.headers_mut().insert("content-type", http::HeaderValue::from_static("text/event-stream"));
            response
        }
        Some("responses-trailing-error") => {
            let terminal = serde_json::json!({"type":"response.completed","response":{"id":"original-response","status":"completed","output":[],"usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}});
            let rejected = serde_json::json!({"type":"response.failed","response":{"id":"unrelated-response","status":"failed","error":{"code":"server_error","message":"unrelated failure"},"usage":{"input_tokens":70,"output_tokens":30,"total_tokens":100}}});
            ([("content-type", "text/event-stream")], format!("event: response.completed\ndata: {terminal}\n\nevent: response.failed\ndata: {rejected}\n\n")).into_response()
        }
        Some("stream") | Some("dropped") | Some("truncated") | Some("live") => {
            let start = "data: {\"id\":\"stream-fixture\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n";
            let terminal = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
            let usage = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10}}\n\ndata: [DONE]\n\n";
            let data = if uri.path().starts_with("/truncated/") || uri.path().starts_with("/live/") { start.to_owned() }
                else if uri.path().starts_with("/dropped/") { format!("{start}{terminal}") }
                else { format!("{start}{terminal}{usage}") };
            let mut response = if uri.path().starts_with("/dropped/") || uri.path().starts_with("/live/") {
                let body = futures::stream::once(async move { Ok::<_, std::io::Error>(data) })
                    .chain(futures::stream::pending());
                Response::new(Body::from_stream(body))
            } else { Response::new(Body::from(data)) };
            response.headers_mut().insert("content-type", http::HeaderValue::from_static("text/event-stream"));
            response
        }
        _ => axum::Json(serde_json::json!({
            "id":"completion-fixture", "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10,"prompt_tokens_details":{"cached_tokens":2}}
        })).into_response(),
    }
}

struct Server(tokio::task::JoinHandle<()>);

fn failed_body() -> serde_json::Value {
    serde_json::json!({"id":"failure-fixture","status":"failed","output":[],"error":{"code":"server_error","message":"fixture failure"},"usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}})
}

fn decision_request() -> Result<bitrouter_ai::decisions::DecisionRequest> {
    Ok(
        crate::gateway::model::protocol::decisions::DecisionsCodec::parse_request(
            serde_json::json!({
                "model":"fixture", "input":"fixture evidence", "questions":[{"type":"predicate","name":"q","instructions":"evaluate"}]
            }),
        )?,
    )
}

impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Clone)]
struct Fixture {
    engine: Engine,
    custody: Arc<Registry>,
    key: GeneratedKey,
    pool: String,
    revision: Revision,
    executor: Arc<HttpExecutor>,
    base: String,
}

impl Fixture {
    async fn context(&self) -> Result<(Arc<SharedAdmission>, PipelineContext, Prompt)> {
        self.operation_context(ModelOperation::Generation).await
    }

    async fn operation_context(
        &self,
        operation: ModelOperation,
    ) -> Result<(Arc<SharedAdmission>, PipelineContext, Prompt)> {
        let admission = match self
            .custody
            .reserve()?
            .admit(&Admission {
                key_hash: &self.key.hash,
                correlation_id: "untrusted-correlation",
                idempotency_key: None,
                fingerprint: &"a".repeat(64),
                operation,
                revision: self.revision,
            })
            .await?
        {
            Submission::Accepted(admission) => admission,
            Submission::Existing(_) => anyhow::bail!("unexpected fixture duplicate"),
        };
        let prompt = Prompt {
            model: "fixture".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![],
            tools: vec![],
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        let mut request =
            PipelineRequest::new("fixture", CallerContext::anonymous(), prompt.clone());
        if operation == ModelOperation::Decisions {
            request.input = PipelineInput::Decisions(decision_request()?);
        }
        request.headers.insert(
            "authorization",
            http::HeaderValue::from_str(&format!("Bearer {}", self.key.secret))?,
        );
        admission.attach(&mut request);
        let mut ctx = PipelineContext::new(request);
        admission.establish(&mut ctx).await?;
        Ok((admission, ctx, prompt))
    }

    fn target(&self, path: &str) -> RoutingTarget {
        RoutingTarget {
            provider_name: "dispatch-fixture".into(),
            service_id: "fixture".into(),
            api_base: format!("{}/{path}", self.base),
            api_key: String::new(),
            api_protocol: ApiProtocol::ChatCompletions,
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
            chat_google_extensions: false,
            reasoning_effort: None,
            model_constraints: Default::default(),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
            headers: vec![],
        }
    }
}

pub(super) async fn exercise(first: &Store, second: &Store) -> Result<()> {
    let authority = Authority::new(first.clone(), DeploymentKey::from_bytes([9; 32])?);
    let revision = authority.head().await?.revision();
    let engine = Engine::join(authority.clone(), 30_000).await?;
    let tenant = uuid::Uuid::new_v4().to_string();
    let key = crate::auth::keys::generate();
    upsert_user(first.database(), &tenant).await?;
    insert_api_key(
        first.database(),
        &NewApiKey {
            id: uuid::Uuid::new_v4().to_string(),
            key_hash: key.hash.clone(),
            user_id: tenant.clone(),
            spend_limit_micro_usd: None,
            rpm_limit: None,
            policy_id: None,
        },
    )
    .await?;
    let pool = uuid::Uuid::new_v4().to_string();
    engine.set_resource(&pool, None, 10).await?;
    engine
        .set_tenant(
            &tenant,
            None,
            &TenantLimits {
                concurrent_executions: 20,
                requests_per_minute: 100,
                resource_pools: [pool.clone()].into(),
            },
        )
        .await?;
    let credentials = Credentials::new(authority);
    let slot = CredentialKey {
        provider: pool.clone(),
        account: "default".into(),
    };
    credentials
        .import(
            &slot,
            &pool,
            None,
            &Credential::api_key("fixture-only-provider-key"),
        )
        .await?;
    let config = Configuration {
        format: 1,
        routing: serde_json::json!({"providers":{"dispatch-fixture":{"api_base":"http://127.0.0.1:1","auto_discover":false}}}),
        policies: vec![],
        policy_lock: None,
        credentials: vec![CredentialBinding {
            provider: "dispatch-fixture".into(),
            account: "default".into(),
            credential_provider: slot.provider,
            credential_account: slot.account,
        }],
        require_known_pricing: false,
    };
    let registry = authentication::build(&config.prepare()?, &config.credentials, credentials)?;
    let executor = HttpExecutor::with_dispatch_and_auth(
        HttpTimeouts::default(),
        OutboundDispatch::builtin(),
        registry,
    )?
    .require_shared_authority();
    let remote = Engine::join(
        Authority::new(second.clone(), DeploymentKey::from_bytes([9; 32])?),
        30_000,
    )
    .await?;
    let state = Upstream {
        engine: remote,
        key_hash: key.hash.clone(),
        calls: Default::default(),
        fenced: Default::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let router = Router::new().fallback(upstream).with_state(state.clone());
    let _server = Server(tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    }));
    let fixture = Fixture {
        custody: Registry::new(engine.clone(), 64)?,
        engine,
        key,
        pool,
        revision,
        executor: Arc::new(executor),
        base,
    };

    // Keep each independent protocol scenario out of the enclosing future's
    // stack layout. The PostgreSQL campaign retains its ordinary stack budget.
    Box::pin(generation_success(&fixture)).await?;
    Box::pin(delivery::exercise(&fixture, second, &tenant)).await?;
    Box::pin(decisions(&fixture)).await?;
    Box::pin(stream_failure(&fixture)).await?;
    Box::pin(responses_drop(&fixture)).await?;
    Box::pin(rejected_trailing_usage(&fixture)).await?;
    Box::pin(rejections(&fixture, &state)).await?;
    Box::pin(ambiguous(&fixture, &state, first)).await?;
    Box::pin(streaming(&fixture)).await?;
    Box::pin(batch_drop(&fixture)).await?;
    Box::pin(failed_responses(&fixture)).await?;
    // Poll HTTP handlers on an owned task, as the real connection service does,
    // without the cumulative stack frames of the surrounding database campaign.
    let mut ingress = tokio::task::JoinSet::new();
    let inputs = (
        fixture.clone(),
        state.clone(),
        first.clone(),
        tenant.clone(),
    );
    ingress
        .spawn(async move { ingress::exercise(&inputs.0, &inputs.1, &inputs.2, &inputs.3).await });
    ingress
        .join_next()
        .await
        .context("missing ingress task")???;
    ensure!(state.calls.load(Ordering::SeqCst) == state.fenced.load(Ordering::SeqCst));
    let before = first
        .get::<serde_json::Value>(Kind::Resource, &fixture.pool)
        .await?
        .context("pool missing")?
        .value;
    for _ in 0..fixture.custody.pending() {
        ensure!(fixture.custody.reconcile_one().await?);
    }
    ensure!(fixture.custody.pending() == 0);
    let after = first
        .get::<serde_json::Value>(Kind::Resource, &fixture.pool)
        .await?
        .context("pool missing")?
        .value;
    ensure!(
        before["active"] == after["active"],
        "local retirement released unknown provider capacity"
    );
    Ok(())
}

async fn generation_success(fixture: &Fixture) -> Result<()> {
    // A separate database connection observes intent before responding at all.
    let (admission, ctx, prompt) = fixture.context().await?;
    ensure!(
        admission.finish_delivery(true).await.is_err(),
        "no provider success was observed"
    );
    fixture
        .executor
        .execute(&fixture.target("ok"), &prompt, &ctx)
        .await?;
    ensure!(!admission.blocks_fallback());
    let view = fixture
        .engine
        .query(&fixture.key.hash, ctx.request_id())
        .await?;
    let Some(AttemptState::Terminal { result }) =
        view.attempts.first().map(|attempt| &attempt.state)
    else {
        anyhow::bail!("success has no durable terminal")
    };
    ensure!(
        result.usage.total_tokens == Some(10)
            && result.usage.cache_read_tokens == Some(2)
            && result.usage.cost_micro_usd.is_none()
    );
    admission.finish_delivery(true).await?;
    admission.finish_delivery(true).await?;
    ensure!(
        !fixture.custody.reconcile_one().await?,
        "live context retired early"
    );
    drop(admission);
    ensure!(
        !fixture.custody.reconcile_one().await?,
        "context lost custody"
    );
    drop(ctx);
    ensure!(fixture.custody.reconcile_one().await?);
    Ok(())
}

async fn decisions(fixture: &Fixture) -> Result<()> {
    for path in ["decisions", "invalid-decisions"] {
        let (admission, ctx, _) = fixture.operation_context(ModelOperation::Decisions).await?;
        let mut target = fixture.target(path);
        target.api_protocol = ApiProtocol::Decisions;
        let result = fixture
            .executor
            .execute_decisions(&target, &decision_request()?, &ctx)
            .await;
        ensure!(result.is_ok() == (path == "decisions"));
        ensure!(!admission.blocks_fallback());
        let view = fixture
            .engine
            .query(&fixture.key.hash, ctx.request_id())
            .await?;
        let Some(AttemptState::Terminal { result }) =
            view.attempts.first().map(|attempt| &attempt.state)
        else {
            anyhow::bail!("Decisions terminal missing")
        };
        ensure!(result.usage.total_tokens == Some(10));
        ensure!(matches!(result.outcome, TerminalOutcome::Succeeded) == (path == "decisions"));
        fixture
            .engine
            .finish(ctx.request_id(), &result.outcome)
            .await?;
    }

    // Every operation uses the bounded response reader, including Decisions.
    let (admission, mut ctx, _) = fixture.operation_context(ModelOperation::Decisions).await?;
    ctx.insert_extension(Arc::new(
        crate::gateway::pipeline::native::NativeResponseByteLimit(8),
    ));
    let mut target = fixture.target("decisions");
    target.api_protocol = ApiProtocol::Decisions;
    ensure!(
        fixture
            .executor
            .execute_decisions(&target, &decision_request()?, &ctx)
            .await
            .is_err()
    );
    admission.reconcile_dispatch().await?;
    ensure!(matches!(
        fixture
            .engine
            .query(&fixture.key.hash, ctx.request_id())
            .await?
            .state,
        ExecutionState::Unknown { .. }
    ));
    Ok(())
}

async fn stream_failure(fixture: &Fixture) -> Result<()> {
    for (path, protocol, expected_usage) in [
        ("stream-error", ApiProtocol::ChatCompletions, None),
        ("chat-usage-error", ApiProtocol::ChatCompletions, Some(10)),
        ("responses-usage-error", ApiProtocol::Responses, Some(10)),
    ] {
        let (admission, ctx, prompt) = fixture.context().await?;
        let mut target = fixture.target(path);
        target.api_protocol = protocol;
        let parts = fixture
            .executor
            .execute_stream(&target, &prompt, &ctx)
            .await?
            .collect::<Vec<_>>()
            .await;
        ensure!(parts.last().is_some_and(Result::is_err) && !admission.blocks_fallback());
        let view = fixture
            .engine
            .query(&fixture.key.hash, ctx.request_id())
            .await?;
        ensure!(
            matches!(view.attempts.first().map(|attempt| &attempt.state), Some(AttemptState::Terminal { result }) if matches!(result.outcome, TerminalOutcome::Failed { .. }) && result.usage.total_tokens == expected_usage),
            "failed stream did not preserve final usage: {path}"
        );
        fixture
            .engine
            .finish(
                ctx.request_id(),
                &super::super::dispatched::failed("provider_failed"),
            )
            .await?;
    }
    Ok(())
}

async fn responses_drop(fixture: &Fixture) -> Result<()> {
    let (admission, ctx, prompt) = fixture.context().await?;
    let mut target = fixture.target("responses-drop");
    target.api_protocol = ApiProtocol::Responses;
    let mut stream = fixture
        .executor
        .execute_stream(&target, &prompt, &ctx)
        .await?;
    ensure!(
        tokio::time::timeout(Duration::from_millis(100), stream.next())
            .await
            .is_err()
    );
    drop(stream);
    admission.reconcile_dispatch().await?;
    let view = fixture
        .engine
        .query(&fixture.key.hash, ctx.request_id())
        .await?;
    ensure!(
        matches!(view.attempts.first().map(|attempt| &attempt.state), Some(AttemptState::Terminal { result }) if result.usage.total_tokens == Some(10))
    );
    fixture
        .engine
        .finish(ctx.request_id(), &TerminalOutcome::Succeeded)
        .await?;
    Ok(())
}

async fn rejected_trailing_usage(fixture: &Fixture) -> Result<()> {
    let (admission, ctx, prompt) = fixture.context().await?;
    let mut target = fixture.target("responses-trailing-error");
    target.api_protocol = ApiProtocol::Responses;
    let parts = fixture
        .executor
        .execute_stream(&target, &prompt, &ctx)
        .await?
        .collect::<Vec<_>>()
        .await;
    ensure!(parts.last().is_some_and(Result::is_err));
    admission.reconcile_dispatch().await?;
    let view = fixture
        .engine
        .query(&fixture.key.hash, ctx.request_id())
        .await?;
    ensure!(
        matches!(view.attempts.first().map(|attempt| &attempt.state),
        Some(AttemptState::Terminal { result }) if result.usage.total_tokens == Some(10)
            && result.outcome == TerminalOutcome::Succeeded),
        "rejected trailing response replaced validated terminal evidence"
    );
    fixture
        .engine
        .finish(ctx.request_id(), &TerminalOutcome::Succeeded)
        .await?;
    Ok(())
}

async fn rejections(fixture: &Fixture, state: &Upstream) -> Result<()> {
    // Redirects do not become hidden sends, and definite rejections release the account.
    for path in ["redirect", "reject"] {
        let before = state.calls.load(Ordering::SeqCst);
        let (admission, ctx, prompt) = fixture.context().await?;
        ensure!(
            fixture
                .executor
                .execute(&fixture.target(path), &prompt, &ctx)
                .await
                .is_err()
        );
        ensure!(state.calls.load(Ordering::SeqCst) == before + 1 && !admission.blocks_fallback());
        let view = fixture
            .engine
            .query(&fixture.key.hash, ctx.request_id())
            .await?;
        ensure!(matches!(
            view.attempts[0].state,
            AttemptState::Terminal { .. }
        ));
        fixture
            .engine
            .finish(
                ctx.request_id(),
                &super::super::dispatched::failed("upstream_rejected"),
            )
            .await?;
    }
    Ok(())
}

async fn ambiguous(fixture: &Fixture, state: &Upstream, first: &Store) -> Result<()> {
    // Invalid success is ambiguous; another call with the same typed context is blocked.
    let (admission, ctx, prompt) = fixture.context().await?;
    ensure!(
        fixture
            .executor
            .execute(&fixture.target("malformed"), &prompt, &ctx)
            .await
            .is_err()
    );
    ensure!(admission.blocks_fallback());
    admission.reconcile_dispatch().await?;
    let before = state.calls.load(Ordering::SeqCst);
    ensure!(
        fixture
            .executor
            .execute(&fixture.target("ok"), &prompt, &ctx)
            .await
            .is_err()
    );
    ensure!(state.calls.load(Ordering::SeqCst) == before);
    let view = fixture
        .engine
        .query(&fixture.key.hash, ctx.request_id())
        .await?;
    ensure!(matches!(view.state, ExecutionState::Unknown { .. }));
    ensure!(
        first
            .get::<serde_json::Value>(Kind::Resource, &fixture.pool)
            .await?
            .context("pool missing")?
            .value["active"]
            == 2
    );
    Ok(())
}

async fn streaming(fixture: &Fixture) -> Result<()> {
    let (admission, ctx, prompt) = fixture.context().await?;
    let parts = fixture
        .executor
        .execute_stream(&fixture.target("stream"), &prompt, &ctx)
        .await?
        .collect::<Vec<_>>()
        .await;
    ensure!(
        parts.iter().all(Result::is_ok),
        "stream fixture failed: {parts:?}"
    );
    ensure!(
        parts
            .last()
            .is_some_and(|part| part.as_ref().is_ok_and(|part| part.is_terminal()))
    );
    ensure!(!admission.blocks_fallback());
    let view = fixture
        .engine
        .query(&fixture.key.hash, ctx.request_id())
        .await?;
    ensure!(
        matches!(&view.attempts[0].state, AttemptState::Terminal { result } if result.usage.total_tokens == Some(10))
    );
    fixture
        .engine
        .finish(ctx.request_id(), &TerminalOutcome::Succeeded)
        .await?;

    let (admission, ctx, prompt) = fixture.context().await?;
    let parts = fixture
        .executor
        .execute_stream(&fixture.target("truncated"), &prompt, &ctx)
        .await?
        .collect::<Vec<_>>()
        .await;
    ensure!(parts.last().is_some_and(Result::is_err));
    admission.reconcile_dispatch().await?;
    ensure!(matches!(
        fixture
            .engine
            .query(&fixture.key.hash, ctx.request_id())
            .await?
            .state,
        ExecutionState::Unknown { .. }
    ));

    // Drop after observing a terminal but before EOF: the captured fact survives.
    let (admission, ctx, prompt) = fixture.context().await?;
    let mut stream = fixture
        .executor
        .execute_stream(&fixture.target("dropped"), &prompt, &ctx)
        .await?;
    while let Ok(part) = tokio::time::timeout(Duration::from_millis(100), stream.next()).await {
        ensure!(
            part.is_some_and(|part| part.is_ok()),
            "fixture ended before client drop"
        );
    }
    drop(stream);
    admission.reconcile_dispatch().await?;
    ensure!(
        matches!(&fixture.engine.query(&fixture.key.hash, ctx.request_id()).await?.attempts[0].state, AttemptState::Terminal { result } if result.usage.total_tokens.is_none())
    );
    fixture
        .engine
        .finish(ctx.request_id(), &TerminalOutcome::Succeeded)
        .await?;
    Ok(())
}

async fn batch_drop(fixture: &Fixture) -> Result<()> {
    use bitrouter_ai::types::StreamPart;
    for (path, protocol) in [
        ("chat-batch", ApiProtocol::ChatCompletions),
        ("messages-batch", ApiProtocol::Messages),
    ] {
        let (admission, ctx, prompt) = fixture.context().await?;
        let mut target = fixture.target(path);
        target.api_protocol = protocol;
        let mut stream = fixture
            .executor
            .execute_stream(&target, &prompt, &ctx)
            .await?;
        loop {
            let part = tokio::time::timeout(Duration::from_secs(2), stream.next())
                .await?
                .context("batch ended before client drop")??;
            if matches!(&part, StreamPart::TextDelta { text } if text == "batch")
                || matches!(&part, StreamPart::Usage { usage } if usage.completion_tokens == 3)
            {
                break;
            }
        }
        // No next poll: the terminal/usage from that decoded event must already
        // belong to the dropped attempt, even though they were not all yielded.
        drop(stream);
        admission.reconcile_dispatch().await?;
        let view = fixture
            .engine
            .query(&fixture.key.hash, ctx.request_id())
            .await?;
        ensure!(
            matches!(view.attempts.first().map(|attempt| &attempt.state),
            Some(AttemptState::Terminal { result }) if result.usage.total_tokens == Some(10)
                && result.outcome == TerminalOutcome::Succeeded),
            "lost batch evidence: {path}"
        );
        fixture
            .engine
            .finish(ctx.request_id(), &TerminalOutcome::Succeeded)
            .await?;
    }
    Ok(())
}

async fn failed_responses(fixture: &Fixture) -> Result<()> {
    for path in [
        "responses-failed",
        "responses-missing-id",
        "responses-not-terminal",
    ] {
        let (admission, ctx, prompt) = fixture.context().await?;
        let mut target = fixture.target(path);
        target.api_protocol = ApiProtocol::Responses;
        ensure!(
            fixture
                .executor
                .execute(&target, &prompt, &ctx)
                .await
                .is_err()
        );
        admission.reconcile_dispatch().await?;
        let view = fixture
            .engine
            .query(&fixture.key.hash, ctx.request_id())
            .await?;
        if path == "responses-failed" {
            ensure!(!admission.blocks_fallback());
            ensure!(
                matches!(view.attempts.first().map(|attempt| &attempt.state),
                Some(AttemptState::Terminal { result }) if result.usage.total_tokens == Some(10)
                    && matches!(result.outcome, TerminalOutcome::Failed { .. }))
            );
            fixture
                .engine
                .finish(
                    ctx.request_id(),
                    &super::super::dispatched::failed("provider_failed"),
                )
                .await?;
        } else {
            ensure!(admission.blocks_fallback());
            ensure!(matches!(view.state, ExecutionState::Unknown { .. }));
        }
    }
    Ok(())
}
