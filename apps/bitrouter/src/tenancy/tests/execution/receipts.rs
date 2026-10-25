//! Actual provider sends and protected receipt reads, never DTO-only authority.
use super::{Fixture, MODEL, draft, failure, headers, provision_protocol};
use crate::tenancy::receipts::{read, store};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use bitrouter_ai::tenancy::{
    authority::CredentialIssued,
    configuration::ConfigurationRevision,
    execution::{ExecutionPage, ExecutionReceipt},
    identity::ExecutionId,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::task::AbortOnDropHandle;

mod attempts;
mod authority;
mod barrier;
mod fault_campaign;
mod ingress_overflow;
mod ingress_socket;
mod lifecycle;
mod reads;
mod wire;

const TIMEOUT: Duration = Duration::from_secs(10);
#[derive(Clone)]
struct Reply {
    status: StatusCode,
    content_type: &'static str,
    body: String,
    received: mpsc::Sender<Value>,
    calls: Arc<AtomicUsize>,
    gate: Option<Arc<Semaphore>>,
}
async fn provider(
    State(reply): State<Reply>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    let authenticated = headers
        .get("authorization")
        .is_some_and(|v| v == "Bearer owned-upstream-secret")
        || headers
            .get("x-api-key")
            .is_some_and(|v| v == "owned-upstream-secret");
    if !authenticated {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    reply.calls.fetch_add(1, Ordering::SeqCst);
    if reply.received.try_send(request).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if let Some(gate) = reply.gate {
        match gate.acquire().await {
            Ok(permit) => permit.forget(),
            Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        }
    }
    (
        reply.status,
        [("content-type", reply.content_type)],
        reply.body,
    )
        .into_response()
}
struct Harness {
    fixture: Fixture,
    revision: ConfigurationRevision,
    credential: CredentialIssued,
    headers: HeaderMap,
    base: String,
    path: &'static str,
    request: Value,
    route: bitrouter_ai::tenancy::configuration::ResourceRef,
    slot: bitrouter_ai::tenancy::configuration::ResourceRef,
    calls: Arc<AtomicUsize>,
    received: mpsc::Receiver<Value>,
    _provider: AbortOnDropHandle<std::io::Result<()>>,
    _gateway: AbortOnDropHandle<std::io::Result<()>>,
}
impl Harness {
    async fn new(protocol: &str, status: StatusCode, body: Value) -> Result<Self> {
        Self::with_reply(protocol, status, serde_json::to_string(&body)?, false, None).await
    }
    async fn with_reply(
        protocol: &str,
        status: StatusCode,
        body: String,
        stream: bool,
        gate: Option<Arc<Semaphore>>,
    ) -> Result<Self> {
        Self::with_namespace_resources(protocol, status, body, stream, gate, false).await
    }
    async fn with_namespace_resources(
        protocol: &str,
        status: StatusCode,
        body: String,
        stream: bool,
        gate: Option<Arc<Semaphore>>,
        additional_resources: bool,
    ) -> Result<Self> {
        let (path, request) = match protocol {
            "chat_completions" => (
                "/v1/chat/completions",
                json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":stream}),
            ),
            "messages" => (
                "/v1/messages",
                json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"max_tokens":25,"stream":stream}),
            ),
            "responses" => (
                "/v1/responses",
                json!({"model":MODEL,"input":"hello","stream":stream}),
            ),
            "decisions" => (
                "/v1/decisions",
                json!({"model":MODEL,"input":"evidence","questions":[{"type":"predicate","name":"q","instructions":"Evaluate evidence"}]}),
            ),
            _ => anyhow::bail!("unknown fixture protocol"),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}/v1", listener.local_addr()?);
        let (received, receive) = mpsc::channel(16);
        let calls = Arc::new(AtomicUsize::new(0));
        let router = Router::new().route(path, post(provider)).with_state(Reply {
            status,
            content_type: if stream {
                "text/event-stream"
            } else {
                "application/json"
            },
            body,
            received,
            calls: calls.clone(),
            gate,
        });
        let provider =
            AbortOnDropHandle::new(tokio::spawn(
                async move { axum::serve(listener, router).await },
            ));
        let (mut fixture, route, slot) = provision_protocol(&base, protocol).await?;
        if additional_resources {
            authority::provision_resources(&mut fixture, &route, &slot, &base).await?;
        }
        let revision = draft(&fixture, &route, "receipt-draft", 25).await?;
        let credential = fixture.issue("receipt-client").await?;
        fixture
            .service
            .dispatch_test_ready
            .store(true, Ordering::Release);
        let headers = headers(
            &fixture,
            credential.secret.as_deref().context("credential")?,
            &revision.revision_id,
        )?;
        let operators = crate::remote_control::auth::ControlAuth::from_lookup(
            &crate::config::ControlConfig::default(),
            |_| Some("receipt-fixture-operator-token-at-least-32-bytes".into()),
        )?;
        let dispatch = Router::new()
            .route(path, post(|| async { StatusCode::NOT_FOUND }))
            .layer(axum::middleware::from_fn_with_state(
                fixture.service.clone(),
                crate::tenancy::http::dispatch,
            ));
        let router =
            crate::tenancy::http::router(fixture.service.clone(), operators).merge(dispatch);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let gateway =
            AbortOnDropHandle::new(tokio::spawn(
                async move { axum::serve(listener, router).await },
            ));
        Ok(Self {
            fixture,
            revision,
            credential,
            headers,
            base,
            path,
            request,
            route,
            slot,
            calls,
            received: receive,
            _provider: provider,
            _gateway: gateway,
        })
    }
    fn client(http2: bool) -> Result<reqwest::Client> {
        let builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TIMEOUT);
        #[cfg(feature = "http2")]
        let builder = if http2 {
            builder.http2_prior_knowledge()
        } else {
            builder.http1_only()
        };
        #[cfg(not(feature = "http2"))]
        ensure!(!http2);
        Ok(builder.build()?)
    }
    fn send(&self, client: &reqwest::Client) -> reqwest::RequestBuilder {
        client
            .post(format!("{}{}", self.base, self.path))
            .headers(self.headers.clone())
            .json(&self.request)
    }
    async fn receipt(
        &self,
        client: &reqwest::Client,
        id: &ExecutionId,
    ) -> Result<ExecutionReceipt> {
        let response = client
            .get(format!(
                "{}/control/v1/tenants/{}/executions/{id}",
                self.base, self.fixture.tenant
            ))
            .bearer_auth(self.credential.secret.as_deref().context("secret")?)
            .header(
                "bitrouter-installation-id",
                self.fixture.installation.as_str(),
            )
            .send()
            .await?;
        ensure!(
            response.status() == StatusCode::OK,
            "receipt read: {}",
            response.status()
        );
        ensure!(
            response
                .headers()
                .get("cache-control")
                .is_some_and(|value| value == "no-store")
        );
        Ok(response.json().await?)
    }
    async fn page(&self) -> Result<ExecutionPage> {
        let caller = self
            .fixture
            .service
            .authenticate(self.credential.secret.as_deref().context("secret")?)
            .map_err(failure)?;
        let bytes = self
            .fixture
            .service
            .read_receipt(
                caller,
                self.fixture.tenant.clone(),
                read::Request::Page {
                    correlation: "same-correlation".try_into()?,
                    after: None,
                    limit: 100,
                },
            )
            .await
            .map_err(failure)?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}
