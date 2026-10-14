//! Authenticated HTTP administration control plane for remote BitRouter clients.
//!
//! This is intentionally not the inference API, the local daemon protocol, or
//! ACP over a network transport. It exposes typed inspection and guarded reload on
//! a dedicated loopback listener so an operator can put a private tunnel or a
//! TLS reverse proxy in front of it without making local IPC remotely callable.

pub mod auth;
pub mod inventory;
pub mod operations;
mod reads;

use crate::output::reports::requests::RequestsReportRuntime;
#[cfg(test)]
use crate::reload::ReloadReportRuntime;
use bitrouter_ai::control::{
    ActionDescriptor, CapabilitiesReport, PROTOCOL_VERSION, ResourceDescriptor, StateReport,
};
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::ControlConfig;
#[cfg(test)]
use crate::gateway::error::BitrouterError;
use anyhow::{Context, Result};
use axum::extract::{
    DefaultBodyLimit, MatchedPath, Path, Query, Request, State,
    rejection::{JsonRejection, QueryRejection},
};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
#[cfg(test)]
use bitrouter_ai::control::ControlScope;
use bitrouter_ai::control::action::Action;
use bitrouter_ai::control::error::{ErrorBody, ErrorEnvelope};
use bitrouter_ai::control::models::ModelsReport;
use bitrouter_ai::control::status::StatusReport;
use serde::Deserialize;
use url::Url;
use {bitrouter_ai::control::route::RouteInput, bitrouter_ai::control::route::RouteReport};

use self::auth::{ControlAuth, ControlCaller};
use self::operations::{OperationError, OperationService};
use crate::actions::requests::RequestsAction;
use crate::paths::ConfigSource;
use bitrouter_ai::control::reload::{OperationReport, ReloadInput};
use bitrouter_ai::control::requests::RequestsReport;
use {
    crate::actions::administration::Administration,
    bitrouter_ai::control::administration::PolicyInput,
};

/// Environment variable containing the remote-control bearer token.
pub const CONTROL_TOKEN_ENV: &str = "BITROUTER_CONTROL_TOKEN";

fn current_capabilities(state: &ControlState, caller: &ControlCaller) -> CapabilitiesReport {
    let rows = inventory::ACTIONS
        .iter()
        .filter(|row| {
            caller.permits(row.scope)
                && (!row.requires_reload || state.operations.is_some())
                && (!row.requires_administration || state.administration.is_some())
        })
        .collect::<Vec<_>>();
    CapabilitiesReport {
        resources: inventory::RESOURCES
            .iter()
            .filter(|row| {
                caller.permits(row.scope) && (!row.requires_reload || state.operations.is_some())
            })
            .map(|row| {
                (
                    row.id.into(),
                    ResourceDescriptor {
                        version: 1,
                        path: row.path.into(),
                        required_scope: row.scope,
                    },
                )
            })
            .collect(),
        limits: std::collections::BTreeMap::from([
            ("max_operations".into(), operations::MAX_OPERATIONS as u64),
            (
                "preparation_timeout_seconds".into(),
                crate::reload::PREPARATION_TIMEOUT_SECONDS,
            ),
            (
                "operation_retention_seconds".into(),
                operations::RETENTION_SECONDS,
            ),
            ("max_reload_body_bytes".into(), 16 * 1024),
            (
                "max_request_rows".into(),
                bitrouter_ai::control::requests::MAX_REQUEST_ROWS,
            ),
            (
                "max_request_window_days".into(),
                crate::actions::requests::MAX_REQUEST_WINDOW_DAYS as u64,
            ),
            ("max_response_bytes".into(), 8 * 1024 * 1024),
        ]),
        protocol: "bitrouter-control".into(),
        protocol_version: PROTOCOL_VERSION,
        server_version: crate::VERSION.into(),
        actions: rows.iter().map(|row| row.legacy_name.into()).collect(),
        action_descriptors: rows
            .iter()
            .map(|row| {
                (
                    row.id.into(),
                    ActionDescriptor {
                        version: row.version,
                        required_scope: row.scope,
                        input_schema: (row.input_schema)(),
                        output_schema: (row.output_schema)(),
                    },
                )
            })
            .collect(),
        server_instance_id: Some(state.instance.clone()),
        scopes: caller.scopes().to_vec(),
    }
}

#[derive(Clone)]
struct ControlState {
    source: ConfigSource,
    socket: PathBuf,
    administration: Option<Administration>,
    instance: String,
    operations: Option<Arc<OperationService>>,
}

impl ControlState {
    fn reads(&self) -> reads::ReadPorts {
        reads::ReadPorts {
            source: self.source.clone(),
            socket: self.socket.clone(),
        }
    }

    fn administration(&self) -> Result<&Administration, ControlError> {
        self.administration.as_ref().ok_or_else(|| {
            ControlError::new(
                StatusCode::NOT_FOUND,
                "unsupported_action",
                "live administration ports are unavailable",
            )
        })
    }
}

/// A validated control listener that is ready to run.
pub struct ControlServer {
    listen: SocketAddr,
    state: ControlState,
    auth: ControlAuth,
}

/// A control listener whose address conflict has already been checked.
pub struct BoundControlServer {
    listen: SocketAddr,
    listener: tokio::net::TcpListener,
    router: Router,
}

