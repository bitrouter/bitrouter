//! Verify canonical tenant defaults at actual provider HTTP boundaries.

use std::{sync::atomic::Ordering, time::Duration};

use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    routing::post,
};
use bitrouter_ai::types::ApiProtocol;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
use tower::ServiceExt;

use super::{MODEL, draft, headers, provision_protocol};

const TIMEOUT: Duration = Duration::from_secs(10);
const TOKEN_FIELDS: [&str; 3] = ["max_tokens", "max_completion_tokens", "max_output_tokens"];

#[derive(Clone)]
struct Provider {
    observed: mpsc::Sender<Value>,
    reply: Value,
}

async fn provider(
    State(state): State<Provider>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if state.observed.try_send(body).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"unexpected provider request"})),
        );
    }
    (StatusCode::OK, Json(state.reply))
}

async fn assert_outbound_defaults(upstream: ApiProtocol) -> Result<()> {
    let (protocol, path, reply) = match upstream {
        ApiProtocol::ChatCompletions => (
            "chat_completions",
            "/v1/chat/completions",
            json!({"id":"chatcmpl-defaults","object":"chat.completion","created":0,
                "model":"model-a","choices":[{"index":0,"message":{"role":"assistant",
                "content":"defaults reply"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}}),
        ),
        ApiProtocol::Messages => (
            "messages",
            "/v1/messages",
            json!({"id":"msg-defaults","type":"message","role":"assistant",
                "model":"model-a","content":[{"type":"text","text":"defaults reply"}],
                "stop_reason":"end_turn","stop_sequence":null,
                "usage":{"input_tokens":4,"output_tokens":2}}),
        ),
        ApiProtocol::Responses => (
            "responses",
            "/v1/responses",
            json!({"id":"resp-defaults","object":"response","status":"completed",
                "model":"model-a","output":[{"id":"msg-defaults","type":"message",
                "role":"assistant","status":"completed","content":[{"type":"output_text",
                "text":"defaults reply","annotations":[]}]}],
                "usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}}),
        ),
        _ => anyhow::bail!("unsupported fixture protocol"),
    };
    let (observed, mut received) = mpsc::channel(8);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://localhost:{}/v1", listener.local_addr()?.port());
    let provider_router = Router::new()
        .route(path, post(provider))
        .with_state(Provider { observed, reply });
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    let provider_task = AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, provider_router)
            .with_graceful_shutdown(stopped.cancelled_owned())
            .await
    }));

    // This shared fixture has conflicting router defaults for all three token
    // aliases (999/777/888) and temperature (1.9). The draft wins over all four.
    let (fixture, route, _) = provision_protocol(&url, protocol).await?;
    let revision = draft(&fixture, &route, "draft-defaults", 25).await?;
    ensure!(revision.document.generation_defaults.temperature == 0.0);
    let credential = fixture.issue("execute-defaults").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = headers(
        &fixture,
        credential.secret.as_deref().context("credential secret")?,
        &revision.revision_id,
    )?;
    let router = Router::new()
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(axum::middleware::from_fn_with_state(
            fixture.service.clone(),
            crate::tenancy::http::dispatch,
        ));

    for inbound in [
        ApiProtocol::ChatCompletions,
        ApiProtocol::Messages,
        ApiProtocol::Responses,
    ] {
        for explicit in [false, true] {
            let (path, token_field, mut body) = match inbound {
                ApiProtocol::ChatCompletions => (
                    "/v1/chat/completions",
                    "max_completion_tokens",
                    json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}]}),
                ),
                ApiProtocol::Messages => (
                    "/v1/messages",
                    "max_tokens",
                    json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}]}),
                ),
                ApiProtocol::Responses => (
                    "/v1/responses",
                    "max_output_tokens",
                    json!({"model":MODEL,"input":"hello"}),
                ),
                _ => anyhow::bail!("unsupported inbound fixture protocol"),
            };
            let (tokens, temperature) = if explicit { (7, 0.25) } else { (25, 0.0) };
            if explicit {
                body[token_field] = json!(tokens);
                body["temperature"] = json!(temperature);
            }
            let mut request = Request::builder()
                .method("POST")
                .uri(path)
                .body(Body::from(serde_json::to_vec(&body)?))?;
            *request.headers_mut() = headers.clone();
            request
                .headers_mut()
                .insert("content-type", "application/json".parse()?);
            let response = tokio::time::timeout(TIMEOUT, router.clone().oneshot(request)).await??;
            let status = response.status();
            let response = tokio::time::timeout(
                TIMEOUT,
                axum::body::to_bytes(response.into_body(), 1024 * 1024),
            )
            .await??;
            ensure!(
                status == StatusCode::OK,
                "{inbound:?} -> {upstream:?}, explicit={explicit}: {status}: {}",
                String::from_utf8_lossy(&response)
            );
            ensure!(String::from_utf8_lossy(&response).contains("defaults reply"));

            let sent = tokio::time::timeout(TIMEOUT, received.recv())
                .await?
                .context("provider did not observe a request")?;
            let output_field = match upstream {
                ApiProtocol::Responses => "max_output_tokens",
                ApiProtocol::ChatCompletions
                    if inbound == ApiProtocol::ChatCompletions && explicit =>
                {
                    "max_completion_tokens"
                }
                _ => "max_tokens",
            };
            ensure!(sent["model"] == "model-a");
            for field in TOKEN_FIELDS {
                if field == output_field {
                    ensure!(
                        sent.get(field).and_then(Value::as_u64) == Some(tokens),
                        "{inbound:?} -> {upstream:?}, explicit={explicit}: {field}: {sent}"
                    );
                } else {
                    ensure!(
                        sent.get(field).is_none(),
                        "{inbound:?} -> {upstream:?}, explicit={explicit}: leaked {field}: {sent}"
                    );
                }
            }
            ensure!(
                sent.get("temperature").and_then(Value::as_f64) == Some(temperature),
                "{inbound:?} -> {upstream:?}, explicit={explicit}: temperature: {sent}"
            );
        }
    }
    ensure!(matches!(
        received.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    fixture.service.shutdown().await?;
    stop.cancel();
    tokio::time::timeout(TIMEOUT, provider_task).await???;
    Ok(())
}

#[tokio::test]
async fn tenant_defaults_and_explicit_values_reach_chat_provider() -> Result<()> {
    assert_outbound_defaults(ApiProtocol::ChatCompletions).await
}

#[tokio::test]
async fn tenant_defaults_and_explicit_values_reach_messages_provider() -> Result<()> {
    assert_outbound_defaults(ApiProtocol::Messages).await
}

#[tokio::test]
async fn tenant_defaults_and_explicit_values_reach_responses_provider() -> Result<()> {
    assert_outbound_defaults(ApiProtocol::Responses).await
}
