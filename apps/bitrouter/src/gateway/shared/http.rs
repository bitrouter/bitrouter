//! Shared inference admission and response identity at the HTTP boundary.

pub(super) mod fingerprint;
mod identity;

use std::pin::Pin;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll};

use anyhow::ensure;
use axum::body::Body;
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bitrouter_ai::types::ApiProtocol;
use bytes::Bytes;
use http_body::{Body as HttpBody, Frame, SizeHint};

use crate::config::gateway_state::SharedGatewayConfig;
use crate::gateway::assembly::App;
use crate::gateway::error::BitrouterError;
use crate::gateway::http::{self as model_http, AppState};
use crate::gateway::pipeline::owned_tasks::{OwnedTasks, Source};

use self::identity::{
    CORRELATION_HEADER, Identity, credential_hash, request_identity, valid_identity,
};
use super::admitted::{SharedAdmission, public_error};
use super::authority::Revision;
use super::engine::{Admission, Engine, Rejection};
use super::ownership::{Generation, Registry, Reservation, Submission};

/// A request takes one immutable application/generation pair. Publication
/// replaces this pair only after the complete application has assembled.
pub(super) struct Snapshot {
    pub revision: Revision,
    pub owner: Generation,
    tasks: Arc<OwnedTasks>,
    state: AppState,
}

impl Snapshot {
    #[cfg(test)]
    pub fn new(revision: Revision, app: Arc<App>) -> anyhow::Result<Arc<Self>> {
        Self::assemble(revision, &app, Generation::new())
    }

    pub(super) fn assemble(
        revision: Revision,
        app: &App,
        owner: Generation,
    ) -> anyhow::Result<Arc<Self>> {
        ensure!(
            revision.config_generation > 0 && revision.key_generation > 0 && !app.skip_auth(),
            "invalid shared serving snapshot"
        );
        let pipeline = app
            .language_model()
            .ok_or_else(|| anyhow::anyhow!("shared application has no model pipeline"))?
            .clone();
        let tasks = pipeline
            .owned_tasks()
            .ok_or_else(|| anyhow::anyhow!("shared application has no owned task lifecycle"))?
            .clone();
        tasks.bind_snapshot()?;
        Ok(Arc::new(Self {
            revision,
            owner,
            state: AppState {
                language_model: pipeline,
                mcp: None,
                skip_auth: false,
                metrics_renderer: app.metrics_renderer().cloned(),
                prompt_transforms: app.prompt_transforms().to_vec(),
            },
            tasks,
        }))
    }

    pub fn tasks(&self) -> &Arc<OwnedTasks> {
        &self.tasks
    }
}

pub(super) struct Serving {
    pub engine: Engine,
    pub custody: Arc<Registry>,
    current: RwLock<Publication>,
    ready: AtomicBool,
    request_bytes: usize,
}

#[derive(Default)]
struct Publication {
    snapshot: Option<Arc<Snapshot>>,
    withdrawn: bool,
}

impl Serving {
    #[cfg(test)]
    pub fn new(
        engine: Engine,
        snapshot: Arc<Snapshot>,
        config: &SharedGatewayConfig,
    ) -> anyhow::Result<Arc<Self>> {
        let serving = Self::empty(engine, config)?;
        serving.install(snapshot)?;
        Ok(serving)
    }

    pub(super) fn empty(engine: Engine, config: &SharedGatewayConfig) -> anyhow::Result<Arc<Self>> {
        ensure!(
            (1024..=16_777_216).contains(&config.max_request_bytes),
            "invalid shared ingress bound"
        );
        Ok(Arc::new(Self {
            custody: Registry::new(engine.clone(), config.local_max_in_flight)?,
            engine,
            current: RwLock::new(Publication::default()),
            ready: AtomicBool::new(false),
            request_bytes: config.max_request_bytes,
        }))
    }

    pub fn set_ready(&self, ready: bool) {
        let current = self
            .current
            .read()
            .unwrap_or_else(|error| error.into_inner());
        self.ready.store(
            ready && !current.withdrawn && current.snapshot.is_some(),
            Ordering::Release,
        );
    }

