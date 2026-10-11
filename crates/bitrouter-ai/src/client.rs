//! Selected-model HTTP invocation independent of routing, catalogs and servers.
//!
//! Calls use one explicit target and an optional registered auth mechanism.
//! Stateful auth can rebuild once after 401; no account or provider fallback is
//! performed. Irreversible refresh belongs to the mechanism's owned transaction.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures_core::Stream;
use tokio_util::sync::CancellationToken;

use crate::auth::{AuthAppliers, AuthOperation, normalize_auth_extension_error};
use crate::decisions::{DecisionRequest, DecisionResult};
use crate::diagnostics::DiagnosticRedactor;
use crate::error::{ModelError, Result};
use crate::protocol::decisions::{DecisionsCodec, DecisionsTransport};
use crate::protocol::{OutboundAdapter, OutboundDispatch, SseEvent, Transport};
use crate::target::ModelTarget;
use crate::types::{ApiProtocol, GenerateResult, ModelOperation, Prompt, StreamPart};

enum SelectedInput<'a> {
    Generation { prompt: &'a Prompt, stream: bool },
    Decisions(&'a DecisionRequest),
}

/// An owned stream of canonical model parts and terminal failures.
/// Dropping it drops the upstream response; cancellation stops pending I/O.
pub type ModelStream = Pin<Box<dyn Stream<Item = Result<StreamPart>> + Send>>;

/// Upstream HTTP client timeout configuration. v0 #394: the upstream client had
/// no timeouts, so a slow provider could hang a request forever.
///
/// `connect` / `read` / `pool_idle` / `tcp_keepalive` are set on the reqwest
/// client at build time. `read` is a **per-read** (idle) timeout — it resets
/// after every chunk, so it fires when an upstream sends no bytes for that long
/// *including mid-stream*, which is the effective stream-idle guard.
///
/// `total` is the optional overall wall-clock cap for the whole request/stream,
/// applied per-request via [`reqwest::RequestBuilder::timeout`]. It is `None` by
/// default: an overall cap would kill legitimately long agentic/reasoning
/// streams, so it is opt-in per deployment or per provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpTimeouts {
    /// TCP connect timeout.
    pub connect: Duration,
    /// Per-read (idle) timeout — resets after each chunk; fires mid-stream when
    /// the upstream goes silent for this long.
    pub read: Duration,
    /// How long an idle pooled connection is kept.
    pub pool_idle: Duration,
    /// TCP keepalive probe interval.
    pub tcp_keepalive: Duration,
    /// Optional overall wall-clock cap for the entire request/stream. `None` ⇒
    /// no cap (default). Opt-in; keep it generous for reasoning providers.
    pub total: Option<Duration>,
}

impl Default for HttpTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            read: Duration::from_secs(120),
            pool_idle: Duration::from_secs(90),
            tcp_keepalive: Duration::from_secs(60),
            total: None,
        }
    }
}

/// One reusable HTTP client and protocol registry for explicitly selected calls.
///
/// This client selects no accounts, reads no ambient credentials, performs no
/// discovery/login. Registered mechanisms may refresh the same selected account
/// and retry once on 401. Provider HTTP statuses and
/// retry hints remain domain facts for the caller's policy.
///
/// ```no_run
/// use bitrouter_ai::client::{HttpTimeouts, ModelClient};
/// use bitrouter_ai::target::ModelTarget;
/// use bitrouter_ai::types::{ApiProtocol, AuthScheme, Prompt};
/// use tokio_util::sync::CancellationToken;
///
/// # async fn selected_call(prompt: &Prompt, credential: String) -> bitrouter_ai::error::Result<()> {
/// let target = ModelTarget {
///     provider_name: "selected-provider".into(),
///     service_id: "selected-native-model".into(),
///     api_protocol: ApiProtocol::Responses,
///     api_base: "https://api.openai.com/v1".into(),
///     api_key: credential,
///     credential_priority: Default::default(),
///     account_label: None,
///     auth_scheme: AuthScheme::Bearer,
///     compatibility: Default::default(),
/// };
/// let client = ModelClient::new(HttpTimeouts::default())?;
/// let cancellation = CancellationToken::new();
/// let result = client.generate(&target, prompt, &cancellation).await?;
/// # let _ = result;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct ModelClient {
    client: reqwest::Client,
    timeouts: HttpTimeouts,
    dispatch: Arc<OutboundDispatch>,
    auth_appliers: AuthAppliers,
}

