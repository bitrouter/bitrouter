//! Opt-in privileged HTTP adapter over the same BRO task service used by the
//! owner-restricted local socket. It never executes a model or tool itself.

use std::future::IntoFuture;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use anyhow::{Result, ensure};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::Event;
use axum::response::{IntoResponse, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use bitrouter_orchestrator::agent::AgentConfig;
use bitrouter_orchestrator::service::{ErrorCode, ServiceError, TaskRequest, TaskService};
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::config::AgentApiConfig;
use bitrouter_sdk::language_model::types::ReasoningEffort;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

#[derive(Clone)]
struct ApiState {
    service: TaskService,
    workspaces: Vec<PathBuf>,
    token: String,
    admission: std::sync::Arc<tokio::sync::Semaphore>,
}

impl ApiState {
    fn workspace(&self, workspace: &std::path::Path) -> Result<(), ApiError> {
        if self.workspaces.iter().any(|allowed| allowed == workspace) {
            Ok(())
        } else {
            Err(error(
                StatusCode::FORBIDDEN,
                "workspace is not authorized for this task API",
            ))
        }
    }

    fn task(&self, id: &str) -> Result<bitrouter_orchestrator::service::TaskSnapshot, ApiError> {
        let snapshot = self.service.read(id).map_err(runtime_error)?;
        self.workspace(&snapshot.workspace)?;
        Ok(snapshot)
    }
}

pub struct BoundAgentApi {
    listener: tokio::net::TcpListener,
    state: ApiState,
}

#[derive(Deserialize)]
struct SubmitBody {
    prompt: String,
    workspace: PathBuf,
    model: String,
    effort: Option<ReasoningEffort>,
    #[serde(default)]
    read_only: bool,
    verification_command: Option<String>,
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    after: u64,
}

#[derive(Deserialize)]
struct InputBody {
    request_id: String,
    approved: bool,
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    code: ErrorCode,
}

type ApiError = (StatusCode, Json<ErrorBody>);

fn error(status: StatusCode, message: impl Into<String>) -> ApiError {
    (
        status,
        Json(ErrorBody {
            error: message.into(),
            code: ErrorCode::InvalidRequest,
        }),
    )
}

fn runtime_error(failure: ServiceError) -> ApiError {
    let status = match failure.code {
        ErrorCode::UnknownTask | ErrorCode::UnknownThread => StatusCode::NOT_FOUND,
        ErrorCode::Unauthorized => StatusCode::FORBIDDEN,
        ErrorCode::Conflict
        | ErrorCode::InstanceChanged
        | ErrorCode::ResyncRequired
        | ErrorCode::RecoveryRequired => StatusCode::CONFLICT,
        ErrorCode::Overloaded | ErrorCode::ShuttingDown | ErrorCode::StorageUnavailable => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        ErrorCode::InvalidRequest => StatusCode::BAD_REQUEST,
    };
    (
        status,
        Json(ErrorBody {
            error: failure.message,
            code: failure.code,
        }),
    )
}

fn instance(headers: &HeaderMap, service: &TaskService) -> Result<(), ApiError> {
    service
        .ensure_instance(
            headers
                .get("x-bro-server-instance")
                .and_then(|header| header.to_str().ok()),
        )
        .map_err(runtime_error)
}

async fn admit(
    State(state): State<ApiState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let Ok(_permit) = state.admission.try_acquire() else {
        return runtime_error(ServiceError {
            code: ErrorCode::Overloaded,
            message: "task request capacity reached".into(),
        })
        .into_response();
    };
    next.run(request).await
}

fn authenticated(headers: &HeaderMap, token: &str) -> Result<(), ApiError> {
    let candidate = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("");
    if candidate.len() == token.len() && bool::from(candidate.as_bytes().ct_eq(token.as_bytes())) {
        Ok(())
    } else {
        Err(error(
            StatusCode::UNAUTHORIZED,
            "task execution credential required",
        ))
    }
}

impl BoundAgentApi {
    pub async fn bind(config: &AgentApiConfig, service: TaskService) -> Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }
        let address: SocketAddr = config.listen.parse()?;
        ensure!(
            matches!(address.ip(), IpAddr::V4(ip) if ip.is_loopback())
                || matches!(address.ip(), IpAddr::V6(ip) if ip.is_loopback()),
            "agent_api.listen must be a loopback address"
        );
        ensure!(
            !config.token_env.is_empty(),
            "agent_api.token_env is required"
        );
        ensure!(
            config.token_env != "BITROUTER_CONTROL_TOKEN",
            "agent API needs a distinct credential"
        );
        ensure!(
            !config.workspaces.is_empty(),
            "agent_api.workspaces must list server-owned paths"
        );
        let token = std::env::var(&config.token_env)?;
        ensure!(
            token.len() >= 24,
            "agent API credential must have at least 24 bytes"
        );
        let listener = tokio::net::TcpListener::bind(address).await?;
        Ok(Some(Self {
            listener,
            state: ApiState {
                workspaces: config
                    .workspaces
                    .iter()
                    .map(|workspace| workspace.canonicalize())
                    .collect::<std::io::Result<Vec<_>>>()?,
                service,
                token,
                admission: std::sync::Arc::new(tokio::sync::Semaphore::new(64)),
            },
        }))
    }

    pub async fn serve(self, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<()> {
        let router = Router::new()
            .route("/agent/v1/capabilities", get(capabilities))
            .route("/agent/v1/tasks", post(submit))
            .route("/agent/v1/tasks/{id}", get(read))
            .route("/agent/v1/tasks/{id}/events", get(events))
            .route("/agent/v1/tasks/{id}/observe", get(observe))
            .route("/agent/v1/tasks/{id}/inputs", post(input))
            .route("/agent/v1/tasks/{id}/cancel", post(cancel))
            .layer(DefaultBodyLimit::max(64 * 1024))
            .layer(axum::middleware::from_fn_with_state(
                self.state.clone(),
                admit,
            ))
            .with_state(self.state);
        let stopped = tokio_util::sync::CancellationToken::new();
        let signal = stopped.clone();
        let server = axum::serve(self.listener, router)
            .with_graceful_shutdown(async move {
                shutdown.await;
                signal.cancel();
            })
            .into_future();
        tokio::pin!(server);
        tokio::select! {
            result = &mut server => { result?; }
            _ = stopped.cancelled() => {
                match tokio::time::timeout(std::time::Duration::from_secs(5), &mut server).await {
                    Ok(result) => result?,
                    Err(_) => tracing::warn!("task HTTP connection drain deadline reached; daemon exit closes remaining clients"),
                }
            }
        }
        Ok(())
    }
}

