//! HTTP request construction, protocol decoding and model-catalog probes.
//!
//! The gateway executor owns authenticated invocation, retries and settlement.

use crate::gateway::model::conversion::ReportAdmission;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::gateway::model::error::{ModelError, Result};
use crate::gateway::model::protocol::decisions::DecisionsCodec;
use crate::gateway::model::protocol::{OutboundAdapter, OutboundDispatch};
use crate::gateway::model::target::ModelTarget;
use bitrouter_ai::decisions::{DecisionRequest, DecisionResult};
use bitrouter_ai::types::{ApiProtocol, GenerateResult, ModelOperation, Prompt};

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

/// Transport and codec primitives used by the gateway executor and catalog probe.
#[derive(Clone)]
pub struct ModelClient {
    client: reqwest::Client,
    timeouts: HttpTimeouts,
    dispatch: Arc<OutboundDispatch>,
}

impl ModelClient {
    pub(crate) fn for_model_list_probe(timeouts: HttpTimeouts) -> Result<Self> {
        Self::with_redirect_policy(
            timeouts,
            Arc::new(OutboundDispatch::builtin()),
            reqwest::redirect::Policy::none(),
        )
    }

    fn with_redirect_policy(
        timeouts: HttpTimeouts,
        dispatch: Arc<OutboundDispatch>,
        redirects: reqwest::redirect::Policy,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(timeouts.connect)
            .read_timeout(timeouts.read)
            .pool_idle_timeout(timeouts.pool_idle)
            .tcp_keepalive(timeouts.tcp_keepalive)
            .redirect(redirects)
            .build()
            .map_err(|error| ModelError::Configuration {
                message: format!("building HTTP client: {error}"),
            })?;
        Ok(Self {
            client,
            timeouts,
            dispatch,
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
        }
    }

    /// Prepare a fresh selected-target body and its bounded conversion assessment.
    /// This performs no authentication or I/O and is not a complete fidelity proof.
    pub fn render_request_with_report(
        &self,
        target: &ModelTarget,
        prompt: &Prompt,
        stream: bool,
    ) -> Result<(
        serde_json::Value,
        crate::gateway::conversion_report::ConversionReport,
    )> {
        if let Some(message) =
            crate::gateway::providers::retired::protocol_message(target.api_protocol.as_str())
                .or_else(|| {
                    crate::gateway::providers::retired::provider_message(&target.provider_name)
                })
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
            .extend(crate::gateway::providers::google_chat::admission(prompt, target).issues);
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
            crate::gateway::providers::retired::protocol_message(target.api_protocol.as_str())
                .or_else(|| {
                    crate::gateway::providers::retired::provider_message(&target.provider_name)
                })
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

    /// Decode a completed native response, retaining usable usage on failure.
    pub fn parse_decision_response(
        text: &str,
        request: &DecisionRequest,
    ) -> Result<DecisionResult> {
        let body = serde_json::from_str(text).map_err(|_| ModelError::DecisionResponse {
            failure: bitrouter_ai::decisions::DecisionResponseFailure {
                message: "invalid Decisions response JSON".into(),
                usage: None,
            },
        })?;
        DecisionsCodec::parse_response(body, request)
    }

    /// GET this protocol's model catalog using the selected transport and timeouts.
    pub async fn list_models(
        &self,
        target: &ModelTarget,
        headers: &http::HeaderMap,
        cancellation: &CancellationToken,
    ) -> Result<Vec<String>> {
        let Some((_, transport)) = self.dispatch.lookup(&target.api_protocol) else {
            return Err(ModelError::configuration(format!(
                "no outbound dispatch registered for protocol '{}'",
                target.api_protocol
            )));
        };
        let Some(url) = transport.model_catalog_url(target) else {
            return Err(ModelError::configuration(format!(
                "protocol '{}' does not publish a model list",
                target.api_protocol
            )));
        };
        let mut request = self.build_get(&url)?;
        request = transport.authorise(request, target).await?;
        for (name, value) in headers.iter() {
            request.headers_mut().insert(name, value.clone());
        }
        let response = self.send(request, cancellation).await?;
        let status = response.status();
        let text = Self::read_body(response, cancellation).await?;
        if !status.is_success() {
            return Err(ModelError::HttpResponse {
                status: status.as_u16(),
                body: text,
                retry_after: None,
            });
        }
        let json: serde_json::Value =
            serde_json::from_str(&text).map_err(|_| ModelError::Decode {
                message: "endpoint did not answer with a model list".into(),
            })?;
        transport
            .parse_model_catalog(&json)
            .ok_or_else(|| ModelError::Decode {
                message: "endpoint did not answer with a model list".into(),
            })
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

    fn build_get(&self, url: &str) -> Result<reqwest::Request> {
        let mut builder = self.client.get(url);
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
        Self::send_with(&self.client, request, cancellation).await
    }

    pub(crate) async fn send_with(
        client: &reqwest::Client,
        request: reqwest::Request,
        cancellation: &CancellationToken,
    ) -> Result<reqwest::Response> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(ModelError::Cancelled),
            response = client.execute(request) => response.map_err(|error| io_error("sending model request", error)),
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
                .and_then(crate::gateway::model::protocol::chat_completions::parse_usage)
                .map(Box::new)
        } else {
            None
        };
        adapter
            .parse_response(json)
            .map_err(|error| match response_error(error) {
                ModelError::InvalidResponse {
                    message,
                    usage: observed,
                } => ModelError::InvalidResponse {
                    message,
                    usage: observed.or(usage),
                },
                error => error,
            })
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
