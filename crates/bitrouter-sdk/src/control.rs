//! Authenticated control HTTP client with explicit service options.
//!
//! This client never discovers local profiles, credentials, configuration or a
//! daemon. Capability discovery does not replace the server's authorization.

use crate::error::{Error, Result, ServiceFailure};
use bitrouter_ai::control::action::Action;
use bitrouter_ai::control::administration::PolicyInput;
use bitrouter_ai::control::models::ModelsReport;
use bitrouter_ai::control::reload::{OperationReport, ReloadInput};
use bitrouter_ai::control::requests::RequestsReport;
use bitrouter_ai::control::route::{RouteInput, RouteReport};
use bitrouter_ai::control::status::StatusReport;
use bitrouter_ai::control::{CapabilitiesReport, MIN_TOKEN_BYTES, PROTOCOL_VERSION, StateReport};
use serde::de::DeserializeOwned;
use std::time::Duration;
use url::{Host, Url};

/// Explicit transport policy for the administration listener.
#[derive(Debug, Clone)]
pub struct ControlOptions {
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub max_response_bytes: usize,
    /// Permit system proxy discovery only when the caller explicitly selects it.
    pub system_proxy: bool,
}

impl Default for ControlOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(3),
            request_timeout: Duration::from_secs(15),
            max_response_bytes: 8 * 1024 * 1024,
            system_proxy: false,
        }
    }
}

/// Typed inspection and guarded administration of one service endpoint.
///
/// Successful capabilities are cached for thirty seconds. Boot changes and
/// authorization/availability errors invalidate the cache. No request is retried
/// or redirected, including a reload whose response is lost after dispatch.
pub struct ControlClient {
    endpoint: Url,
    token: String,
    http: reqwest::Client,
    max_response_bytes: usize,
    capabilities: tokio::sync::Mutex<Option<(std::time::Instant, CapabilitiesReport)>>,
}

impl ControlClient {
    pub fn new(endpoint: &str, token: &str) -> Result<Self> {
        Self::with_options(endpoint, token, ControlOptions::default())
    }