impl ControlServer {
    /// Build the optional listener from config and process environment.
    ///
    /// Disabled config returns `None` without consulting the token. Enabled
    /// config fails before either HTTP server starts if the address is not a
    /// numeric loopback socket or the token is absent/too short.
    pub fn from_config(
        config: &ControlConfig,
        source: ConfigSource,
        socket: PathBuf,
    ) -> Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }

        let listen: SocketAddr = config.listen.parse().with_context(|| {
            format!(
                "control.listen '{}' is not a valid numeric host:port",
                config.listen
            )
        })?;
        if !listen.ip().is_loopback() {
            anyhow::bail!(
                "refusing control.listen {listen}: the remote-control MVP binds only to a \
                 loopback address; expose it through a private tunnel or TLS reverse proxy"
            );
        }

        let auth = ControlAuth::from_config(config)?;
        Ok(Some(Self {
            listen,
            state: ControlState {
                source,
                socket,
                administration: None,
                operations: None,
                instance: uuid::Uuid::new_v4().to_string(),
            },
            auth,
        }))
    }

    pub fn with_administration(mut self, administration: Administration) -> Self {
        self.state.administration = Some(administration);
        self
    }

    pub fn with_reloader(mut self, reloader: Arc<dyn crate::daemon::DaemonReloader>) -> Self {
        if let Some(state) = reloader.reload_state() {
            self.state.instance = state.server_instance_id;
            self.state.operations = Some(Arc::new(OperationService::new(reloader)));
        }
        self
    }

    /// Address the control listener will bind.
    pub fn listen(&self) -> SocketAddr {
        self.listen
    }

    /// Bind before the daemon publishes its pid/readiness.
    pub async fn bind(self) -> Result<BoundControlServer> {
        let listener = tokio::net::TcpListener::bind(self.listen)
            .await
            .with_context(|| format!("bind remote control listener {}", self.listen))?;
        Ok(BoundControlServer {
            listen: self.listen,
            listener,
            router: control_router_with(self.state, self.auth),
        })
    }
}

impl BoundControlServer {
    pub fn listen(&self) -> SocketAddr {
        self.listen
    }

    /// Serve until `shutdown` resolves.
    pub async fn serve_with_shutdown<S>(self, shutdown: S) -> Result<()>
    where
        S: Future<Output = ()> + Send + 'static,
    {
        tracing::info!(listen = %self.listen, "remote control listening");
        axum::serve(self.listener, self.router)
            .with_graceful_shutdown(shutdown)
            .await
            .context("serve remote control listener")
    }
}

#[cfg(test)]
fn control_router(source: ConfigSource, socket: PathBuf, token: Vec<u8>) -> Result<Router> {
    let token = String::from_utf8(token).map_err(|_| anyhow::anyhow!("invalid test token"))?;
    let auth = ControlAuth::from_lookup(&ControlConfig::default(), |_| Some(token.clone()))?;
    Ok(control_router_with(
        ControlState {
            source,
            socket,
            administration: None,
            operations: None,
            instance: uuid::Uuid::new_v4().to_string(),
        },
        auth,
    ))
}

fn control_router_with(state: ControlState, expected: ControlAuth) -> Router {
    let mut router = Router::new()
        .route("/control/v1/capabilities", get(capabilities))
        .route("/control/v1/state", get(control_state))
        .route("/control/v1/operations/{request_id}", get(operation));
    let instance = state.instance.clone();
    let operations = state.operations.clone();
    for row in inventory::ACTIONS {
        let handler = match row.action {
            Action::Status => get(status),
            Action::Models => get(models),
            Action::Route => post(route_preview),
            Action::Requests => get(requests),
            Action::Providers => get(providers),
            Action::Observe => get(observe),
            Action::PolicyStatus => get(policy_status),
            Action::PolicyShow => get(policy_show),
            Action::Agents => get(agents),
            Action::Reload => post(reload).layer(DefaultBodyLimit::max(16 * 1024)),
        };
        router = router.route(row.path, handler);
    }
    router
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .layer(axum::middleware::from_fn_with_state(
            (expected, instance, operations),
            require_control_token,
        ))
        .with_state(state)
}

