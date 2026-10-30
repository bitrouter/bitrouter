//! Real selected OAuth -> Messages -> durable receipt, with no host applier.
use super::{Fixture, MODEL, draft, failure, headers};
use crate::tenancy::{Service, oauth::storage, resources::Resources};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{Form, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use bitrouter_ai::tenancy::{
    authority::{CredentialIssued, OwnerKind, ResourceKind},
    configuration::ResourceRef,
    execution::ExecutionReceipt,
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

mod accounting;
#[cfg(unix)]
mod audit;
#[cfg(unix)]
mod custody;
mod faults;
mod namespace;
mod positive;

const TIMEOUT: Duration = Duration::from_secs(10);
#[derive(Clone)]
struct Provider {
    static_auth: bool,
    requests: Arc<AtomicUsize>,
    refreshes: Arc<AtomicUsize>,
    models: Arc<AtomicUsize>,
    refresh_seen: mpsc::Sender<()>,
    model_seen: mpsc::Sender<Value>,
    refresh_gate: Arc<Semaphore>,
    model_gate: Arc<Semaphore>,
    reply: Arc<std::sync::Mutex<(StatusCode, String)>>,
    chunked_refresh: Arc<std::sync::atomic::AtomicBool>,
    model_status: Arc<std::sync::Mutex<StatusCode>>,
}
async fn refresh(
    State(state): State<Provider>,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Response {
    if form.get("refresh_token").map(String::as_str) != Some("seed-refresh")
        || form.get("grant_type").map(String::as_str) != Some("refresh_token")
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    state.refreshes.fetch_add(1, Ordering::SeqCst);
    if state.refresh_seen.try_send(()).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    match state.refresh_gate.acquire().await {
        Ok(permit) => permit.forget(),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
    let reply = match state.reply.lock() {
        Ok(reply) => reply.clone(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if state.chunked_refresh.load(Ordering::Acquire) {
        let chunks: Vec<Result<axum::body::Bytes, std::io::Error>> = reply
            .1
            .as_bytes()
            .chunks(8192)
            .map(|chunk| Ok(axum::body::Bytes::copy_from_slice(chunk)))
            .collect();
        return (
            reply.0,
            [("content-type", "application/json")],
            axum::body::Body::from_stream(futures::stream::iter(chunks)),
        )
            .into_response();
    }
    match state.reply.lock() {
        Ok(reply) => (
            reply.0,
            [("content-type", "application/json")],
            reply.1.clone(),
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
async fn model(
    State(state): State<Provider>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    if (state.static_auth
        && headers
            .get("x-api-key")
            .is_none_or(|value| value != "rotated-access"))
        || (!state.static_auth
            && (headers
                .get("authorization")
                .is_none_or(|value| value != "Bearer rotated-access")
                || headers.contains_key("x-api-key")
                || headers.get("x-app").is_none_or(|value| value != "cli")
                || headers
                    .get("anthropic-beta")
                    .and_then(|value| value.to_str().ok())
                    .is_none_or(|value| !value.contains("oauth-2025-04-20"))))
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    state.models.fetch_add(1, Ordering::SeqCst);
    if state.model_seen.try_send(request.clone()).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    match state.model_gate.acquire().await {
        Ok(permit) => permit.forget(),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
    let status = match state.model_status.lock() {
        Ok(status) => *status,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if status != StatusCode::OK {
        return (status, Json(json!({"type":"error","error":{"type":"authentication_error","message":"rejected"}}))).into_response();
    }
    let usage = json!({"input_tokens":11,"output_tokens":7,"cache_read_input_tokens":3,"cache_creation_input_tokens":2});
    if request["stream"] == true {
        let events = vec![
            json!({"type":"message_start","message":{"id":"msg_oauth","type":"message","role":"assistant","model":"model-a","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":11,"output_tokens":0,"cache_read_input_tokens":3,"cache_creation_input_tokens":2}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"owned reply"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":7}}),
            json!({"type":"message_stop"}),
        ];
        let mut body = String::new();
        for event in events {
            let Some(kind) = event["type"].as_str() else {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            };
            body.push_str(&format!("event: {kind}\ndata: {event}\n\n"));
        }
        ([("content-type", "text/event-stream")], body).into_response()
    } else {
        Json(json!({"id":"msg_oauth","type":"message","role":"assistant","model":"model-a","content":[{"type":"text","text":"owned reply"}],"stop_reason":"end_turn","stop_sequence":null,"usage":usage})).into_response()
    }
}
struct Harness {
    fixture: Fixture,
    slot: ResourceRef,
    route: ResourceRef,
    credential: CredentialIssued,
    resources: Arc<Resources>,
    manifest: Vec<u8>,
    headers: HeaderMap,
    base: String,
    provider: Provider,
    refresh_seen: mpsc::Receiver<()>,
    model_seen: mpsc::Receiver<Value>,
    provider_stop: tokio_util::sync::CancellationToken,
    gateway_stop: tokio_util::sync::CancellationToken,
    _provider: AbortOnDropHandle<std::io::Result<()>>,
    _gateway: AbortOnDropHandle<std::io::Result<()>>,
}
impl Harness {
    async fn new(expired: bool) -> Result<Self> {
        Self::new_mode(expired, false).await
    }
    async fn new_mode(expired: bool, static_auth: bool) -> Result<Self> {
        Self::with_path(expired, static_auth, None).await
    }
    async fn with_path(
        expired: bool,
        static_auth: bool,
        path: Option<&std::path::Path>,
    ) -> Result<Self> {
        Self::with_token_endpoint(expired, static_auth, path, None).await
    }
    /// A fixture-owned raw peer can send deliberately incomplete HTTP bodies.
    /// The endpoint still enters through the ordinary privileged seed import.
    async fn with_token_endpoint(
        expired: bool,
        static_auth: bool,
        path: Option<&std::path::Path>,
        token_endpoint: Option<&str>,
    ) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let upstream = format!("http://{}", listener.local_addr()?);
        let (refresh_seen, refresh_rx) = mpsc::channel(8);
        let (model_seen, model_rx) = mpsc::channel(8);
        let provider=Provider { static_auth, requests: Arc::new(AtomicUsize::new(0)), chunked_refresh: Arc::new(std::sync::atomic::AtomicBool::new(false)), model_status:Arc::new(std::sync::Mutex::new(StatusCode::OK)), refreshes:Arc::new(AtomicUsize::new(0)), models:Arc::new(AtomicUsize::new(0)),refresh_seen,model_seen,
            refresh_gate:Arc::new(Semaphore::new(0)),model_gate:Arc::new(Semaphore::new(0)),
            reply:Arc::new(std::sync::Mutex::new((StatusCode::OK,json!({"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600}).to_string()))) };
        let requests = provider.requests.clone();
        let router = Router::new()
            .route("/oauth/token", post(refresh))
            .route("/v1/messages", post(model))
            .with_state(provider.clone())
            .layer(axum::middleware::from_fn(
                move |request: axum::extract::Request, next: axum::middleware::Next| {
                    let requests = requests.clone();
                    async move {
                        // Observe every request before authentication or body extraction.
                        requests.fetch_add(1, Ordering::SeqCst);
                        next.run(request).await
                    }
                },
            ));
        let provider_stop = tokio_util::sync::CancellationToken::new();
        let stopping = provider_stop.clone();
        let serving = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stopping.cancelled_owned())
                .await
        }));
        let owner = path
            .map(|path| crate::tenancy::ownership::acquire_path(path, true))
            .transpose()?;
        let mut fixture = if let Some(owner) = &owner {
            let db =
                crate::db::connect_tenancy(&format!("sqlite://{}", owner.path.display()), owner)
                    .await?;
            Fixture::with_database(db, Default::default(), &mut |_| {}).await?
        } else {
            Fixture::new().await?
        };
        fixture.service.shutdown().await?;
        let resources = Arc::new(Resources::with_key(fixture.installation.clone(), [29; 32])?);
        let reference = |kind, id: &str| -> Result<ResourceRef> {
            Ok(ResourceRef {
                owner_kind: OwnerKind::Tenant,
                owner_id: fixture.tenant.as_str().try_into()?,
                resource_kind: kind,
                resource_id: id.try_into()?,
                revision_id: "r1".try_into()?,
            })
        };
        let route = reference(ResourceKind::Route, MODEL)?;
        let slot = reference(ResourceKind::Credential, "oauth-slot")?;
        let registration =
            crate::gateway::providers::login::find("anthropic").context("registration")?;
        let expires = if expired {
            1
        } else {
            u64::try_from(chrono::Utc::now().timestamp())? + 3600
        };
        let token_endpoint = token_endpoint
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{upstream}/oauth/token"));
        let mut manifest = json!({"format":1,"installation_id":fixture.installation,"resources":[
            {"reference":slot,"definition":{"kind":"credential","credential":{"type":"oauth","data":{"access_token":if expired{"seed-access"}else{"rotated-access"},"refresh_token":"seed-refresh","expires_at":expires}},
                "oauth":{"format":1,"mechanism":"claude_code_refresh_v1","provider":"claude-code","account":"default","registration":"anthropic","token_endpoint":token_endpoint,"client_id":registration.auth.client_id}}},
            {"reference":route,"definition":{"kind":"route","selector":"bitrouter/owned","policy":null,"routing":{"providers":{"claude-code":{"api_base":format!("{upstream}/v1"),"api_protocol":[{"*":"messages"}],"models":[{"id":"model-a"}]}},"routers":{"owned":{"selection":{"kind":"model","model":"claude-code:model-a"}}}},"credentials":[{"provider":"claude-code","account":"default","credential":slot}]}}
        ]});
        if static_auth {
            manifest["resources"][0]["definition"] = json!({"kind":"credential","credential":{"type":"api_key","data":{"value":"rotated-access"}}});
        }
        let manifest = serde_json::to_vec(&manifest)?;
        resources.provision_bytes(&fixture.db, &manifest).await?;
        fixture.service = Service::start_prepared(
            fixture.db.clone(),
            owner,
            crate::tenancy::actor::OperationLimits::default(),
            crate::tenancy::store::load(&fixture.db, chrono::Utc::now()).await?,
            resources.clone(),
        )
        .await?;
        let revision = draft(&fixture, &route, "oauth-config", 25).await?;
        let credential = fixture.issue("oauth-client").await?;
        fixture
            .service
            .dispatch_test_ready
            .store(true, Ordering::Release);
        let headers = headers(
            &fixture,
            credential.secret.as_deref().context("secret")?,
            &revision.revision_id,
        )?;
        let (base, gateway, gateway_stop) = gateway(&fixture).await?;
        Ok(Self {
            fixture,
            slot,
            route,
            credential,
            resources,
            manifest,
            headers,
            base,
            provider,
            refresh_seen: refresh_rx,
            model_seen: model_rx,
            provider_stop,
            gateway_stop,
            _provider: serving,
            _gateway: gateway,
        })
    }
    fn client(h2: bool) -> Result<reqwest::Client> {
        let builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TIMEOUT);
        #[cfg(feature = "http2")]
        let builder = if h2 {
            builder.http2_prior_knowledge()
        } else {
            builder.http1_only()
        };
        #[cfg(not(feature = "http2"))]
        ensure!(!h2);
        Ok(builder.build()?)
    }
    fn send(&self, client: &reqwest::Client, stream: bool) -> reqwest::RequestBuilder {
        client.post(format!("{}/v1/messages",self.base)).headers(self.headers.clone())
            .json(&json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"system":"retain my instruction","max_tokens":25,"stream":stream}))
    }
    async fn receipt(&self, id: &ExecutionId) -> Result<ExecutionReceipt> {
        crate::tenancy::receipts::store::lookup(
            &self.fixture.db,
            &self.fixture.installation,
            &self.fixture.tenant,
            id,
        )
        .await?
        .context("receipt")
    }
    async fn row(&self) -> Result<storage::Row> {
        storage::load(&self.fixture.db, &self.fixture.installation, &self.slot)
            .await?
            .context("slot")
    }
    async fn reopen(&mut self) -> Result<()> {
        let revision = self
            .headers
            .get("bitrouter-execution-revision")
            .context("retained execution revision")?
            .to_str()?
            .try_into()?;
        self._gateway.abort();
        self.fixture.service.shutdown().await?;
        self.fixture.service = Service::start_prepared(
            self.fixture.db.clone(),
            None,
            crate::tenancy::actor::OperationLimits::default(),
            crate::tenancy::store::load(&self.fixture.db, chrono::Utc::now()).await?,
            self.resources.clone(),
        )
        .await?;
        self.fixture
            .service
            .dispatch_test_ready
            .store(true, Ordering::Release);
        // Reauthenticate the same credential/revision against this instance;
        // a prior instance's capability binding correctly remains invalid.
        self.headers = headers(
            &self.fixture,
            self.credential.secret.as_deref().context("secret")?,
            &revision,
        )?;
        let (base, gateway, gateway_stop) = gateway(&self.fixture).await?;
        self.base = base;
        self._gateway = gateway;
        self.gateway_stop = gateway_stop;
        Ok(())
    }
}
impl Harness {
    async fn close(mut self) -> Result<()> {
        self.provider_stop.cancel();
        self.gateway_stop.cancel();
        tokio::time::timeout(TIMEOUT, &mut self._gateway).await???;
        tokio::time::timeout(TIMEOUT, &mut self._provider).await???;
        Ok(())
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.provider.refresh_gate.close();
        self.provider.model_gate.close();
    }
}
async fn gateway(
    fixture: &Fixture,
) -> Result<(
    String,
    AbortOnDropHandle<std::io::Result<()>>,
    tokio_util::sync::CancellationToken,
)> {
    let router = Router::new()
        .route("/v1/messages", post(|| async { StatusCode::NOT_FOUND }))
        .layer(axum::middleware::from_fn_with_state(
            fixture.service.clone(),
            crate::tenancy::http::dispatch,
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let stop = tokio_util::sync::CancellationToken::new();
    let stopping = stop.clone();
    Ok((
        base,
        AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stopping.cancelled_owned())
                .await
        })),
        stop,
    ))
}
fn execution_id(response: &reqwest::Response) -> Result<ExecutionId> {
    Ok(response
        .headers()
        .get("x-bitrouter-request-id")
        .context("execution ID")?
        .to_str()?
        .try_into()?)
}
fn modes() -> &'static [bool] {
    if cfg!(feature = "http2") {
        &[false, true]
    } else {
        &[false]
    }
}