    pub fn with_options(endpoint: &str, token: &str, options: ControlOptions) -> Result<Self> {
        let endpoint = parsed_endpoint(endpoint)?;
        if token.len() < MIN_TOKEN_BYTES {
            return Err(Error::Options(
                "control bearer token must contain at least 32 bytes",
            ));
        }
        reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| Error::Options("control bearer token is not a valid HTTP header value"))?;
        if options.connect_timeout.is_zero()
            || options.request_timeout.is_zero()
            || options.max_response_bytes == 0
        {
            return Err(Error::Options(
                "control timeouts and response limit must be positive",
            ));
        }
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(options.connect_timeout)
            .timeout(options.request_timeout);
        if !options.system_proxy {
            builder = builder.no_proxy();
        }
        let http = builder.build().map_err(Error::transport)?;
        Ok(Self {
            endpoint,
            token: token.into(),
            http,
            max_response_bytes: options.max_response_bytes,
            capabilities: tokio::sync::Mutex::new(None),
        })
    }

    pub async fn capabilities(&self) -> Result<CapabilitiesReport> {
        {
            let cache = self.capabilities.lock().await;
            if let Some((fetched, report)) = cache.as_ref()
                && fetched.elapsed() < std::time::Duration::from_secs(30)
            {
                return Ok(report.clone());
            }
        }
        let report = self.get::<CapabilitiesReport>("capabilities", None).await?;
        *self.capabilities.lock().await = Some((std::time::Instant::now(), report.clone()));
        Ok(report)
    }

    async fn require_resource(&self, resource: &'static str) -> Result<()> {
        let capabilities = self.capabilities().await?;
        if capabilities.protocol != "bitrouter-control"
            || capabilities.protocol_version != PROTOCOL_VERSION
            || !capabilities.resources.get(resource).is_some_and(|row| {
                row.version == 1 && capabilities.scopes.contains(&row.required_scope)
            })
        {
            return Err(Error::ControlResourceUnavailable { resource });
        }
        Ok(())
    }

    pub async fn state_report(&self) -> Result<StateReport> {
        self.require_resource("state").await?;
        self.get("state", None).await
    }

    pub async fn state(&self) -> Result<bitrouter_ai::control::reload::ReloadState> {
        Ok(self.state_report().await?.reload)
    }

    pub async fn submit_reload(&self, input: &ReloadInput) -> Result<OperationReport> {
        self.require_action(Action::Reload).await?;
        self.require_resource("state").await?;
        let url = self.action_url("reload")?;
        self.send(self.http.post(url).bearer_auth(&self.token).json(input))
            .await
    }

    pub async fn operation(&self, request_id: &str, instance: &str) -> Result<OperationReport> {
        self.require_resource("operation").await?;
        let request_id = uuid::Uuid::parse_str(request_id)
            .map_err(|_| Error::Request("invalid request UUID"))?;
        uuid::Uuid::parse_str(instance).map_err(|_| Error::Request("invalid instance UUID"))?;
        self.get(
            &format!("operations/{request_id}"),
            Some(&[("instance", instance)]),
        )
        .await
    }

    /// Submit once. A disconnected POST is never retried; identifiers survive in
    /// the error so an operator can recover the daemon-owned operation.
    pub async fn reload(&self) -> Result<OperationReport> {
        self.require_action(Action::Reload).await?;
        let state = self.state().await?;
        let input = ReloadInput {
            request_id: uuid::Uuid::new_v4().to_string(),
            expected_server_instance_id: state.server_instance_id,
            expected_generation: state.generation,
        };
        let recovery = |source| Error::ReloadUnknown {
            input: input.clone(),
            source: Box::new(source),
        };
        let mut report = self.submit_reload(&input).await.map_err(recovery)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while report.is_running() {
            tokio::time::sleep(Duration::from_millis(250)).await;
            match tokio::time::timeout_at(
                deadline,
                self.operation(&input.request_id, &input.expected_server_instance_id),
            )
            .await
            {
                Ok(result) => report = result.map_err(recovery)?,
                Err(_) => break,
            }
        }
        Ok(report)
    }

    pub async fn providers(
        &self,
    ) -> Result<bitrouter_ai::control::administration::ProvidersReport> {
        self.require_action(Action::Providers).await?;
        self.get("providers", None).await
    }

    pub async fn agents(&self) -> Result<bitrouter_ai::control::administration::AgentsReport> {
        self.require_action(Action::Agents).await?;
        self.get("agents", None).await
    }

    pub async fn observe(&self) -> Result<bitrouter_ai::control::administration::ObserveReport> {
        self.require_action(Action::Observe).await?;
        self.get("observe/status", None).await
    }

    pub async fn policy(
        &self,
        input: &PolicyInput,
    ) -> Result<bitrouter_ai::control::administration::PolicyReport> {
        input.validate().map_err(|_| {
            Error::Request("identifier must contain 1–256 bytes and no control characters")
        })?;
        let (action, path) = if input.name.is_some() {
            (Action::PolicyShow, "policy/show")
        } else {
            (Action::PolicyStatus, "policy/status")
        };
        self.require_action(action).await?;
        let mut query = vec![(
            "view",
            match input.view {
                bitrouter_ai::control::administration::PolicyView::Active => "active",
                bitrouter_ai::control::administration::PolicyView::Disk => "disk",
            },
        )];
        if let Some(name) = &input.name {
            query.push(("name", name.as_str()));
        }
        self.get(path, Some(&query)).await
    }

    pub async fn status(&self) -> Result<StatusReport> {
        self.require_action(Action::Status).await?;
        self.get("status", None).await
    }

    pub async fn models(&self, provider: Option<&str>) -> Result<ModelsReport> {
        self.require_action(Action::Models).await?;
        let query = provider.map(|provider| vec![("provider", provider)]);
        self.get("models", query.as_deref()).await
    }

    pub async fn route(&self, input: &RouteInput) -> Result<RouteReport> {
        self.require_action(Action::Route).await?;
        let url = self.action_url("route/preview")?;
        let request = self.http.post(url).bearer_auth(&self.token).json(input);
        self.send(request).await
    }

    pub async fn requests(&self, limit: Option<u64>) -> Result<RequestsReport> {
        self.requests_filtered(
            &bitrouter_ai::control::requests::RequestFilters::with_limit(
                limit.unwrap_or(bitrouter_ai::control::requests::MAX_REQUEST_ROWS),
            ),
        )
        .await
    }

    pub async fn requests_filtered(
        &self,
        filters: &bitrouter_ai::control::requests::RequestFilters,
    ) -> Result<RequestsReport> {
        self.require_action(Action::Requests).await?;
        let has_filters = filters.since.is_some()
            || filters.until.is_some()
            || filters.model.is_some()
            || filters.provider.is_some();
        if has_filters
            && !self
                .capabilities()
                .await?
                .action_descriptors
                .contains_key("requests")
        {
            return Err(Error::Request(
                "remote server does not advertise filtered request inspection",
            ));
        }
        let url = self.action_url("requests")?;
        self.send(self.http.get(url).bearer_auth(&self.token).query(filters))
            .await
    }

    pub async fn can_action(&self, action: Action) -> Result<bool> {
        let capabilities = self.capabilities().await?;
        if capabilities.protocol != "bitrouter-control" {
            return Err(Error::Response(
                "remote endpoint speaks an unsupported control protocol",
            ));
        }
        if capabilities.protocol_version != PROTOCOL_VERSION {
            return Err(Error::ControlProtocolVersion {
                server: capabilities.protocol_version,
                client: PROTOCOL_VERSION,
            });
        }
        Ok(if capabilities.action_descriptors.is_empty() {
            action.supports_legacy_discovery()
                && capabilities
                    .actions
                    .iter()
                    .any(|name| name == action.legacy_name())
        } else {
            capabilities
                .action_descriptors
                .get(action.id())
                .is_some_and(|descriptor| {
                    descriptor.version == action.version()
                        && descriptor.required_scope == action.required_scope()
                        && capabilities.scopes.contains(&action.required_scope())
                })
        })
    }

    async fn require_action(&self, action: Action) -> Result<()> {
        if !self.can_action(action).await? {
            *self.capabilities.lock().await = None;
            return Err(Error::ControlActionUnavailable { action });
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

    async fn send<T: DeserializeOwned>(&self, request: reqwest::RequestBuilder) -> Result<T> {
        let response = request.send().await.map_err(Error::transport)?;
        let status = response.status();
        {
            let mut cache = self.capabilities.lock().await;
            let changed = response
                .headers()
                .get("x-bitrouter-control-instance")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|instance| {
                    cache
                        .as_ref()
                        .and_then(|(_, report)| report.server_instance_id.as_deref())
                        .is_some_and(|cached| cached != instance)
                });
            if changed || matches!(status.as_u16(), 401 | 403 | 404) {
                *cache = None;
            }
        }
        if !status.is_success() {
            let headers = response.headers().clone();
            let (body, body_error) =
                match crate::transport::body(response, self.max_response_bytes).await {
                    Ok(bytes) => match serde_json::from_slice(&bytes) {
                        Ok(body) => (Some(body), None),
                        Err(_) => (
                            None,
                            Some(Error::Response("invalid control error envelope")),
                        ),
                    },
                    Err(error) => (None, Some(error)),
                };
            return Err(Error::Control(Box::new(ServiceFailure {
                status: status.as_u16(),
                headers,
                body,
                body_error,
            })));
        }
        let bytes = crate::transport::body(response, self.max_response_bytes).await?;
        serde_json::from_slice(&bytes)
            .map_err(|_| Error::Response("invalid remote control response"))
    }

    fn action_url(&self, path: &str) -> Result<Url> {
        self.endpoint
            .join(path)
            .map_err(|_| Error::Request("invalid control action path"))
    }
}

/// Normalize an origin or `/control/v1` endpoint using the client's transport rules.
/// Local profile selection remains the application's responsibility.
pub fn normalize_endpoint(endpoint: &str) -> Result<String> {
    parsed_endpoint(endpoint).map(|url| url.to_string())
}

fn parsed_endpoint(endpoint: &str) -> Result<Url> {
    let mut url =
        Url::parse(endpoint).map_err(|_| Error::Options("invalid control endpoint URL"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::Options(
            "remote control endpoint must not contain user information",
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(Error::Options(
            "remote control endpoint must not contain a query or fragment",
        ));
    }
    match url.scheme() {
        "https" => {}
        "http" if url_host_is_loopback(&url) => {}
        "http" => {
            return Err(Error::Options(
                "plain HTTP remote control is allowed only on loopback; use HTTPS or an SSH tunnel",
            ));
        }
        _ => return Err(Error::Options("unsupported remote control URL scheme")),
    }
    match url.path().trim_end_matches('/') {
        "" | "/control/v1" => url.set_path("/control/v1/"),
        _ => {
            return Err(Error::Options(
                "remote control endpoint path must be empty or /control/v1",
            ));
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