async fn require_control_token(
    State((expected, instance, operations)): State<(
        ControlAuth,
        String,
        Option<Arc<OperationService>>,
    )>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Response {
    let initial_reload = operations.as_ref().and_then(|service| service.state().ok());
    let started = std::time::Instant::now();
    let route_path = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str())
        .unwrap_or("unmatched")
        .to_owned();
    let bounded_report = request.uri().path().starts_with("/control/v1/");
    let caller = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .and_then(|(_, token)| expected.authenticate(token.as_bytes()));
    let credential_id = caller.as_ref().map(|caller| caller.id().to_owned());
    let mut response = match caller {
        None => ControlError::unauthorized().into_response(),
        Some(_) if !origin_matches_host(&headers) => ControlError::new(
            StatusCode::FORBIDDEN,
            "origin_mismatch",
            "request Origin must match the control endpoint Host",
        )
        .into_response(),
        Some(caller) => {
            let path = request
                .extensions()
                .get::<MatchedPath>()
                .map(|path| path.as_str())
                .unwrap_or(request.uri().path());
            let scope = inventory::ACTIONS
                .iter()
                .find(|row| row.path == path)
                .map(|row| row.scope)
                .or_else(|| {
                    inventory::RESOURCES
                        .iter()
                        .find(|row| row.path == path)
                        .map(|row| row.scope)
                });
            if scope.is_some_and(|scope| !caller.permits(scope)) {
                ControlError::new(
                    StatusCode::FORBIDDEN,
                    "scope_denied",
                    "required control scope is not granted",
                )
                .into_response()
            } else {
                request.extensions_mut().insert(caller);
                next.run(request).await
            }
        }
    };
    if bounded_report {
        let (parts, body) = response.into_parts();
        response = match axum::body::to_bytes(body, 8 * 1024 * 1024).await {
            Ok(bytes) => Response::from_parts(parts, axum::body::Body::from(bytes)),
            Err(_) => ControlError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "report_too_large",
                "control report exceeds the response limit",
            )
            .into_response(),
        };
    }
    tracing::info!(action = %route_path, credential_id = credential_id.as_deref().unwrap_or("unauthenticated"), status = response.status().as_u16(), duration_ms = started.elapsed().as_millis() as u64, "control request completed");
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    if credential_id.is_some()
        && let Some(state) = operations.as_ref().and_then(|service| service.state().ok())
    {
        let spans_versions = initial_reload
            .as_ref()
            .is_some_and(|initial| initial.running || initial.generation != state.generation)
            || state.running;
        response.headers_mut().insert(
            "x-bitrouter-control-read-consistency",
            header::HeaderValue::from_static(if spans_versions {
                "may_span_versions"
            } else if state.consistency == bitrouter_ai::control::reload::ReloadConsistency::Mixed {
                "mixed"
            } else {
                "consistent"
            }),
        );
        if let Ok(value) = header::HeaderValue::from_str(&state.generation.to_string()) {
            response
                .headers_mut()
                .insert("x-bitrouter-control-generation", value);
        }
    }
    if credential_id.is_some()
        && let Ok(value) = header::HeaderValue::from_str(&instance)
    {
        response
            .headers_mut()
            .insert("x-bitrouter-control-instance", value);
    }
    response
}

fn origin_matches_host(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Some(host) = headers.get(header::HOST) else {
        return false;
    };
    let Some(origin) = origin
        .to_str()
        .ok()
        .and_then(|value| Url::parse(value).ok())
    else {
        return false;
    };
    let Ok(host) = host.to_str() else {
        return false;
    };
    let Ok(expected) = Url::parse(&format!("{}://{host}/", origin.scheme())) else {
        return false;
    };
    origin.host() == expected.host()
        && origin.port_or_known_default() == expected.port_or_known_default()
}

async fn capabilities(
    State(state): State<ControlState>,
    axum::Extension(caller): axum::Extension<ControlCaller>,
) -> Json<CapabilitiesReport> {
    Json(current_capabilities(&state, &caller))
}

fn operation_service(state: &ControlState) -> Result<&Arc<OperationService>, ControlError> {
    state
        .operations
        .as_ref()
        .ok_or_else(|| OperationError::Unsupported.into())
}

async fn control_state(
    State(state): State<ControlState>,
) -> Result<Json<StateReport>, ControlError> {
    let operations = operation_service(&state)?;
    let config_state = operations.configuration_state().await;
    let reload = operations.state()?;
    let live_policy_digest = if let Some(administration) = &state.administration {
        administration
            .policy(PolicyInput {
                view: bitrouter_ai::control::administration::PolicyView::Active,
                name: None,
            })
            .await
            .map_err(ControlError::internal)?
            .digest
    } else {
        None
    };
    Ok(Json(StateReport {
        reload,
        live_policy_digest,
        config_state,
    }))
}

