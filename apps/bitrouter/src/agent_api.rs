//! Opt-in privileged HTTP adapter over the same BRO Thread service used by the
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
use bitrouter_ai::types::ReasoningEffort;
use bitrouter_orchestrator::agent::AgentConfig;
use bitrouter_orchestrator::service::{ErrorCode, ServiceError, ThreadService};
use bitrouter_orchestrator::thread::{
    PermissionProfile, ThreadHistoryRequest, ThreadRequest, ThreadTarget,
};
use bitrouter_orchestrator::turn::{
    ApprovalAnswer, CancelTurnRequest, SteeringRequest, TurnRequest,
};
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::config::AgentApiConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

#[derive(Clone)]
struct ApiState {
    service: ThreadService,
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
                "workspace is not authorized for this Thread API",
            ))
        }
    }

    async fn thread(
        &self,
        headers: &HeaderMap,
        id: String,
        hot: bool,
    ) -> Result<ThreadTarget, ApiError> {
        authenticated(headers, &self.token)?;
        instance(headers, &self.service)?;
        let target = ThreadTarget {
            thread_id: id,
            server_instance_id: self.service.capabilities().server_instance_id,
        };
        let caller = api_caller();
        let view = if hot {
            self.service.load_thread(&target, &caller).await
        } else {
            self.service.read_stored_thread_view(&target, &caller).await
        }
        .map_err(runtime_error)?;
        self.workspace(&view.thread.workspace)?;
        Ok(target)
    }
}

pub struct BoundAgentApi {
    listener: tokio::net::TcpListener,
    state: ApiState,
}

#[derive(Deserialize)]
struct CreateBody {
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
    cutoff: Option<u64>,
    #[serde(default = "page_limit")]
    limit: usize,
}
fn page_limit() -> usize {
    128
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum TurnMode {
    Start,
    Enqueue,
}
#[derive(Deserialize)]
struct TurnBody {
    prompt: String,
    mode: TurnMode,
}
#[derive(Deserialize)]
struct InputBody {
    turn_id: String,
    request_id: String,
    approved: bool,
}
#[derive(Deserialize)]
struct SteeringBody {
    expected_turn_id: String,
    text: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum CancelMode {
    Active,
    Queued,
}
#[derive(Deserialize)]
struct CancelBody {
    mode: CancelMode,
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
        ErrorCode::UnknownTurn | ErrorCode::UnknownThread => StatusCode::NOT_FOUND,
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

fn instance(headers: &HeaderMap, service: &ThreadService) -> Result<(), ApiError> {
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
            message: "Thread request capacity reached".into(),
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
            "Thread execution credential required",
        ))
    }
}

impl BoundAgentApi {
    pub async fn bind(config: &AgentApiConfig, service: ThreadService) -> Result<Option<Self>> {
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
            .route("/agent/v2/capabilities", get(capabilities))
            .route("/agent/v2/threads", post(create))
            .route("/agent/v2/threads/{id}", get(read))
            .route("/agent/v2/threads/{id}/turns", post(turn))
            .route("/agent/v2/threads/{id}/turns/{turn_id}", get(read_turn))
            .route(
                "/agent/v2/threads/{id}/turns/{turn_id}/cancel",
                post(cancel),
            )
            .route("/agent/v2/threads/{id}/steer", post(steer))
            .route("/agent/v2/threads/{id}/inputs", post(input))
            .route("/agent/v2/threads/{id}/resume", post(resume))
            .route("/agent/v2/threads/{id}/history", get(events))
            .route("/agent/v2/threads/{id}/observe", get(observe))
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
                    Err(_) => tracing::warn!("Thread HTTP connection drain deadline reached; daemon exit closes remaining clients"),
                }
            }
        }
        Ok(())
    }
}