fn json_response(protocol: &str, usage: Value) -> Result<Value> {
    Ok(match protocol {
        "chat_completions" => {
            json!({"id":"chatcmpl-receipts","object":"chat.completion","created":0,"model":"model-a","choices":[{"index":0,"message":{"role":"assistant","content":"receipt reply"},"finish_reason":"stop"}],"usage":usage})
        }
        "responses" => {
            json!({"id":"resp_receipts","object":"response","created_at":0,"status":"completed","model":"model-a","output":[{"id":"msg_receipts","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"receipt reply","annotations":[]}]}],"usage":usage})
        }
        "messages" => {
            json!({"id":"msg_receipts","type":"message","role":"assistant","model":"model-a","content":[{"type":"text","text":"receipt reply"}],"stop_reason":"end_turn","stop_sequence":null,"usage":usage})
        }
        "decisions" => {
            json!({"model":"model-a","answers":[{"type":"predicate","name":"q","probability":0.7}],"usage":usage})
        }
        _ => anyhow::bail!("unknown fixture protocol"),
    })
}
fn full_usage(protocol: &str) -> Value {
    if protocol == "messages" {
        json!({"input_tokens":11,"output_tokens":7,"cache_read_input_tokens":3,"cache_creation_input_tokens":2})
    } else if protocol == "chat_completions" {
        json!({"prompt_tokens":11,"completion_tokens":7,"total_tokens":18,"prompt_tokens_details":{"cached_tokens":3,"cache_write_tokens":2},"completion_tokens_details":{"reasoning_tokens":4}})
    } else {
        json!({"input_tokens":11,"output_tokens":7,"total_tokens":18,"input_tokens_details":{"cached_tokens":3,"cache_write_tokens":2},"output_tokens_details":{"reasoning_tokens":4}})
    }
}
fn execution_id(response: &reqwest::Response) -> Result<ExecutionId> {
    Ok(response
        .headers()
        .get("x-bitrouter-request-id")
        .context("admitted response identity")?
        .to_str()?
        .try_into()?)
}