async fn reload(
    State(state): State<ControlState>,
    axum::Extension(caller): axum::Extension<ControlCaller>,
    payload: Result<Json<ReloadInput>, JsonRejection>,
) -> Result<Response, ControlError> {
    let Json(input) = payload.map_err(|error| {
        ControlError::new(
            error.status(),
            "invalid_request",
            "reload requires a valid bounded JSON request",
        )
    })?;
    let operation = operation_service(&state)?
        .submit(caller.id(), input)
        .await?;
    let status = if operation.is_running() {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(operation)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationLookup {
    instance: String,
}

async fn operation(
    State(state): State<ControlState>,
    axum::Extension(caller): axum::Extension<ControlCaller>,
    Path(request_id): Path<String>,
    query: Result<Query<OperationLookup>, QueryRejection>,
) -> Result<Json<OperationReport>, ControlError> {
    let Query(query) =
        query.map_err(|_| ControlError::bad_request("operation lookup requires instance"))?;
    Ok(Json(
        operation_service(&state)?
            .lookup(caller.id(), &request_id, &query.instance)
            .await?,
    ))
}

impl From<OperationError> for ControlError {
    fn from(error: OperationError) -> Self {
        let status = match error {
            OperationError::InvalidRequest => StatusCode::BAD_REQUEST,
            OperationError::NotFound | OperationError::Unsupported => StatusCode::NOT_FOUND,
            OperationError::Expired => StatusCode::GONE,
            OperationError::Capacity | OperationError::GenerationExhausted => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            _ => StatusCode::CONFLICT,
        };
        let message = match error {
            OperationError::NotFound => {
                "operation outcome is unknown; no retained result exists for this credential"
            }
            OperationError::Expired => {
                "operation outcome is unknown; the retention deadline has passed"
            }
            OperationError::ServerInstanceChanged => {
                "server instance changed; inspect live state before attempting another reload"
            }
            _ => error.code(),
        };
        Self::new(status, error.code(), message)
    }
}

async fn status(State(state): State<ControlState>) -> Result<Json<StatusReport>, ControlError> {
    state
        .reads()
        .status_report()
        .await
        .map(Json)
        .map_err(ControlError::internal)
}

async fn models(
    State(state): State<ControlState>,
    query: Result<Query<inventory::ModelsInput>, QueryRejection>,
) -> Result<Json<ModelsReport>, ControlError> {
    let Query(params) = query.map_err(|_| ControlError::bad_request("invalid model filters"))?;
    if let Some(provider) = &params.provider {
        bitrouter_ai::control::administration::validate_identifier(provider)
            .map_err(ControlError::bad_request)?;
    }
    state
        .reads()
        .models_report()
        .await
        .map(|report| Json(report.filtered(params.provider.as_deref())))
        .map_err(ControlError::internal)
}

async fn route_preview(
    State(state): State<ControlState>,
    payload: Result<Json<RouteInput>, JsonRejection>,
) -> Result<Json<RouteReport>, ControlError> {
    let Json(input) = payload.map_err(|_| ControlError::bad_request("invalid route input"))?;
    state
        .reads()
        .route_report(input)
        .await
        .map(Json)
        .map_err(ControlError::bad_request)
}

async fn requests(
    State(state): State<ControlState>,
    query: Result<Query<bitrouter_ai::control::requests::RequestFilters>, QueryRejection>,
) -> Result<Json<RequestsReport>, ControlError> {
    let Query(params) = query.map_err(|_| ControlError::bad_request("invalid request filters"))?;
    let mut report = RequestsAction::new(state.source, state.socket)
        .report_filtered(params)
        .await
        .map_err(ControlError::bad_request)?;
    report.sanitize_for_remote();
    Ok(Json(report))
}

async fn providers(
    State(state): State<ControlState>,
) -> Result<Json<bitrouter_ai::control::administration::ProvidersReport>, ControlError> {
    Ok(Json(state.administration()?.providers()))
}

async fn observe(
    State(state): State<ControlState>,
) -> Result<Json<bitrouter_ai::control::administration::ObserveReport>, ControlError> {
    Ok(Json(state.administration()?.observe()))
}

async fn agents(
    State(state): State<ControlState>,
) -> Result<Json<bitrouter_ai::control::administration::AgentsReport>, ControlError> {
    Ok(Json(state.administration()?.agents()))
}

async fn policy_status(
    State(state): State<ControlState>,
    query: Result<Query<PolicyInput>, QueryRejection>,
) -> Result<Json<bitrouter_ai::control::administration::PolicyReport>, ControlError> {
    let Query(input) = query.map_err(|_| ControlError::bad_request("invalid policy input"))?;
    if input.name.is_some() {
        return Err(ControlError::bad_request(
            "policy status does not accept a name",
        ));
    }
    state
        .administration()?
        .policy(input)
        .await
        .map(Json)
        .map_err(ControlError::bad_request)
}

async fn policy_show(
    State(state): State<ControlState>,
    query: Result<Query<PolicyInput>, QueryRejection>,
) -> Result<Json<bitrouter_ai::control::administration::PolicyReport>, ControlError> {
    let Query(input) = query.map_err(|_| ControlError::bad_request("invalid policy input"))?;
    if input.name.is_none() {
        return Err(ControlError::bad_request("policy show requires a name"));
    }
    state
        .administration()?
        .policy(input)
        .await
        .map(Json)
        .map_err(ControlError::bad_request)
}

async fn not_found() -> ControlError {
    ControlError::new(
        StatusCode::NOT_FOUND,
        "not_found",
        "control endpoint not found",
    )
}

async fn method_not_allowed() -> ControlError {
    ControlError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "control endpoint does not support this HTTP method",
    )
}

#[derive(Debug)]
struct ControlError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ControlError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "valid control bearer token required",
        )
    }

    fn bad_request(error: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", error.to_string())
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(error = %error, "remote control action failed");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "control action failed",
        )
    }
}