fn api_caller() -> CallerContext {
    CallerContext::new("agent-api", "agent-api")
}
fn acceptance_key(headers: &HeaderMap) -> Result<String, ApiError> {
    headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|key| !key.is_empty() && key.len() <= 128 && key.is_ascii())
        .map(str::to_owned)
        .ok_or_else(|| {
            error(
                StatusCode::BAD_REQUEST,
                "valid Idempotency-Key header required",
            )
        })
}
async fn capabilities(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authenticated(&headers, &state.token)?;
    Ok(Json(
        json!({"version": 2, "runtime": state.service.capabilities(), "operations": ["create_thread", "start_turn", "enqueue_turn", "steer", "cancel_turn", "input", "resume_queue", "read_thread", "read_turn", "history", "observe"]}),
    ))
}
async fn create(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<CreateBody>,
) -> Result<impl IntoResponse, ApiError> {
    authenticated(&headers, &state.token)?;
    instance(&headers, &state.service)?;
    let workspace = body
        .workspace
        .canonicalize()
        .map_err(|e| runtime_error(e.to_string().into()))?;
    state.workspace(&workspace)?;
    let snapshot = state
        .service
        .create_thread(
            &state.service.capabilities().server_instance_id,
            ThreadRequest {
                caller: api_caller(),
                workspace,
                config: if body.read_only {
                    AgentConfig::fixed(body.model, body.effort).read_only()
                } else {
                    AgentConfig::fixed(body.model, body.effort)
                },
                permission_profile: if body.read_only {
                    PermissionProfile::ReadOnly
                } else {
                    PermissionProfile::Ask
                },
                verification_command: body.verification_command,
                idempotency_key: acceptance_key(&headers)?,
            },
        )
        .await
        .map_err(runtime_error)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"version": 2, "thread": snapshot})),
    ))
}
async fn read(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let target = state.thread(&headers, id, false).await?;
    let view = state
        .service
        .read_stored_thread_view(&target, &api_caller())
        .await
        .map_err(runtime_error)?;
    Ok(Json(json!({"version":2, "view":view})))
}
async fn turn(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<TurnBody>,
) -> Result<impl IntoResponse, ApiError> {
    let target = state.thread(&headers, id, true).await?;
    let request = TurnRequest {
        prompt: body.prompt,
        idempotency_key: acceptance_key(&headers)?,
    };
    let receipt = match body.mode {
        TurnMode::Start => {
            state
                .service
                .start_turn(&target, &api_caller(), request)
                .await
        }
        TurnMode::Enqueue => {
            state
                .service
                .enqueue_turn(&target, &api_caller(), request)
                .await
        }
    }
    .map_err(runtime_error)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"version":2, "receipt":receipt})),
    ))
}
async fn read_turn(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((id, turn_id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let target = state.thread(&headers, id, false).await?;
    let snapshot = state
        .service
        .read_stored_turn(&target, &api_caller(), &turn_id)
        .await
        .map_err(runtime_error)?;
    Ok(Json(json!({"version":2, "turn":snapshot})))
}
async fn events(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<Value>, ApiError> {
    let target = state.thread(&headers, id, false).await?;
    let page = state
        .service
        .thread_history(
            &target,
            &api_caller(),
            ThreadHistoryRequest {
                after: query.after,
                cutoff: query.cutoff,
                limit: query.limit,
            },
        )
        .await
        .map_err(runtime_error)?;
    Ok(Json(json!({"version":2, "history":page})))
}
async fn input(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<InputBody>,
) -> Result<Json<Value>, ApiError> {
    let target = state.thread(&headers, id, true).await?;
    let receipt = state
        .service
        .answer_thread_input(
            &target,
            &api_caller(),
            ApprovalAnswer {
                turn_id: body.turn_id,
                request_id: body.request_id,
                approved: body.approved,
                idempotency_key: acceptance_key(&headers)?,
            },
        )
        .await
        .map_err(runtime_error)?;
    Ok(Json(json!({"version":2,"receipt":receipt})))
}
async fn cancel(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((id, turn_id)): Path<(String, String)>,
    Json(body): Json<CancelBody>,
) -> Result<Json<Value>, ApiError> {
    let target = state.thread(&headers, id, true).await?;
    let key = acceptance_key(&headers)?;
    let receipt = if matches!(body.mode, CancelMode::Queued) {
        state
            .service
            .cancel_queued_turn(&target, &api_caller(), &turn_id, key)
            .await
    } else {
        state
            .service
            .cancel_turn(
                &target,
                &api_caller(),
                CancelTurnRequest {
                    turn_id,
                    idempotency_key: key,
                },
            )
            .await
    }
    .map_err(runtime_error)?;
    Ok(Json(json!({"version":2,"receipt":receipt})))
}
async fn steer(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<SteeringBody>,
) -> Result<Json<Value>, ApiError> {
    let target = state.thread(&headers, id, true).await?;
    let receipt = state
        .service
        .steer(
            &target,
            &api_caller(),
            SteeringRequest {
                expected_turn_id: body.expected_turn_id,
                text: body.text,
                idempotency_key: acceptance_key(&headers)?,
            },
        )
        .await
        .map_err(runtime_error)?;
    Ok(Json(json!({"version":2,"receipt":receipt})))
}
async fn resume(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let target = state.thread(&headers, id, true).await?;
    let snapshot = state
        .service
        .resume_queue(&target, &api_caller(), acceptance_key(&headers)?)
        .await
        .map_err(runtime_error)?;
    Ok(Json(json!({"version":2,"thread":snapshot})))
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
    let target = state.thread(&headers, id, true).await?;
    let subscription = state
        .service
        .observe_thread(&target, &api_caller(), query.after)
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