impl ModelClient {
    /// Build a client with the built-in generation and Decisions protocols.
    pub fn new(timeouts: HttpTimeouts) -> Result<Self> {
        Self::with_dispatch(timeouts, Arc::new(OutboundDispatch::builtin()))
    }

    /// Build a client with an explicitly supplied protocol registry.
    pub fn with_dispatch(timeouts: HttpTimeouts, dispatch: Arc<OutboundDispatch>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(timeouts.connect)
            .read_timeout(timeouts.read)
            .pool_idle_timeout(timeouts.pool_idle)
            .tcp_keepalive(timeouts.tcp_keepalive)
            .build()
            .map_err(|error| ModelError::Configuration {
                message: format!("building HTTP client: {error}"),
            })?;
        Ok(Self {
            client,
            timeouts,
            dispatch,
            auth_appliers: AuthAppliers::new(),
        })
    }

    /// Use a caller-configured HTTP transport (for example, disabled redirects
    /// and retries for a durable managed attempt). Request deadlines still use
    /// `timeouts`; connection-level settings belong to the supplied client.
    pub fn with_http_client(
        timeouts: HttpTimeouts,
        dispatch: Arc<OutboundDispatch>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            client,
            timeouts,
            dispatch,
            auth_appliers: AuthAppliers::new(),
        }
    }

    /// Register explicit authentication mechanisms; no credentials are loaded here.
    pub fn with_auth_appliers(mut self, auth_appliers: AuthAppliers) -> Self {
        self.auth_appliers = auth_appliers;
        self
    }

    /// Render a fresh target-specific request without changing source history.
    /// The caller can apply provider body shaping before building/authenticating.
    pub fn render_request(
        &self,
        target: &ModelTarget,
        prompt: &Prompt,
        stream: bool,
    ) -> Result<serde_json::Value> {
        self.render_request_with_report(target, prompt, stream)
            .map(|(body, _)| body)
    }

    /// Prepare a fresh selected-target body and its bounded conversion assessment.
    /// This performs no authentication or I/O and is not a complete fidelity proof.
    pub fn render_request_with_report(
        &self,
        target: &ModelTarget,
        prompt: &Prompt,
        stream: bool,
    ) -> Result<(serde_json::Value, crate::conversion::ConversionReport)> {
        if let Some(message) =
            crate::providers::retired::protocol_message(target.api_protocol.as_str())
                .or_else(|| crate::providers::retired::provider_message(&target.provider_name))
        {
            return Err(ModelError::configuration(message));
        }
        if target.api_protocol.operation() != ModelOperation::Generation {
            return Err(ModelError::invalid_request(
                "generation requires a generation protocol",
            ));
        }
        let (adapter, _) = self.dispatch.lookup(&target.api_protocol).ok_or_else(|| {
            ModelError::Configuration {
                message: format!(
                    "no outbound dispatch registered for protocol '{}' (target provider '{}')",
                    target.api_protocol, target.provider_name
                ),
            }
        })?;
        let mut report = adapter.admission(prompt);
        report
            .issues
            .extend(crate::providers::google_chat::admission(prompt, target).issues);
        report.require_admitted()?;
        let mut projection = prompt.clone();
        projection.model = target.service_id.clone();
        projection.stream = stream;
        let body = adapter.render_request_for_target(&projection, target)?;
        Ok((body, report))
    }

    /// Render one native decision projection without I/O or changing the source.
    pub fn render_decision_request(
        &self,
        target: &ModelTarget,
        request: &DecisionRequest,
    ) -> Result<serde_json::Value> {
        if let Some(message) =
            crate::providers::retired::protocol_message(target.api_protocol.as_str())
                .or_else(|| crate::providers::retired::provider_message(&target.provider_name))
        {
            return Err(ModelError::configuration(message));
        }
        if target.api_protocol != ApiProtocol::Decisions {
            return Err(ModelError::invalid_request(
                "Decisions requires a Decisions protocol",
            ));
        }
        let mut projection = request.clone();
        projection.model.clone_from(&target.service_id);
        DecisionsCodec::render_request(&projection)
    }

    /// Invoke one selected native Decisions target with shared authentication/I/O.
    pub async fn decide(
        &self,
        target: &ModelTarget,
        request: &DecisionRequest,
        cancellation: &CancellationToken,
    ) -> Result<DecisionResult> {
        self.render_decision_request(target, request)?;
        let (response, redactor) = self
            .send_selected(target, &SelectedInput::Decisions(request), cancellation)
            .await?;
        let status = response.status();
        let retry_after = parse_retry_after(response.headers().get(reqwest::header::RETRY_AFTER));
        let text = Self::read_body(response, cancellation)
            .await
            .map_err(|error| redactor.scrub_error(error))?;
        if !status.is_success() {
            return Err(redactor.scrub_error(ModelError::HttpResponse {
                status: status.as_u16(),
                body: text,
                retry_after,
            }));
        }
        Self::parse_decision_response(&text, request).map_err(|error| redactor.scrub_error(error))
    }

    /// Decode a completed native response, retaining usable usage on failure.
    pub fn parse_decision_response(
        text: &str,
        request: &DecisionRequest,
    ) -> Result<DecisionResult> {
        let body = serde_json::from_str(text).map_err(|_| ModelError::DecisionResponse {
            failure: crate::decisions::DecisionResponseFailure {
                message: "invalid Decisions response JSON".into(),
                usage: None,
            },
        })?;
        DecisionsCodec::parse_response(body, request)
    }

    /// Build one JSON POST with the selected overall request deadline.
    /// Authentication/header policy can be applied to the final request by the caller.
    pub fn build_request(&self, url: &str, body: &serde_json::Value) -> Result<reqwest::Request> {
        let mut builder = self.client.post(url).json(body);
        if let Some(total) = self.timeouts.total {
            builder = builder.timeout(total);
        }
        builder.build().map_err(|error| ModelError::Configuration {
            message: format!("building request: {error}"),
        })
    }

    /// Send an already authenticated request without retrying or classifying HTTP status.
    pub async fn send(
        &self,
        request: reqwest::Request,
        cancellation: &CancellationToken,
    ) -> Result<reqwest::Response> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(ModelError::Cancelled),
            response = self.client.execute(request) => response.map_err(|error| io_error("sending model request", error)),
        }
    }

    /// Read a response body, retaining read/total timeout and caller cancellation.
    pub async fn read_body(
        response: reqwest::Response,
        cancellation: &CancellationToken,
    ) -> Result<String> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(ModelError::Cancelled),
            text = response.text() => text.map_err(|error| io_error("reading upstream body", error)),
        }
    }

    /// Decode a successful body and enforce the Responses terminal/id contract.
    pub fn parse_response(
        adapter: &dyn OutboundAdapter,
        protocol: &ApiProtocol,
        text: &str,
    ) -> Result<GenerateResult> {
        let json: serde_json::Value =
            serde_json::from_str(text).map_err(|error| ModelError::Decode {
                message: format!("upstream returned non-JSON body: {error}"),
            })?;
        if *protocol == ApiProtocol::Responses {
            validate_responses_terminal(&json)?;
        }
        let usage = if *protocol == ApiProtocol::ChatCompletions {
            json.get("usage")
                .and_then(crate::protocol::chat_completions::parse_usage)
                .map(Box::new)
        } else {
            None
        };
        adapter
            .parse_response(json)
            .map_err(|error| match response_error(error) {
                ModelError::InvalidResponse { message, .. } => {
                    ModelError::InvalidResponse { message, usage }
                }
                error => error,
            })
    }

    /// Decode a successful HTTP stream. Clean EOF requires a model terminal part.
    /// Parts already emitted, including usage, are retained before a late error.
    pub fn decode_stream(
        adapter: Arc<dyn OutboundAdapter>,
        response: reqwest::Response,
        cancellation: CancellationToken,
    ) -> ModelStream {
        let mut decoder = adapter.stream_decoder();
        let stream = async_stream::stream! {
            use eventsource_stream::Eventsource;
            let mut events = response.bytes_stream().eventsource();
            let mut terminal = false;
            loop {
                let event = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        yield Err(ModelError::Cancelled);
                        return;
                    }
                    event = events.next() => event,
                };
                let Some(event) = event else { break; };
                match event {
                    Ok(event) => {
                        let event = SseEvent {
                            event: if event.event.is_empty() { None } else { Some(event.event) },
                            data: event.data,
                        };
                        match decoder.decode(&event).map_err(response_error) {
                            Ok(parts) => for part in parts {
                                terminal |= part.is_terminal();
                                yield Ok(part);
                            },
                            Err(mut error) => {
                                if adapter.protocol() == ApiProtocol::ChatCompletions
                                    && let Ok(chunk) = serde_json::from_str::<serde_json::Value>(&event.data)
                                    && let Some(reported) = chunk.get("usage").and_then(crate::protocol::chat_completions::parse_usage) {
                                    yield Ok(StreamPart::Usage { usage: reported.clone() });
                                    if let ModelError::InvalidResponse { usage, .. } = &mut error { *usage = Some(Box::new(reported)); }
                                }
                                yield Err(error); return;
                            }
                        }
                    }
                    Err(eventsource_stream::EventStreamError::Transport(error)) => {
                        yield Err(io_error("upstream stream error", error));
                        return;
                    }
                    Err(error) => {
                        yield Err(ModelError::Decode { message: format!("upstream stream error: {error}") });
                        return;
                    }
                }
            }
            match decoder.finish().map_err(response_error) {
                Ok(parts) => for part in parts {
                    terminal |= part.is_terminal();
                    yield Ok(part);
                },
                Err(error) => { yield Err(error); return; }
            }
            if !terminal {
                yield Err(ModelError::InvalidResponse { message: "upstream stream ended without a model terminal part".into(), usage: None });
            }
        };
        Box::pin(stream)
    }

    /// Call one explicit target without catalog/config/account resolution.
    /// Token cancellation ends pending I/O; custom authentication must finish
    /// first. Dropping this call future can drop custom authentication.
    pub async fn generate(
        &self,
        target: &ModelTarget,
        prompt: &Prompt,
        cancellation: &CancellationToken,
    ) -> Result<GenerateResult> {
        if crate::providers::codex::requires_streaming(target) {
            return crate::stream::collect::collect_generate(
                self.stream(target, prompt, cancellation).await?,
            )
            .await;
        }
        let (response, redactor) = self
            .send_selected(
                target,
                &SelectedInput::Generation {
                    prompt,
                    stream: false,
                },
                cancellation,
            )
            .await?;
        let result = async {
            let status = response.status();
            let retry_after =
                parse_retry_after(response.headers().get(reqwest::header::RETRY_AFTER));
            let text = Self::read_body(response, cancellation).await?;
            if !status.is_success() {
                return Err(ModelError::HttpResponse {
                    status: status.as_u16(),
                    body: text,
                    retry_after,
                });
            }
            let (adapter, _) = self.dispatch.lookup(&target.api_protocol).ok_or_else(|| {
                ModelError::Configuration {
                    message: "selected protocol disappeared".into(),
                }
            })?;
            crate::providers::google_chat::bind_result(
                Self::parse_response(adapter.as_ref(), &target.api_protocol, &text)?,
                target,
            )
        }
        .await;
        result.map_err(|error| redactor.scrub_error(error))
    }

    /// Start one explicit streaming target. Dropping the returned stream stops reads.
    /// Token cancellation ends pending I/O; custom authentication must finish
    /// first. Dropping this call future can drop custom authentication.
    pub async fn stream(
        &self,
        target: &ModelTarget,
        prompt: &Prompt,
        cancellation: &CancellationToken,
    ) -> Result<ModelStream> {
        let (response, redactor) = self
            .send_selected(
                target,
                &SelectedInput::Generation {
                    prompt,
                    stream: true,
                },
                cancellation,
            )
            .await?;
        let status = response.status();
        if !status.is_success() {
            let retry_after =
                parse_retry_after(response.headers().get(reqwest::header::RETRY_AFTER));
            let body = Self::read_body(response, cancellation)
                .await
                .map_err(|error| redactor.scrub_error(error))?;
            return Err(redactor.scrub_error(ModelError::HttpResponse {
                status: status.as_u16(),
                body,
                retry_after,
            }));
        }
        let (adapter, _) = self.dispatch.lookup(&target.api_protocol).ok_or_else(|| {
            ModelError::Configuration {
                message: "selected protocol disappeared".into(),
            }
        })?;
        Ok(Box::pin(
            crate::providers::google_chat::bind_stream(
                Self::decode_stream(Arc::clone(adapter), response, cancellation.clone()),
                target.clone(),
            )
            .map(move |part| part.map_err(|error| redactor.scrub_error(error))),
        ))
    }

    async fn send_selected(
        &self,
        target: &ModelTarget,
        input: &SelectedInput<'_>,
        cancellation: &CancellationToken,
    ) -> Result<(reqwest::Response, DiagnosticRedactor)> {
        let mut redactor = DiagnosticRedactor::default();
        redactor.add_replacement(target.api_key.clone(), "[redacted credential]".into());
        if let SelectedInput::Generation { prompt, .. } = input {
            redactor.capture_prompt_continuity(prompt);
        }
        let mut refreshed = false;
        loop {
            let request = self
                .authenticated_request(target, input, cancellation, &mut redactor)
                .await
                .map_err(|error| redactor.scrub_error(error))?;
            let rejected = request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .cloned();
            let response = self
                .send(request, cancellation)
                .await
                .map_err(|error| redactor.scrub_error(error))?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
                if cancellation.is_cancelled() {
                    return Err(ModelError::Cancelled);
                }
                if let Some(applier) = self.auth_appliers.lookup(&target.provider_name) {
                    let retry = applier
                        .refresh_after_unauthorized(target, rejected.as_ref())
                        .await
                        .map_err(|error| {
                            normalize_auth_extension_error(error, AuthOperation::Refresh)
                        })?;
                    if retry {
                        refreshed = true;
                        continue;
                    }
                }
            }
            return Ok((response, redactor));
        }
    }

    async fn authenticated_request(
        &self,
        target: &ModelTarget,
        input: &SelectedInput<'_>,
        cancellation: &CancellationToken,
        redactor: &mut DiagnosticRedactor,
    ) -> Result<reqwest::Request> {
        if cancellation.is_cancelled() {
            return Err(ModelError::Cancelled);
        }
        let applier = self.auth_appliers.lookup(&target.provider_name);
        if applier.is_none()
            && target.api_key.is_empty()
            && !matches!(target.api_protocol, ApiProtocol::Custom(_))
        {
            return Err(ModelError::invalid_credential(
                "missing effective model credential; supply a selected credential explicitly",
            ));
        }
        let (mut body, transport, stream): (serde_json::Value, &dyn Transport, bool) = match input {
            SelectedInput::Generation { prompt, stream } => {
                let body = self.render_request(target, prompt, *stream)?;
                let (_, transport) = self
                    .dispatch
                    .lookup(&target.api_protocol)
                    .ok_or_else(|| ModelError::configuration("selected protocol disappeared"))?;
                (body, transport.as_ref(), *stream)
            }
            SelectedInput::Decisions(request) => (
                self.render_decision_request(target, request)?,
                &DecisionsTransport,
                false,
            ),
        };
        if let Some(applier) = applier {
            applier
                .prepare_body(&mut body, target)
                .await
                .map_err(|error| {
                    normalize_auth_extension_error(error, AuthOperation::BodyPreparation)
                })?;
        }
        if matches!(input, SelectedInput::Decisions(_)) {
            DecisionsCodec::parse_request(body.clone())?;
        }
        let request = self.build_request(&transport.endpoint_url(target, stream), &body)?;
        let request = if let Some(applier) = applier {
            applier
                .apply_with_authority(request, target)
                .await
                .map_err(|error| {
                    normalize_auth_extension_error(error, AuthOperation::RequestAuthentication)
                })?
                .into_request()
        } else {
            transport.authorise(request, target).await?
        };
        if cancellation.is_cancelled() {
            return Err(ModelError::Cancelled);
        }
        redactor.capture_request_credentials(&request, &target.api_key);
        crate::providers::google_chat::validate_authenticated_request(&request, target)?;
        Ok(request)
    }
}