impl IntoResponse for ControlError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(ErrorEnvelope {
                error: ErrorBody {
                    code: self.code.into(),
                    message: self.message,
                },
            }),
        )
            .into_response();
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::control::{HttpControlClient, normalize_endpoint};
    use axum::body::Body;
    use http::Request;
    use tower::ServiceExt;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn test_router(source: ConfigSource, socket: PathBuf) -> anyhow::Result<Router> {
        control_router(source, socket, TOKEN.as_bytes().to_vec())
    }

    fn default_source() -> anyhow::Result<(tempfile::TempDir, ConfigSource)> {
        let directory = tempfile::tempdir()?;
        Ok((
            directory,
            ConfigSource::Default {
                home: PathBuf::from("/nonexistent-bitrouter-test-home"),
            },
        ))
    }

    fn configured_source() -> anyhow::Result<(tempfile::TempDir, ConfigSource)> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("bitrouter.yaml");
        std::fs::write(
            &path,
            r#"
server:
  listen: "127.0.0.1:4356"
  skip_auth: true
database:
  url: "sqlite://meter.db"
providers:
  fixture:
    api_base: https://example.invalid/v1
    api_key: test-key
    models: [{ id: test-model }]
"#,
        )?;
        Ok((directory, ConfigSource::File(path)))
    }

    fn authorized(path: &str) -> anyhow::Result<Request<Body>> {
        Ok(Request::builder()
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())?)
    }

    struct ImmediateReloader(Arc<crate::reload::ReloadCoordinator>, bool);
    #[async_trait::async_trait]
    impl crate::daemon::DaemonReloader for ImmediateReloader {
        async fn reload(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn reload_state(&self) -> Option<bitrouter_ai::control::reload::ReloadState> {
            Some(self.0.state())
        }
        async fn configuration_state(
            &self,
        ) -> Option<bitrouter_ai::control::reload::ConfigurationState> {
            let reload = self.0.state();
            Some(bitrouter_ai::control::reload::ConfigurationState {
                server_instance_id: Some(reload.server_instance_id),
                generation: Some(reload.generation),
                source: bitrouter_ai::control::reload::ConfigSourceKind::File,
                saved: bitrouter_ai::control::reload::SavedConfigState::Available,
                running: bitrouter_ai::control::reload::RunningConfigState::InSync,
                reload_required_fields: Vec::new(),
                restart_required_fields: Vec::new(),
                named_policy: bitrouter_ai::control::reload::AuxiliaryConfigState::NotConfigured,
                access_policies: bitrouter_ai::control::reload::AuxiliaryConfigState::NotConfigured,
                last_reload: reload.last_outcome,
                mixed_state_history: reload.mixed_state_history,
            })
        }
        fn reserve_remote(
            &self,
            instance: &str,
            generation: u64,
        ) -> Result<crate::reload::ReloadReservation, crate::reload::ReloadAdmissionError> {
            self.0.reserve_remote(instance, generation)
        }
        async fn reload_reserved(
            &self,
            reservation: crate::reload::ReloadReservation,
        ) -> bitrouter_ai::control::reload::ReloadReport {
            let mut report = bitrouter_ai::control::reload::ReloadReport::succeeded(
                reservation.server_instance_id().into(),
                reservation.generation(),
            );
            if self.1 {
                report.outcome = bitrouter_ai::control::reload::ReloadOutcome::PartiallyApplied;
                for participant in &mut report.participants {
                    if participant.participant
                        == bitrouter_ai::control::reload::ReloadParticipant::RoutingTable
                    {
                        participant.outcome =
                            bitrouter_ai::control::reload::ReloadParticipantOutcome::Applied;
                    }
                    if participant.participant
                        == bitrouter_ai::control::reload::ReloadParticipant::NamedPolicyRuntime
                    {
                        participant.outcome =
                            bitrouter_ai::control::reload::ReloadParticipantOutcome::Failed;
                        participant.error = Some(bitrouter_ai::control::reload::ReloadFailure {
                            code: "fixture_policy_failure".into(),
                            message: "injected policy table failure".into(),
                        });
                    }
                }
            }
            self.0.complete(&reservation, report.clone());
            report
        }
    }

    #[test]
    fn legacy_remote_state_keeps_configuration_evidence_unknown() -> anyhow::Result<()> {
        let coordinator = crate::reload::ReloadCoordinator::new();
        let mut value = serde_json::to_value(coordinator.state())?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("reload state did not serialize as an object"))?;
        object.insert("live_policy_digest".to_string(), serde_json::Value::Null);

        let report: StateReport = serde_json::from_value(value)?;
        assert!(report.config_state.is_none());
        Ok(())
    }

    /// Separate test executable used only by Docker acceptance. This exercises
    /// real HTTP/CLI/TUI rendering of a fault; AppReloader tests separately
    /// inject faults at the actual participant boundaries.
    #[tokio::test]
    #[ignore = "long-running Docker HTTP fault fixture"]
    async fn docker_partial_fixture() -> anyhow::Result<()> {
        use crate::config::ControlCredentialConfig;
        let config = ControlConfig {
            credentials: vec![
                ControlCredentialConfig {
                    id: "reader".into(),
                    token_env: "SERVER_READER_TOKEN".into(),
                    scopes: vec![ControlScope::Read],
                },
                ControlCredentialConfig {
                    id: "administrator".into(),
                    token_env: "SERVER_ADMIN_TOKEN".into(),
                    scopes: vec![ControlScope::Read, ControlScope::Reload],
                },
            ],
            ..ControlConfig::default()
        };
        let auth = ControlAuth::from_config(&config)?;
        let reloader = Arc::new(ImmediateReloader(
            crate::reload::ReloadCoordinator::new(),
            true,
        ));
        let router = control_router_with(
            ControlState {
                source: ConfigSource::Default {
                    home: PathBuf::from("/nonexistent-fault-fixture"),
                },
                socket: PathBuf::from("/nonexistent-fault-fixture/control.sock"),
                administration: None,
                instance: reloader.0.state().server_instance_id,
                operations: Some(Arc::new(OperationService::new(reloader))),
            },
            auth,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:4358").await?;
        axum::serve(listener, router).await?;
        Ok(())
    }

    #[tokio::test]
    async fn http_mutations_enforce_scope_shape_owner_and_boot() -> anyhow::Result<()> {
        use crate::config::ControlCredentialConfig;
        let (_directory, source) = default_source()?;
        let reloader = Arc::new(ImmediateReloader(
            crate::reload::ReloadCoordinator::new(),
            false,
        ));
        let instance = reloader.0.state().server_instance_id;
        let config = ControlConfig {
            credentials: vec![
                ControlCredentialConfig {
                    id: "reader".into(),
                    token_env: "READER".into(),
                    scopes: vec![ControlScope::Read],
                },
                ControlCredentialConfig {
                    id: "admin".into(),
                    token_env: "ADMIN".into(),
                    scopes: vec![ControlScope::Read, ControlScope::Reload],
                },
                ControlCredentialConfig {
                    id: "other".into(),
                    token_env: "OTHER".into(),
                    scopes: vec![ControlScope::Read, ControlScope::Reload],
                },
            ],
            ..ControlConfig::default()
        };
        let auth = ControlAuth::from_lookup(&config, |name| Some(name.repeat(32)))?;
        let router = control_router_with(
            ControlState {
                source,
                socket: PathBuf::from("missing.sock"),
                administration: None,
                instance: instance.clone(),
                operations: Some(Arc::new(OperationService::new(reloader))),
            },
            auth,
        );
        let input = ReloadInput {
            request_id: uuid::Uuid::new_v4().to_string(),
            expected_server_instance_id: instance.clone(),
            expected_generation: 0,
        };
        let request = |token: &str,
                       method: &str,
                       path: &str,
                       body: String|
         -> anyhow::Result<Request<Body>> {
            Ok(Request::builder()
                .method(method)
                .uri(path)
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", token.repeat(32)),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))?)
        };
        for path in [
            "/control/v1/reload",
            "/control/v1/operations/00000000-0000-0000-0000-000000000000?instance=00000000-0000-0000-0000-000000000000",
        ] {
            let method = if path.ends_with("reload") {
                "POST"
            } else {
                "GET"
            };
            let response = router
                .clone()
                .oneshot(request(
                    "READER",
                    method,
                    path,
                    serde_json::to_string(&input)?,
                )?)
                .await?;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        for body in [
            format!("{{\"env\":[],{}", &serde_json::to_string(&input)?[1..]),
            "x".repeat(16 * 1024 + 1),
        ] {
            let response = router
                .clone()
                .oneshot(request("ADMIN", "POST", "/control/v1/reload", body)?)
                .await?;
            assert!(response.status().is_client_error());
        }
        let response = router
            .clone()
            .oneshot(request(
                "ADMIN",
                "POST",
                "/control/v1/reload",
                serde_json::to_string(&input)?,
            )?)
            .await?;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
        let path = format!(
            "/control/v1/operations/{}?instance={instance}",
            input.request_id
        );
        let response = router
            .clone()
            .oneshot(request("OTHER", "GET", &path, String::new())?)
            .await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = router
            .clone()
            .oneshot(request("ADMIN", "GET", &path, String::new())?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let changed = format!(
            "/control/v1/operations/{}?instance={}",
            input.request_id,
            uuid::Uuid::new_v4()
        );
        let response = router
            .oneshot(request("ADMIN", "GET", &changed, String::new())?)
            .await?;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        Ok(())
    }

    #[tokio::test]
    async fn legacy_checker_management_routes_are_not_exposed() -> anyhow::Result<()> {
        let (_directory, source) = default_source()?;
        let router = test_router(source, PathBuf::from("missing.sock"))?;
        for (method, path) in [
            ("GET", "/control/v1/checks"),
            ("POST", "/control/v1/checks/company/probe"),
            ("GET", "/control/v1/checks/receipts?limit=10"),
            ("GET", "/control/v1/checks/receipts/request-1"),
        ] {
            let request = Request::builder()
                .method(method)
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())?;
            let response = router.clone().oneshot(request).await?;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        Ok(())
    }

    #[tokio::test]
    async fn every_endpoint_requires_the_control_token() -> anyhow::Result<()> {
        let (_directory, source) = default_source()?;
        let response = test_router(source, PathBuf::from("missing.sock"))?
            .oneshot(
                Request::builder()
                    .uri("/control/v1/capabilities")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL),
            Some(&header::HeaderValue::from_static("no-store"))
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        assert_eq!(value["error"]["code"], "unauthorized");
        Ok(())
    }

    #[tokio::test]
    async fn every_inventory_route_authenticates_before_extracting_inputs() -> anyhow::Result<()> {
        let (_directory, source) = default_source()?;
        let router = test_router(source, PathBuf::from("missing.sock"))?;
        let routes = inventory::ACTIONS
            .iter()
            .map(|row| (row.method, row.path))
            .chain(inventory::RESOURCES.iter().map(|row| ("GET", row.path)));
        for (method, path) in routes {
            let path = path.replace("{request_id}", "00000000-0000-0000-0000-000000000000");
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(&path)
                        .body(Body::from("invalid-input"))?,
                )
                .await?;
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {path}"
            );
            assert_eq!(
                response
                    .headers()
                    .get(header::CACHE_CONTROL)
                    .and_then(|value| value.to_str().ok()),
                Some("no-store")
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn ambiguous_post_response_retains_ids_without_an_automatic_retry() -> anyhow::Result<()>
    {
        use crate::config::ControlCredentialConfig;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (_directory, source) = default_source()?;
        let reloader = Arc::new(ImmediateReloader(
            crate::reload::ReloadCoordinator::new(),
            false,
        ));
        let instance = reloader.0.state().server_instance_id;
        let operations = Arc::new(OperationService::new(reloader));
        let config = ControlConfig {
            credentials: vec![ControlCredentialConfig {
                id: "admin".into(),
                token_env: "TOKEN".into(),
                scopes: vec![ControlScope::Read, ControlScope::Reload],
            }],
            ..ControlConfig::default()
        };
        let auth = ControlAuth::from_lookup(&config, |_| Some(TOKEN.into()))?;
        let posts = Arc::new(AtomicUsize::new(0));
        let captured = posts.clone();
        let router = control_router_with(
            ControlState {
                source,
                socket: PathBuf::from("missing.sock"),
                administration: None,
                instance: instance.clone(),
                operations: Some(operations.clone()),
            },
            auth,
        )
        .layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: Next| {
                let captured = captured.clone();
                async move {
                    let mutate = request.uri().path() == "/control/v1/reload";
                    let response = next.run(request).await;
                    if mutate && response.status().is_success() {
                        captured.fetch_add(1, Ordering::SeqCst);
                        let (parts, _) = response.into_parts();
                        Response::from_parts(parts, Body::from("{truncated"))
                    } else {
                        response
                    }
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        let client = HttpControlClient::from_token(&endpoint, TOKEN.into(), "fixture")?;
        let result = client.reload().await;
        assert!(result.is_err());
        let message = result
            .err()
            .ok_or_else(|| anyhow::anyhow!("expected interrupted receipt"))?
            .to_string();
        let request_id = message
            .split_whitespace()
            .find(|word| uuid::Uuid::parse_str(word).is_ok())
            .ok_or_else(|| anyhow::anyhow!("recovery error omitted request UUID: {message}"))?;
        assert!(message.contains(&instance));
        let operation = client.operation(request_id, &instance).await?;
        assert!(operation.succeeded());
        assert_eq!(posts.load(Ordering::SeqCst), 1);
        assert_eq!(operations.state()?.generation, 1);
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn redirects_never_forward_control_credentials() -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = Arc::new(AtomicUsize::new(0));
        let target_hits = hits.clone();
        let destination = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let destination_url = format!(
            "http://{}/control/v1/capabilities",
            destination.local_addr()?
        );
        let destination_task = tokio::spawn(async move {
            axum::serve(
                destination,
                Router::new().fallback(move || {
                    target_hits.fetch_add(1, Ordering::SeqCst);
                    async { "credential must never arrive here" }
                }),
            )
            .await
        });
        let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", origin.local_addr()?);
        let origin_task = tokio::spawn(async move {
            axum::serve(
                origin,
                Router::new().route(
                    "/control/v1/capabilities",
                    get(move || {
                        let destination_url = destination_url.clone();
                        async move {
                            (
                                StatusCode::TEMPORARY_REDIRECT,
                                [(header::LOCATION, destination_url)],
                            )
                        }
                    }),
                ),
            )
            .await
        });
        let client = HttpControlClient::from_token(&endpoint, TOKEN.into(), "fixture")?;
        assert!(client.capabilities().await.is_err());
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        origin_task.abort();
        destination_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn old_server_capabilities_support_only_legacy_reads() -> anyhow::Result<()> {
        let router = Router::new()
            .route("/control/v1/capabilities", get(|| async {
                Json(serde_json::json!({
                    "protocol": "bitrouter-control", "protocol_version": 1,
                    "server_version": "old", "actions": ["status", "models", "route_preview", "requests"]
                }))
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = stop_rx.await;
                })
                .await
        });
        let client =
            HttpControlClient::from_token(&format!("http://{address}"), TOKEN.into(), "test")?;
        assert!(client.can_action("list_models").await?);
        assert!(client.can_action("route").await?);
        assert!(!client.can_action("providers_list").await?);
        let _ = stop_tx.send(());
        task.await??;
        Ok(())
    }

    #[tokio::test]
    async fn browser_origin_must_match_control_host() -> anyhow::Result<()> {
        let (_directory, source) = default_source()?;
        let response = test_router(source, PathBuf::from("missing.sock"))?
            .oneshot(
                Request::builder()
                    .uri("/control/v1/capabilities")
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .header(header::HOST, "127.0.0.1:4358")
                    .header(header::ORIGIN, "https://attacker.example")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        Ok(())
    }

    #[tokio::test]
    async fn capabilities_advertise_only_implemented_actions() -> anyhow::Result<()> {
        let (_directory, source) = default_source()?;
        let response = test_router(source, PathBuf::from("missing.sock"))?
            .oneshot(authorized("/control/v1/capabilities")?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL),
            Some(&header::HeaderValue::from_static("no-store"))
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        let report: CapabilitiesReport = serde_json::from_slice(&bytes)?;
        assert_eq!(report.protocol_version, PROTOCOL_VERSION);
        assert_eq!(
            report.actions,
            inventory::ACTIONS
                .iter()
                .filter(|row| !row.requires_administration && !row.requires_reload)
                .map(|row| row.legacy_name.to_string())
                .collect::<Vec<_>>()
        );
        Ok(())
    }

    #[tokio::test]
    async fn status_does_not_disclose_the_server_socket() -> anyhow::Result<()> {
        let (directory, source) = default_source()?;
        let socket = directory.path().join("private-control.sock");
        let response = test_router(source, socket)?
            .oneshot(authorized("/control/v1/status")?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        let report: StatusReport = serde_json::from_slice(&bytes)?;
        assert!(!report.running);
        assert!(report.socket.is_none());
        let config_state = report
            .config_state
            .ok_or_else(|| anyhow::anyhow!("remote status omitted server-side saved state"))?;
        assert_eq!(
            config_state.source,
            bitrouter_ai::control::reload::ConfigSourceKind::Default
        );
        assert_eq!(
            config_state.saved,
            bitrouter_ai::control::reload::SavedConfigState::Generated
        );
        assert_eq!(
            config_state.running,
            bitrouter_ai::control::reload::RunningConfigState::Unknown
        );
        Ok(())
    }

    #[tokio::test]
    async fn remote_state_uses_daemon_reloader_configuration_evidence() -> anyhow::Result<()> {
        use crate::config::ControlCredentialConfig;

        let (_directory, source) = default_source()?;
        let reloader = Arc::new(ImmediateReloader(
            crate::reload::ReloadCoordinator::new(),
            false,
        ));
        let config = ControlConfig {
            credentials: vec![ControlCredentialConfig {
                id: "reader".into(),
                token_env: "TOKEN".into(),
                scopes: vec![ControlScope::Read],
            }],
            ..ControlConfig::default()
        };
        let auth = ControlAuth::from_lookup(&config, |_| Some(TOKEN.into()))?;
        let router = control_router_with(
            ControlState {
                source,
                socket: PathBuf::from("missing.sock"),
                administration: None,
                instance: reloader.0.state().server_instance_id,
                operations: Some(Arc::new(OperationService::new(reloader))),
            },
            auth,
        );

        let response = router.oneshot(authorized("/control/v1/state")?).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        let report: StateReport = serde_json::from_slice(&bytes)?;
        let config_state = report
            .config_state
            .ok_or_else(|| anyhow::anyhow!("remote state omitted daemon evidence"))?;
        assert_eq!(
            config_state.source,
            bitrouter_ai::control::reload::ConfigSourceKind::File
        );
        assert_eq!(
            config_state.saved,
            bitrouter_ai::control::reload::SavedConfigState::Available
        );
        assert_eq!(
            config_state.running,
            bitrouter_ai::control::reload::RunningConfigState::InSync
        );
        Ok(())
    }

    #[tokio::test]
    async fn http_client_handshake_covers_every_read_action() -> anyhow::Result<()> {
        let (directory, source) = configured_source()?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let router = test_router(source, directory.path().join("private-control.sock"))?;
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
        });

        let client = HttpControlClient::from_token(
            &format!("http://{address}"),
            TOKEN.to_string(),
            "test token",
        )?;
        let report = client.status().await?;
        assert!(!report.running);
        assert!(report.socket.is_none());
        let config_state = report
            .config_state
            .ok_or_else(|| anyhow::anyhow!("HTTP status omitted server-side saved state"))?;
        assert_eq!(
            config_state.source,
            bitrouter_ai::control::reload::ConfigSourceKind::File
        );
        assert_eq!(
            config_state.saved,
            bitrouter_ai::control::reload::SavedConfigState::Available
        );
        assert_eq!(
            config_state.running,
            bitrouter_ai::control::reload::RunningConfigState::Unknown
        );
        let models = client.models(None).await?;
        assert!(models.models.iter().any(|model| model.id == "test-model"));
        let route = client
            .route(&RouteInput {
                model: "test-model".to_string(),
                prompt: None,
            })
            .await?;
        assert_eq!(route.effective_model, "test-model");
        assert_eq!(route.provider_chain.len(), 1);
        let requests = client.requests(Some(1)).await?;
        assert!(requests.rows.is_empty());

        let denied = HttpControlClient::from_token(
            &format!("http://{address}"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            "test token",
        )?
        .status()
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("wrong token unexpectedly succeeded"))?;
        let kind = denied
            .chain()
            .find_map(|error| error.downcast_ref::<BitrouterError>())
            .map(BitrouterError::kind);
        assert_eq!(kind, Some(crate::gateway::error::ErrorKind::Unauthorized));

        let _ = shutdown_tx.send(());
        task.await??;
        Ok(())
    }

    #[test]
    fn remote_plain_http_and_credentials_in_urls_are_rejected() {
        let insecure = normalize_endpoint("http://router.example/control/v1")
            .err()
            .map(|error| error.to_string());
        assert!(insecure.is_some_and(|message| message.contains("plain HTTP")));

        let credentials = normalize_endpoint("https://user:secret@router.example/control/v1")
            .err()
            .map(|error| error.to_string());
        assert!(credentials.is_some_and(|message| message.contains("user information")));
    }

    #[test]
    fn enabled_listener_must_be_loopback() {
        let config = ControlConfig {
            enabled: true,
            listen: "0.0.0.0:4358".to_string(),
            ..Default::default()
        };
        let error = ControlServer::from_config(
            &config,
            ConfigSource::Default {
                home: PathBuf::from("."),
            },
            PathBuf::from("bitrouter.sock"),
        )
        .err()
        .map(|error| error.to_string());
        assert!(error.is_some_and(|message| message.contains("loopback")));
    }
}