async fn capabilities(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authenticated(&headers, &state.token)?;
    Ok(Json(
        json!({"version": 1, "runtime": state.service.capabilities(), "operations": ["submit", "read", "events", "observe", "input", "cancel"]}),
    ))
}

async fn submit(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<SubmitBody>,
) -> Result<impl IntoResponse, ApiError> {
    authenticated(&headers, &state.token)?;
    instance(&headers, &state.service)?;
    let key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 128 && value.is_ascii())
        .ok_or_else(|| {
            error(
                StatusCode::BAD_REQUEST,
                "valid Idempotency-Key header required",
            )
        })?;
    let workspace = body
        .workspace
        .canonicalize()
        .map_err(|error| runtime_error(error.to_string().into()))?;
    state.workspace(&workspace)?;
    let snapshot = state
        .service
        .submit(TaskRequest {
            prompt: body.prompt,
            workspace,
            caller: CallerContext::new("agent-api", "agent-api"),
            config: if body.read_only {
                AgentConfig::fixed(body.model, body.effort).read_only()
            } else {
                AgentConfig::fixed(body.model, body.effort)
            },
            verification_command: body.verification_command,
            idempotency_key: Some(key.to_string()),
        })
        .await
        .map_err(runtime_error)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"version": 1, "task": snapshot})),
    ))
}

async fn read(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    authenticated(&headers, &state.token)?;
    instance(&headers, &state.service)?;
    let snapshot = state.task(&id)?;
    Ok(Json(json!({"version": 1, "task": snapshot})))
}

async fn events(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<Value>, ApiError> {
    authenticated(&headers, &state.token)?;
    instance(&headers, &state.service)?;
    state.task(&id)?;
    let events = state
        .service
        .events_after(&id, query.after)
        .map_err(runtime_error)?;
    Ok(Json(json!({"version": 1, "events": events})))
}

async fn input(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<InputBody>,
) -> Result<StatusCode, ApiError> {
    authenticated(&headers, &state.token)?;
    instance(&headers, &state.service)?;
    state.task(&id)?;
    state
        .service
        .answer_input(&id, &body.request_id, body.approved)
        .await
        .map_err(runtime_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn cancel(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    authenticated(&headers, &state.token)?;
    instance(&headers, &state.service)?;
    state.task(&id)?;
    state.service.cancel(&id).await.map_err(runtime_error)?;
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
struct ObserveQuery {
    after: Option<u64>,
}

async fn observe(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<ObserveQuery>,
) -> Result<impl IntoResponse, ApiError> {
    authenticated(&headers, &state.token)?;
    instance(&headers, &state.service)?;
    state.task(&id)?;
    let subscription = state
        .service
        .observe(&id, query.after)
        .map_err(runtime_error)?;
    let stream = futures::stream::unfold(Some(subscription), |subscription| async move {
        let mut subscription = subscription?;
        match subscription.next().await {
            Ok(Some(observation)) => {
                Some((Event::default().json_data(observation), Some(subscription)))
            }
            Ok(None) => None,
            Err(error) => Some((Event::default().event("error").json_data(error), None)),
        }
    });
    Ok(Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()))
}