    #[cfg(test)]
    pub fn snapshot(&self) -> anyhow::Result<Arc<Snapshot>> {
        self.current
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .snapshot
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("fixture has no serving snapshot"))
    }

    /// Publication and registration share this short gate. No old-generation
    /// reservation can appear after a swap; no lock is held across an await.
    /// Lock order is Serving.current, then Registry.entries.
    pub fn acquire_request(&self) -> anyhow::Result<(Arc<Snapshot>, Reservation, Source)> {
        let current = self
            .current
            .read()
            .unwrap_or_else(|error| error.into_inner());
        if !self.ready.load(Ordering::Acquire) {
            return Err(Rejection::Unavailable.into());
        }
        let current = current.snapshot.as_ref().ok_or(Rejection::Unavailable)?;
        let source = current.tasks.acquire()?;
        let reservation = self.custody.reserve_for(current.owner)?;
        Ok((current.clone(), reservation, source))
    }

    fn acquire_reader(&self) -> anyhow::Result<(Arc<Snapshot>, Source)> {
        let current = self
            .current
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let current = current.snapshot.as_ref().ok_or(Rejection::Unavailable)?;
        Ok((current.clone(), current.tasks.acquire()?))
    }

    /// This proves only local request/reader quiescence after publication.
    /// Native compensation and exporter drains are separate retirement barriers.
    /// The caller must be the sole remaining owner of the retired snapshot;
    /// snapshots and custody never expose weak handles or lend retired work.
    pub fn retired_quiescent(&self, snapshot: &Arc<Snapshot>) -> bool {
        let current = self
            .current
            .read()
            .unwrap_or_else(|error| error.into_inner());
        !current
            .snapshot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, snapshot))
            && self.custody.pending_for(snapshot.owner) == 0
            && snapshot.tasks.pending().sources == 0
            && Arc::strong_count(snapshot) == 1
    }

    /// The lifecycle owner receives the previous app for bounded finalizer drain.
    /// A stale or incomplete publication never mutates the active application.
    #[cfg(test)]
    pub fn publish(&self, snapshot: Arc<Snapshot>) -> anyhow::Result<Arc<Snapshot>> {
        self.install(snapshot)?
            .ok_or_else(|| anyhow::anyhow!("fixture publication has no predecessor"))
    }

    /// Only the whole-generation manager publishes production snapshots. Its
    /// resource transfer is synchronous with this gate and never awaits here.
    pub(super) fn install(&self, snapshot: Arc<Snapshot>) -> anyhow::Result<Option<Arc<Snapshot>>> {
        let mut current = self
            .current
            .write()
            .unwrap_or_else(|error| error.into_inner());
        ensure!(
            !current.withdrawn,
            "shared serving is permanently withdrawn"
        );
        ensure!(
            current
                .snapshot
                .as_ref()
                .is_none_or(|current| snapshot.revision.advances(current.revision)),
            "shared serving revision did not advance"
        );
        if let Some(current) = &current.snapshot {
            current.tasks.close_sources();
        }
        Ok(current.snapshot.replace(snapshot))
    }

    pub(super) fn withdraw(&self) {
        let previous = {
            let mut current = self
                .current
                .write()
                .unwrap_or_else(|error| error.into_inner());
            current.withdrawn = true;
            self.ready.store(false, Ordering::Release);
            if let Some(snapshot) = &current.snapshot {
                snapshot.tasks.close_sources();
            }
            current.snapshot.take()
        };
        drop(previous);
    }

    pub(super) fn loaded_revision(&self) -> Option<Revision> {
        self.current
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.revision)
    }

    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/v1/messages", post(messages))
            .route("/v1/chat/completions", post(chat))
            .route("/v1/responses", post(responses))
            .route("/v1/decisions", post(decisions))
            .route("/v1/gateway/executions/{id}", get(execution))
            .route("/v1/models", get(models))
            .route("/metrics", get(metrics))
            .route("/health", get(health))
            .layer(axum::extract::DefaultBodyLimit::max(self.request_bytes))
            .with_state(self.clone())
    }
}

async fn messages(State(state): State<Arc<Serving>>, request: Request) -> Response {
    infer(state, request, ApiProtocol::Messages).await
}
async fn chat(State(state): State<Arc<Serving>>, request: Request) -> Response {
    infer(state, request, ApiProtocol::ChatCompletions).await
}
async fn responses(State(state): State<Arc<Serving>>, request: Request) -> Response {
    infer(state, request, ApiProtocol::Responses).await
}
async fn decisions(State(state): State<Arc<Serving>>, request: Request) -> Response {
    infer(state, request, ApiProtocol::Decisions).await
}

