//! Opt-in privileged HTTP adapter over the same BRO Thread service used by the
//! owner-restricted local socket. It never executes a model or tool itself.

use crate::harness::agent::AgentConfigExt;
use std::future::IntoFuture;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use crate::config::AgentApiConfig;
use crate::gateway::caller::CallerContext;
use crate::harness::service::ServiceError;
use crate::harness::service::ThreadService;
use crate::harness::thread::ThreadHistoryRequest;
use crate::harness::thread::ThreadRequest;
use crate::harness::thread::ThreadTarget;
use crate::harness::turn::{ApprovalAnswer, CancelTurnRequest, SteeringRequest, TurnRequest};
use anyhow::{Result, ensure};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::Event;
use axum::response::{IntoResponse, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use bitrouter_ai::harness::agent::AgentConfig;
use bitrouter_ai::harness::api::{
    self, CancelMode, CancelRequest, CapabilitiesResponse, CreateThreadRequest, ErrorResponse,
    HistoryQuery, HistoryResponse, InputRequest, ObservationError, ObserveQuery, PROTOCOL_VERSION,
    SteeringReceiptResponse, ThreadResponse, ThreadViewResponse, TurnMode, TurnReceiptResponse,
    TurnResponse,
};
use bitrouter_ai::harness::runtime::ErrorCode;
use bitrouter_ai::harness::thread::PermissionProfile;
use subtle::ConstantTimeEq;

#[derive(Clone)]
struct ApiState {
    service: ThreadService,
    workspaces: Vec<PathBuf>,
    token: String,
    admission: std::sync::Arc<tokio::sync::Semaphore>,
}

impl ApiState {
    fn native_execution(&self, headers: &HeaderMap) -> Result<(), ApiError> {
        authenticated(headers, &self.token)?;
        instance(headers, &self.service)?;
        ThreadService::require_native_execution().map_err(runtime_error)
    }

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

type ApiError = (StatusCode, Json<ErrorResponse>);

fn error(status: StatusCode, message: impl Into<String>) -> ApiError {
    (
        status,
        Json(ErrorResponse {
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
        ErrorCode::UnsupportedCapability => StatusCode::NOT_IMPLEMENTED,
    };
    (
        status,
        Json(ErrorResponse {
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
) -> Result<Json<CapabilitiesResponse>, ApiError> {
    authenticated(&headers, &state.token)?;
    let runtime = state.service.capabilities();
    let operations: &[&str] = if runtime.native_execution {
        &[
            "create_thread",
            "start_turn",
            "enqueue_turn",
            "steer",
            "cancel_turn",
            "input",
            "resume_queue",
            "read_thread",
            "read_turn",
            "history",
            "observe",
        ]
    } else {
        &["read_thread", "read_turn", "history", "observe"]
    };
    Ok(Json(CapabilitiesResponse {
        version: PROTOCOL_VERSION,
        runtime,
        operations: operations
            .iter()
            .map(|operation| (*operation).to_owned())
            .collect(),
    }))
}
async fn create(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<CreateThreadRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.native_execution(&headers)?;
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
                    AgentConfig::fixed(body.model, body.effort)
                        .with_model_mode(body.model_mode)
                        .with_output_reservation(body.max_output_tokens)
                        .with_skill_constraints(body.skills)
                        .read_only()
                } else {
                    AgentConfig::fixed(body.model, body.effort)
                        .with_model_mode(body.model_mode)
                        .with_output_reservation(body.max_output_tokens)
                        .with_skill_constraints(body.skills)
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
        Json(ThreadResponse {
            version: PROTOCOL_VERSION,
            thread: snapshot,
        }),
    ))
}
async fn read(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<ThreadViewResponse>, ApiError> {
    let target = state.thread(&headers, id, false).await?;
    let view = state
        .service
        .read_stored_thread_view(&target, &api_caller())
        .await
        .map_err(runtime_error)?;
    Ok(Json(ThreadViewResponse {
        version: PROTOCOL_VERSION,
        view,
    }))
}
async fn turn(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<api::TurnRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.native_execution(&headers)?;
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
        Json(TurnReceiptResponse {
            version: PROTOCOL_VERSION,
            receipt,
        }),
    ))
}
async fn read_turn(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((id, turn_id)): Path<(String, String)>,
) -> Result<Json<TurnResponse>, ApiError> {
    let target = state.thread(&headers, id, false).await?;
    let snapshot = state
        .service
        .read_stored_turn(&target, &api_caller(), &turn_id)
        .await
        .map_err(runtime_error)?;
    Ok(Json(TurnResponse {
        version: PROTOCOL_VERSION,
        turn: snapshot,
    }))
}
async fn events(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<HistoryResponse>, ApiError> {
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
    Ok(Json(HistoryResponse {
        version: PROTOCOL_VERSION,
        history: page,
    }))
}
async fn input(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<InputRequest>,
) -> Result<Json<TurnReceiptResponse>, ApiError> {
    state.native_execution(&headers)?;
    let target = state.thread(&headers, id, true).await?;
    let view = state
        .service
        .read_thread_view(&target, &api_caller())
        .map_err(runtime_error)?;
    if view
        .latest_turn
        .as_ref()
        .filter(|turn| turn.turn_id == body.turn_id)
        .and_then(|turn| turn.pending_input.as_ref())
        .is_some_and(|input| {
            input.request_id == body.request_id && input.tool_name == "mcp_skill.activate"
        })
        && !body.skill_consent
    {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "MCP skill approval requires explicit skill_consent=true; command policy grants no skill consent",
        ));
    }
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
    Ok(Json(TurnReceiptResponse {
        version: PROTOCOL_VERSION,
        receipt,
    }))
}
async fn cancel(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((id, turn_id)): Path<(String, String)>,
    Json(body): Json<CancelRequest>,
) -> Result<Json<TurnReceiptResponse>, ApiError> {
    state.native_execution(&headers)?;
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
    Ok(Json(TurnReceiptResponse {
        version: PROTOCOL_VERSION,
        receipt,
    }))
}
async fn steer(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<api::SteeringRequest>,
) -> Result<Json<SteeringReceiptResponse>, ApiError> {
    state.native_execution(&headers)?;
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
    Ok(Json(SteeringReceiptResponse {
        version: PROTOCOL_VERSION,
        receipt,
    }))
}
async fn resume(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<ThreadResponse>, ApiError> {
    state.native_execution(&headers)?;
    let target = state.thread(&headers, id, true).await?;
    let snapshot = state
        .service
        .resume_queue(&target, &api_caller(), acceptance_key(&headers)?)
        .await
        .map_err(runtime_error)?;
    Ok(Json(ThreadResponse {
        version: PROTOCOL_VERSION,
        thread: snapshot,
    }))
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
            Err(error) => Some((
                Event::default().event("error").json_data(ObservationError {
                    code: error.code,
                    message: error.message,
                }),
                None,
            )),
        }
    });
    Ok(Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()))
}

#[cfg(all(test, not(feature = "core")))]
mod core_disabled_tests;
