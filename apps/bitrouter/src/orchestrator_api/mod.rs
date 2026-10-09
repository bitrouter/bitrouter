//! Authenticated transport over the same in-process orchestrator core.

mod auth;
mod channel;
mod output;
mod responses;

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use bitrouter_orchestrator::core::protocol::{Capabilities, CoreError, ErrorCode, Limits, VERSION};
use bitrouter_orchestrator::core::session::CoreSession;
use bitrouter_sdk::App;
use sea_orm::DatabaseConnection;
use tokio::sync::{Mutex, Semaphore};

type SessionKey = (String, String, String);
type OutputBudgets = (Weak<output::Budget>, Weak<output::Budget>);

struct Entry {
    session: Option<CoreSession>,
    port: Arc<channel::RemotePort>,
    grant: bitrouter_orchestrator::core::protocol::OwnershipGrant,
    limits: Limits,
    connected: bool,
    ready: bool,
    job: Option<responses::Job>,
    recovery_evidence: Vec<bitrouter_orchestrator::core::protocol::ProviderAttemptEvidence>,
}

struct Shared {
    app: Arc<App>,
    db: DatabaseConnection,
    capabilities: Capabilities,
    sessions: Mutex<BTreeMap<SessionKey, Entry>>,
    connections: Arc<Semaphore>,
    requests: Arc<Semaphore>,
    inspections: Arc<Semaphore>,
    outputs: Mutex<BTreeMap<SessionKey, OutputBudgets>>,
    shutdown: tokio_util::sync::CancellationToken,
}

/// Host-owned registry; durable execution state remains at the harness.
#[derive(Clone)]
pub struct ManagedCoreApi {
    shared: Arc<Shared>,
}

impl ManagedCoreApi {
    pub fn new(app: Arc<App>, db: DatabaseConnection, core_instance_id: String) -> Self {
        let limits = Limits::default();
        // Admission bounds the sum of all session model slots by the host cap.
        let max_sessions = 4;
        let capabilities = Capabilities {
            version: VERSION,
            core_instance_id,
            operations: [
                "session.bind",
                "session.restore",
                "session.head",
                "operation.get",
                "session.release",
                "input.enqueue",
                "input.steer",
                "run.cancel",
                "agent.cancel",
                "queue.resume",
                "signals.update",
                "tool.result",
                "tool.status",
                "model.evidence",
                "material.result",
                "checkpoint.ack",
                "artifact.chunk",
                "responses.create",
                bitrouter_orchestrator::core::context_router::FEATURE,
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            transports: vec!["http_sse".into(), "websocket".into()],
            unsupported_features: [
                "response.inject",
                "encrypted_multi_agent_items",
                "stateless_replay",
                "native_multi_agent_items",
                "running_restore_handoff",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            max_sessions,
            max_host_model_attempts: max_sessions * limits.active_models,
            limits,
        };
        Self {
            shared: Arc::new(Shared {
                app,
                db,
                capabilities,
                sessions: Mutex::new(BTreeMap::new()),
                connections: Arc::new(Semaphore::new(max_sessions as usize)),
                requests: Arc::new(Semaphore::new(16)),
                inspections: Arc::new(Semaphore::new(4)),
                outputs: Mutex::new(BTreeMap::new()),
                shutdown: tokio_util::sync::CancellationToken::new(),
            }),
        }
    }

    /// Wrap the existing inference router, preserving its ordinary handlers.
    pub fn wrap(&self, router: Router) -> Router {
        let routes = Router::new()
            .route("/v1/orchestrator/capabilities", get(capabilities))
            .route("/v1/orchestrator/channel", get(channel::upgrade))
            .with_state(self.clone());
        router
            .layer(axum::middleware::from_fn_with_state(
                self.clone(),
                responses::intercept,
            ))
            .merge(routes)
    }

    /// Fence managed execution and close channels before HTTP graceful drain.
    pub async fn shutdown(&self) {
        self.shared.shutdown.cancel();
        let sessions: Vec<_> = self
            .shared
            .sessions
            .lock()
            .await
            .values()
            .filter_map(|entry| entry.session.clone())
            .collect();
        for session in sessions {
            session.disconnect().await;
        }
    }

    fn key(principal: &auth::Principal, session_id: &str) -> SessionKey {
        let (user, key) = principal.scope();
        (user, key, session_id.into())
    }

    async fn response_scope(
        &self,
        principal: &auth::Principal,
        session_id: &str,
        epoch: u64,
    ) -> Result<(Limits, Arc<output::Budget>), ApiError> {
        let sessions = self.shared.sessions.lock().await;
        let entry = sessions
            .get(&Self::key(principal, session_id))
            .filter(|entry| entry.connected && entry.ready)
            .ok_or_else(|| {
                ApiError::core(
                    ErrorCode::UnauthorizedScope,
                    "session is not bound to this principal",
                )
            })?;
        if entry.grant.execution_epoch != epoch {
            return Err(ApiError::core(
                ErrorCode::StaleEpoch,
                "request does not name the bound epoch",
            ));
        }
        entry
            .session
            .as_ref()
            .map(|_| (entry.limits.clone(), entry.port.output.budget()))
            .ok_or_else(|| {
                ApiError::core(
                    ErrorCode::Busy,
                    "session binding is awaiting its checkpoint ACK",
                )
            })
    }
}

async fn capabilities(
    State(api): State<ManagedCoreApi>,
    headers: HeaderMap,
) -> Result<Json<Capabilities>, ApiError> {
    auth::Principal::authenticate(&api.shared.db, &headers).await?;
    Ok(Json(api.shared.capabilities.clone()))
}

pub(super) struct ApiError(CoreError);

impl ApiError {
    fn core(code: ErrorCode, message: &str) -> Self {
        Self(CoreError::rejected(code, message))
    }
    fn unauthorized() -> Self {
        Self::core(
            ErrorCode::UnauthorizedScope,
            "managed API requires an active virtual key",
        )
    }
    fn unavailable() -> Self {
        Self::core(
            ErrorCode::CheckpointUnavailable,
            "managed authority is unavailable",
        )
    }
}

impl From<CoreError> for ApiError {
    fn from(error: CoreError) -> Self {
        Self(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.0.code {
            ErrorCode::UnauthorizedScope => StatusCode::UNAUTHORIZED,
            ErrorCode::Busy
            | ErrorCode::StaleEpoch
            | ErrorCode::StaleRevision
            | ErrorCode::OperationConflict
            | ErrorCode::CheckpointConflict => StatusCode::CONFLICT,
            ErrorCode::LimitExceeded => StatusCode::PAYLOAD_TOO_LARGE,
            ErrorCode::CheckpointUnavailable | ErrorCode::ArtifactUnavailable => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            _ => StatusCode::BAD_REQUEST,
        };
        (status, Json(serde_json::json!({"error":self.0}))).into_response()
    }
}
