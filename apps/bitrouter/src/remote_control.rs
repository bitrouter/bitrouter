//! Authenticated, read-only HTTP control plane for remote BitRouter clients.
//!
//! This is intentionally not the inference API, the local daemon protocol, or
//! ACP over a network transport. It exposes the existing typed read actions on
//! a dedicated loopback listener so an operator can put a private tunnel or a
//! TLS reverse proxy in front of it without making local IPC remotely callable.

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{
    Query, Request, State,
    rejection::{JsonRejection, QueryRejection},
};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bitrouter_mcp::actions::models::ModelsReport;
use bitrouter_mcp::actions::route::{RouteInput, RouteReport};
use bitrouter_mcp::actions::status::StatusReport;
use bitrouter_sdk::config::ControlConfig;
use bitrouter_sdk::error::BitrouterError;
use futures::StreamExt;
use hmac::{Hmac, KeyInit, Mac};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use url::{Host, Url};

use crate::actions::models::RoutableModels;
use crate::actions::requests::{MAX_REQUEST_ROWS, RequestsAction};
use crate::actions::route::RouteAction;
use crate::actions::status::DaemonStatus;
use crate::output::reports::requests::RequestsReport;
use crate::paths::ConfigSource;

/// Environment variable containing the remote-control bearer token.
pub const CONTROL_TOKEN_ENV: &str = "BITROUTER_CONTROL_TOKEN";

/// Current HTTP control protocol version.
pub const PROTOCOL_VERSION: u32 = 1;

/// Actions implemented by this protocol version.
const ACTIONS: [&str; 4] = ["status", "models", "route_preview", "requests"];

/// Minimum token size accepted by the server.
const MIN_TOKEN_BYTES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilitiesReport {
    pub protocol: String,
    pub protocol_version: u32,
    pub server_version: String,
    pub actions: Vec<String>,
}

impl CapabilitiesReport {
    fn current() -> Self {
        Self {
            protocol: "bitrouter-control".to_string(),
            protocol_version: PROTOCOL_VERSION,
            server_version: crate::VERSION.to_string(),
            actions: ACTIONS.iter().map(ToString::to_string).collect(),
        }
    }
}

#[derive(Clone)]
struct ControlState {
    source: ConfigSource,
    socket: PathBuf,
}

#[derive(Clone)]
struct ControlToken(Arc<[u8]>);

/// A validated control listener that is ready to run.
pub struct ControlServer {
    listen: SocketAddr,
    router: Router,
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

        let token = std::env::var(CONTROL_TOKEN_ENV).with_context(|| {
            format!(
                "control.enabled is true but {CONTROL_TOKEN_ENV} is not set; provide a \
                 dedicated operator token of at least {MIN_TOKEN_BYTES} bytes"
            )
        })?;
        if token.len() < MIN_TOKEN_BYTES {
            anyhow::bail!(
                "{CONTROL_TOKEN_ENV} must contain at least {MIN_TOKEN_BYTES} bytes when \
                 control.enabled is true"
            );
        }