async fn infer(state: Arc<Serving>, mut request: Request, inbound: ApiProtocol) -> Response {
    // No body-consuming extractor runs before this nonqueueing acquisition.
    let (snapshot, reservation, source) = match state.acquire_request() {
        Ok(acquired) => acquired,
        Err(error) => return failure(error),
    };
    // Shared serving owns ordinary inference only. Keep profile inspection
    // inside the reserved path, before any durable admission or duplicate lookup.
    if inbound == ApiProtocol::Responses && request.headers().contains_key("bitrouter-beta") {
        return model_http::managed_profile::unavailable();
    }
    let (key_hash, correlation, idempotency) = match request_identity(request.headers()) {
        Ok(identity) => identity,
        Err(error) => return error.into_response(),
    };
    let identity = match Identity::new(&correlation, reservation.execution_id()) {
        Ok(identity) => identity,
        Err(error) => return error.into_response(),
    };
    request
        .headers_mut()
        .insert(CORRELATION_HEADER, identity.correlation.clone());
    let headers = request.headers().clone();
    let Json(body) = match Json::<serde_json::Value>::from_request(request, &()).await {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    if inbound == ApiProtocol::Responses && model_http::managed_profile::is_managed(&body) {
        return model_http::managed_profile::unavailable();
    }
    let fingerprint = match fingerprint::request(&inbound, &headers, &body) {
        Ok(fingerprint) => fingerprint,
        Err(error) => return error.into_response(),
    };
    let input = match model_http::parse_input(&inbound, body) {
        Ok(input) => input,
        Err(error) => return error.into_response(),
    };
    let admitted = reservation
        .admit(&Admission {
            key_hash: &key_hash,
            correlation_id: &correlation,
            idempotency_key: idempotency.as_deref(),
            fingerprint: &fingerprint,
            operation: input.operation(),
            revision: snapshot.revision,
        })
        .await;
    match admitted {
        Ok(Submission::Existing(execution)) => {
            match Identity::new(&correlation, &execution.execution_id) {
                Ok(identity) => {
                    identity.attach((StatusCode::ACCEPTED, Json(execution)).into_response())
                }
                Err(error) => error.into_response(),
            }
        }
        Ok(Submission::Accepted(admission)) => {
            let response = model_http::handle_shared(
                snapshot.state.clone(),
                headers,
                inbound,
                input,
                correlation,
                admission.clone(),
            )
            .await;
            let response = identity.attach(response);
            response.map(|inner| {
                Body::new(CustodyBody {
                    inner,
                    _admission: admission,
                    _snapshot: snapshot,
                    _source: source,
                })
            })
        }
        Err(error) => {
            // A lost acknowledgement may hide a committed admission. Its
            // candidate identity permits a status query even if recovery later
            // proves that no execution committed.
            let uncertain = error.downcast_ref::<Rejection>().is_none();
            let response = failure(error);
            if uncertain {
                identity.attach(response)
            } else {
                response
            }
        }
    }
}

async fn execution(
    State(state): State<Arc<Serving>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let key_hash = match credential_hash(&headers) {
        Ok(hash) => hash,
        Err(error) => return error.into_response(),
    };
    if !valid_identity(&id) {
        return BitrouterError::NotFound("shared execution not found".into()).into_response();
    }
    match state.engine.query(&key_hash, &id).await {
        Ok(execution) => match Identity::new(&execution.correlation_id, &execution.execution_id) {
            Ok(identity) => identity.attach(Json(execution).into_response()),
            Err(error) => error.into_response(),
        },
        Err(error) => failure(error),
    }
}

async fn models(State(state): State<Arc<Serving>>, headers: HeaderMap) -> Response {
    if !state.ready.load(Ordering::Acquire) {
        return unavailable();
    }
    let (snapshot, source) = match state.acquire_reader() {
        Ok(acquired) => acquired,
        Err(error) => return failure(error),
    };
    let response = model_http::list_models(State(snapshot.state.clone()), headers)
        .await
        .into_response();
    drop(snapshot);
    drop(source);
    response
}

async fn metrics(State(state): State<Arc<Serving>>) -> Response {
    let (snapshot, source) = match state.acquire_reader() {
        Ok(acquired) => acquired,
        Err(error) => return failure(error),
    };
    let response = model_http::prometheus_metrics(State(snapshot.state.clone())).await;
    drop(snapshot);
    drop(source);
    response
}

async fn health(State(state): State<Arc<Serving>>) -> Response {
    if state.ready.load(Ordering::Acquire) {
        Json(serde_json::json!({"status":"ok"})).into_response()
    } else {
        unavailable()
    }
}

fn failure(error: anyhow::Error) -> Response {
    if matches!(
        error.downcast_ref::<Rejection>(),
        Some(Rejection::IdempotencyConflict)
    ) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error":{
                "type":"invalid_request_error", "code":"idempotency_conflict",
                "message":"idempotency key is already bound to a different request"
            }})),
        )
            .into_response();
    }
    let mut response = public_error(error).into_response();
    if response.status() == StatusCode::SERVICE_UNAVAILABLE {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    response
}

fn unavailable() -> Response {
    failure(Rejection::Unavailable.into())
}

/// Request custody remains while response frames or producer work can exist.
/// Connection ownership separately covers Hyper's later output tail.
struct CustodyBody {
    inner: Body,
    _admission: Arc<SharedAdmission>,
    _snapshot: Arc<Snapshot>,
    // Fields drop in declaration order. Stream Drop may register compensation;
    // release the external producer only after those destructors have run.
    _source: Source,
}

impl HttpBody for CustodyBody {
    type Data = Bytes;
    type Error = axum::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}