fn io_error(context: &str, error: reqwest::Error) -> ModelError {
    if error.is_timeout() {
        ModelError::Timeout
    } else {
        ModelError::Transport {
            message: format!("{context}: {error}"),
        }
    }
}

fn response_error(error: ModelError) -> ModelError {
    match error {
        error @ (ModelError::Provider { .. }
        | ModelError::PolicyViolation { .. }
        | ModelError::InvalidResponse { .. }) => error,
        error => ModelError::InvalidResponse {
            message: error.to_string(),
            usage: None,
        },
    }
}

fn validate_responses_terminal(json: &serde_json::Value) -> Result<()> {
    let status = json.get("status").and_then(serde_json::Value::as_str);
    if !matches!(status, Some("completed" | "incomplete")) {
        return Err(ModelError::InvalidResponse {
            message: format!(
                "Responses response has non-success terminal status '{}'",
                status.unwrap_or("<missing>")
            ),
            usage: None,
        });
    }
    if json
        .get("id")
        .and_then(serde_json::Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(ModelError::InvalidResponse {
            message: "Responses response missing non-empty 'id'".into(),
            usage: None,
        });
    }
    Ok(())
}

/// Parse provider Retry-After as delay-seconds or an HTTP date, without retrying.
pub fn parse_retry_after(value: Option<&reqwest::header::HeaderValue>) -> Option<u64> {
    let value = value?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds);
    }
    let deadline = httpdate::parse_http_date(value).ok()?;
    match deadline.duration_since(std::time::SystemTime::now()) {
        Ok(delay) => Some(delay.as_secs() + u64::from(delay.subsec_nanos() > 0)),
        Err(_) => Some(0),
    }
}