        Ok(Some(Self {
            listen,
            router: control_router(source, socket, token.into_bytes())?,
        }))
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
            router: self.router,
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

fn control_router(source: ConfigSource, socket: PathBuf, token: Vec<u8>) -> Result<Router> {
    let state = ControlState { source, socket };
    let token_digest = digest_token(&token)?;
    let auth = axum::middleware::from_fn_with_state(
        ControlToken(Arc::from(token_digest)),
        require_control_token,
    );

    Ok(Router::new()
        .route("/control/v1/capabilities", get(capabilities))
        .route("/control/v1/status", get(status))
        .route("/control/v1/models", get(models))
        .route("/control/v1/route/preview", post(route_preview))
        .route("/control/v1/requests", get(requests))
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .layer(auth)
        .with_state(state))
}

async fn require_control_token(
    State(expected): State<ControlToken>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.as_bytes().strip_prefix(b"Bearer "));
    let valid = supplied.is_some_and(|token| {
        let verifier = Hmac::<Sha256>::new_from_slice(b"bitrouter-control-token-compare-v1");
        verifier.is_ok_and(|mut verifier| {
            verifier.update(token);
            verifier.verify_slice(expected.0.as_ref()).is_ok()
        })
    });
    if !valid {
        return ControlError::unauthorized().into_response();
    }

    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

fn digest_token(token: &[u8]) -> Result<Vec<u8>> {
    let mut digest = Hmac::<Sha256>::new_from_slice(b"bitrouter-control-token-compare-v1")
        .map_err(|_| anyhow::anyhow!("initialize control token comparison"))?;
    digest.update(token);
    Ok(digest.finalize().into_bytes().to_vec())
}

async fn capabilities() -> Json<CapabilitiesReport> {
    Json(CapabilitiesReport::current())
}

async fn status(State(state): State<ControlState>) -> Result<Json<StatusReport>, ControlError> {
    let mut report = DaemonStatus::new(&state.socket, Some(state.source))
        .report()
        .await
        .map_err(ControlError::internal)?;
    // The local IPC path is an implementation detail of the server host. It is
    // neither actionable remotely nor appropriate to disclose.
    report.socket = None;
    Ok(Json(report))
}

#[derive(Debug, Default, Deserialize)]
struct ModelsParams {
    provider: Option<String>,
}

async fn models(
    State(state): State<ControlState>,
    query: Result<Query<ModelsParams>, QueryRejection>,
) -> Result<Json<ModelsReport>, ControlError> {
    let Query(params) = query.map_err(|error| ControlError::bad_request(error.body_text()))?;
    let report = RoutableModels::new(state.source, Some(state.socket))
        .report()
        .await
        .map_err(ControlError::internal)?
        .filtered(params.provider.as_deref());
    Ok(Json(report))
}

async fn route_preview(
    State(state): State<ControlState>,
    payload: Result<Json<RouteInput>, JsonRejection>,
) -> Result<Json<RouteReport>, ControlError> {
    let Json(input) = payload.map_err(|error| ControlError::bad_request(error.body_text()))?;
    let report = RouteAction::new(state.source, Some(state.socket))
        .report(input)
        .await
        .map_err(ControlError::bad_request)?;
    Ok(Json(report))
}

#[derive(Debug, Deserialize)]
struct RequestsParams {
    #[serde(default = "default_request_limit")]
    limit: u64,
}

fn default_request_limit() -> u64 {
    MAX_REQUEST_ROWS
}

async fn requests(
    State(state): State<ControlState>,
    query: Result<Query<RequestsParams>, QueryRejection>,
) -> Result<Json<RequestsReport>, ControlError> {
    let Query(params) = query.map_err(|error| ControlError::bad_request(error.body_text()))?;
    let report = RequestsAction::new(state.source, state.socket)
        .report(params.limit)
        .await
        .map_err(ControlError::bad_request)?;
    Ok(Json(report))
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

#[derive(Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

impl IntoResponse for ControlError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(ErrorEnvelope {
                error: ErrorBody {
                    code: self.code,
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

/// HTTP implementation of the read-only control actions.
///
/// Every action performs the capability handshake first. A successful result
/// is cached for this client instance, so a long-lived TUI checks once while a
/// one-shot CLI still checks every invocation. A version mismatch or
/// unsupported action is more useful than interpreting a coincidental response
/// shape.
pub struct HttpControlClient {
    endpoint: Url,
    token: String,
    http: reqwest::Client,
    capabilities: tokio::sync::OnceCell<CapabilitiesReport>,
}

impl HttpControlClient {
    /// Construct a client for an origin or `/control/v1` endpoint.
    ///
    /// Plain HTTP is accepted only for a loopback URL, which supports local
    /// development and SSH port-forwarding without normalizing insecure remote
    /// deployment into the CLI. Redirects are disabled so a bearer cannot be
    /// forwarded to another origin.
    pub fn new(endpoint: &str, token_env: &str) -> Result<Self> {
        let token = std::env::var(token_env).map_err(|_| {
            BitrouterError::Unauthorized(format!(
                "remote control token environment variable {token_env} is not set"
            ))
        })?;
        Self::from_token(endpoint, token, token_env)
    }

    fn from_token(endpoint: &str, token: String, token_source: &str) -> Result<Self> {
        let endpoint = normalize_endpoint(endpoint).map_err(BitrouterError::bad_request)?;
        if token.len() < MIN_TOKEN_BYTES {
            return Err(BitrouterError::Unauthorized(format!(
                "remote control token from {token_source} must contain at least \
                 {MIN_TOKEN_BYTES} bytes"
            ))
            .into());
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(3))
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .context("build remote control HTTP client")?;
        Ok(Self {
            endpoint,
            token,
            http,
            capabilities: tokio::sync::OnceCell::new(),
        })
    }

    pub async fn capabilities(&self) -> Result<CapabilitiesReport> {
        self.capabilities
            .get_or_try_init(|| self.get("capabilities", None))
            .await
            .cloned()
    }

    pub async fn status(&self) -> Result<StatusReport> {
        self.require_action("status").await?;
        self.get("status", None).await
    }

    pub async fn models(&self, provider: Option<&str>) -> Result<ModelsReport> {
        self.require_action("models").await?;
        let query = provider.map(|provider| vec![("provider", provider)]);
        self.get("models", query.as_deref()).await
    }

    pub async fn route(&self, input: &RouteInput) -> Result<RouteReport> {
        self.require_action("route_preview").await?;
        let url = self.action_url("route/preview")?;
        let request = self.http.post(url).bearer_auth(&self.token).json(input);
        self.send(request).await
    }

    pub async fn requests(&self, limit: Option<u64>) -> Result<RequestsReport> {
        self.require_action("requests").await?;
        let limit = limit.map(|limit| limit.to_string());
        let query = limit.as_deref().map(|limit| vec![("limit", limit)]);
        self.get("requests", query.as_deref()).await
    }

    async fn require_action(&self, action: &str) -> Result<()> {
        let capabilities = self.capabilities().await?;
        if capabilities.protocol != "bitrouter-control" {
            return Err(BitrouterError::UpstreamInvalidResponse {
                message: format!(
                    "remote endpoint speaks unsupported protocol '{}'",
                    capabilities.protocol
                ),
            }
            .into());
        }
        if capabilities.protocol_version != PROTOCOL_VERSION {
            return Err(BitrouterError::UpstreamInvalidResponse {
                message: format!(
                    "remote control protocol version {} is incompatible with client version {}",
                    capabilities.protocol_version, PROTOCOL_VERSION
                ),
            }
            .into());
        }
        if !capabilities
            .actions
            .iter()
            .any(|candidate| candidate == action)
        {
            return Err(BitrouterError::UpstreamInvalidResponse {
                message: format!("remote BitRouter does not support control action '{action}'"),
            }
            .into());
        }
        Ok(())
    }

    async fn get<T>(&self, path: &str, query: Option<&[(&str, &str)]>) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let url = self.action_url(path)?;
        let mut request = self.http.get(url).bearer_auth(&self.token);
        if let Some(query) = query {
            request = request.query(query);
        }
        self.send(request).await
    }

    async fn send<T>(&self, request: reqwest::RequestBuilder) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                let classified = if error.is_timeout() {
                    BitrouterError::UpstreamTimeout
                } else {
                    BitrouterError::UpstreamUnavailable
                };
                return Err(anyhow::Error::new(classified)).with_context(|| {
                    format!("remote control endpoint {} is unreachable", self.endpoint)
                });
            }
        };
        let status = response.status();
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| BitrouterError::UpstreamInvalidResponse {
                message: format!("read remote control response: {error}"),
            })?;
            if body.len().saturating_add(chunk.len()) > 8 * 1024 * 1024 {
                return Err(BitrouterError::UpstreamInvalidResponse {
                    message: "remote control response exceeded the 8 MiB client limit".to_string(),
                }
                .into());
            }
            body.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            if let Ok(envelope) = serde_json::from_slice::<RemoteErrorEnvelope>(&body) {
                return Err(map_remote_error(status, envelope.error).into());
            }
            return Err(BitrouterError::Upstream {
                status: status.as_u16(),
                message: "remote control returned an invalid error response".to_string(),
            }
            .into());
        }
        serde_json::from_slice(&body).map_err(|error| {
            BitrouterError::UpstreamInvalidResponse {
                message: format!("decode remote control response: {error}"),
            }
            .into()
        })
    }

    fn action_url(&self, path: &str) -> Result<Url> {
        self.endpoint
            .join(path)
            .with_context(|| format!("build remote control URL for {path}"))
    }
}

pub(crate) fn normalize_endpoint(endpoint: &str) -> Result<Url> {
    let mut url =
        Url::parse(endpoint).with_context(|| format!("invalid control endpoint {endpoint}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("remote control endpoint must not contain user information");
    }
    if url.query().is_some() || url.fragment().is_some() {
        anyhow::bail!("remote control endpoint must not contain a query or fragment");
    }
    match url.scheme() {
        "https" => {}
        "http" if url_host_is_loopback(&url) => {}
        "http" => anyhow::bail!(
            "plain HTTP remote control is allowed only on loopback; use HTTPS or an SSH tunnel"
        ),
        scheme => anyhow::bail!("unsupported remote control URL scheme '{scheme}'"),
    }
    match url.path().trim_end_matches('/') {
        "" => url.set_path("/control/v1/"),
        "/control/v1" => url.set_path("/control/v1/"),
        path => {
            anyhow::bail!("remote control endpoint path must be empty or /control/v1, got '{path}'")
        }
    }
    Ok(url)
}

fn url_host_is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

#[derive(Deserialize)]
struct RemoteErrorEnvelope {
    error: RemoteErrorBody,
}

#[derive(Deserialize)]
struct RemoteErrorBody {
    code: String,
    message: String,
}

fn map_remote_error(status: StatusCode, error: RemoteErrorBody) -> BitrouterError {
    match error.code.as_str() {
        "unauthorized" => BitrouterError::Unauthorized(error.message),
        "bad_request" | "method_not_allowed" => BitrouterError::bad_request(error.message),
        "not_found" => BitrouterError::NotFound(error.message),
        _ => BitrouterError::Upstream {
            status: status.as_u16(),
            message: error.message,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(report.actions, ACTIONS.map(ToString::to_string));
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
        assert_eq!(kind, Some(bitrouter_sdk::error::ErrorKind::Unauthorized));

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
