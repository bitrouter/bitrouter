//! The `Executor` — the component that turns a resolved `RoutingTarget` plus a
//! `Prompt` into an upstream call. Ships the trait, a `MockExecutor` for tests,
//! and `HttpExecutor` — the real protocol-aware HTTP executor.

use std::pin::Pin;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use futures_core::Stream;

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::{BitrouterError, Result};
use crate::language_model::auth::AuthAppliers;
use crate::language_model::context::PipelineContext;
use crate::language_model::protocol::{OutboundAdapter, OutboundDispatch, SseEvent};
use crate::language_model::types::{
    ApiProtocol, ExecutionResult, GenerateResult, Prompt, RoutingTarget, StreamPart,
};

/// A boxed stream of canonical stream parts.
pub type StreamPartStream = Pin<Box<dyn Stream<Item = Result<StreamPart>> + Send>>;

/// Performs the actual upstream call for one routing target.
///
/// `ctx` is the live [`PipelineContext`]; the executor reads any pending
/// outbound headers via [`PipelineContext::take_outbound_trace_headers`] to
/// propagate W3C trace context (`traceparent` / `tracestate`) into the
/// upstream call. Custom executors that don't need propagation can ignore it.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Execute a non-streaming request against `target`.
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> Result<ExecutionResult>;

    /// Start a streaming request against `target`.
    async fn execute_stream(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> Result<StreamPartStream>;
}

/// One canned upstream response for `MockExecutor`.
pub enum MockResponse {
    /// A successful non-streaming result.
    Generate(GenerateResult),
    /// A successful streaming result (the part list, each emitted in order).
    Stream(Vec<StreamPart>),
    /// An error (drives fallback testing).
    Error(BitrouterError),
}

/// A scriptable executor for tests. Each call pops the next scripted response
/// (keyed by provider name when scripted per-provider, else from a flat queue).
pub struct MockExecutor {
    queue: Mutex<Vec<MockResponse>>,
}

impl MockExecutor {
    /// Build an executor that will serve `responses` in order.
    pub fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            // reversed so `pop()` serves in declared order
            queue: Mutex::new(responses.into_iter().rev().collect()),
        }
    }

    /// Build an executor that always returns one successful text result.
    pub fn always_text(text: impl Into<String>) -> Self {
        use crate::language_model::types::{Content, FinishReason, Usage};
        let result = GenerateResult {
            content: vec![Content::Text {
                text: text.into(),
                provider_metadata: Default::default(),
            }],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                ..Default::default()
            }),
            finish_reason: Some(FinishReason::Stop),
            response_id: None,
            stop_details: None,
            provider_metadata: Default::default(),
        };
        Self::new(vec![MockResponse::Generate(result)])
    }

    fn next(&self) -> Result<MockResponse> {
        self.queue
            .lock()
            .expect("mock executor lock poisoned")
            .pop()
            .ok_or_else(|| BitrouterError::internal("MockExecutor: no scripted response left"))
    }
}

#[async_trait]
impl Executor for MockExecutor {
    async fn execute(
        &self,
        target: &RoutingTarget,
        _prompt: &Prompt,
        _ctx: &PipelineContext,
    ) -> Result<ExecutionResult> {
        match self.next()? {
            MockResponse::Generate(result) => Ok(ExecutionResult {
                provider_id: target.provider_name.clone(),
                model_id: target.service_id.clone(),
                account_label: target.account_label.clone(),
                result,
                latency_ms: 1,
                generation_time_ms: 1,
                server_tool_calls: Vec::new(),
            }),
            MockResponse::Stream(_) => Err(BitrouterError::internal(
                "MockExecutor: scripted a stream response for a non-streaming call",
            )),
            MockResponse::Error(e) => Err(e),
        }
    }

    async fn execute_stream(
        &self,
        _target: &RoutingTarget,
        _prompt: &Prompt,
        _ctx: &PipelineContext,
    ) -> Result<StreamPartStream> {
        match self.next()? {
            MockResponse::Stream(parts) => {
                let stream = futures::stream::iter(parts.into_iter().map(Ok));
                Ok(Box::pin(stream))
            }
            MockResponse::Generate(_) => Err(BitrouterError::internal(
                "MockExecutor: scripted a non-streaming response for a streaming call",
            )),
            MockResponse::Error(e) => Err(e),
        }
    }
}

// ===== real HTTP executor =====

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

/// The real protocol-aware HTTP executor. For each routing target it picks the
/// target's [`ProtocolAdapter`], renders the canonical prompt into that wire
/// format, performs the upstream call, and parses the response back into the
/// canonical representation.
/// Cap an upstream-supplied error message so a chatty provider that echoes
/// the request body, an API key, or a stack trace doesn't surface through
/// the client. ~1 KiB of char data is plenty for diagnostics. Truncated
/// to a UTF-8 char boundary so we never panic on a multi-byte slice.
fn truncate_upstream_message(text: &str) -> String {
    const MAX_CHARS: usize = 1024;
    let truncated: String = text.chars().take(MAX_CHARS).collect();
    if truncated.chars().count() < text.chars().count() {
        format!("{truncated}… [truncated]")
    } else {
        truncated
    }
}

/// Turn a non-2xx upstream response into the right [`BitrouterError`].
///
/// Most non-2xx maps to [`BitrouterError::Upstream`] carrying the
/// status. The exception: credit / balance exhaustion. Some gateways
/// signal it cleanly with `402`, but others (e.g. opencode) return a
/// `401`/`403` with a `CreditsError` / "insufficient balance" body —
/// which would otherwise be misread as an auth failure. Recognise that
/// family and map it to [`BitrouterError::UpstreamPaymentRequired`] so the
/// fallback policy drops to the next account / provider instead of
/// failing the request outright.
fn classify_upstream_error(status: u16, body: &str, retry_after: Option<u64>) -> BitrouterError {
    if status == 429 {
        return BitrouterError::UpstreamRateLimited { retry_after };
    }
    if matches!(status, 401..=403) && looks_like_credit_exhaustion(body) {
        return BitrouterError::UpstreamPaymentRequired;
    }
    BitrouterError::Upstream {
        status,
        message: truncate_upstream_message(body),
    }
}

/// Parse the standard `Retry-After` response field into a delay in seconds.
/// Both delay-seconds and HTTP-date are defined by RFC 9110 section 10.2.3:
/// <https://www.rfc-editor.org/rfc/rfc9110#section-10.2.3>.
fn parse_retry_after(value: Option<&reqwest::header::HeaderValue>) -> Option<u64> {
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

fn parse_upstream_success(
    adapter: &dyn OutboundAdapter,
    json: serde_json::Value,
) -> Result<GenerateResult> {
    adapter
        .parse_response(json)
        .map_err(|error| BitrouterError::UpstreamInvalidResponse {
            message: error.to_string(),
        })
}

/// Classify a transport error that surfaces from the SSE decode loop *after*
/// the stream is open. A read-timeout firing mid-stream must map to
/// [`BitrouterError::UpstreamTimeout`] (504) — the same as a pre-stream
/// timeout — rather than a generic 502, so status codes and metrics stay
/// truthful once streaming has started. Non-timeout errors (parse / decode /
/// other transport) keep the 502 mapping and carry the message.
fn stream_transport_error(is_timeout: bool, display: impl std::fmt::Display) -> BitrouterError {
    if is_timeout {
        BitrouterError::UpstreamTimeout
    } else {
        BitrouterError::Upstream {
            status: 502,
            message: format!("upstream stream error: {display}"),
        }
    }
}

fn upstream_body_error(context: &'static str, error: reqwest::Error) -> BitrouterError {
    if error.is_timeout() {
        BitrouterError::UpstreamTimeout
    } else {
        BitrouterError::Upstream {
            status: 502,
            message: format!("{context}: {error}"),
        }
    }
}

/// Heuristic: does this upstream error body describe a depleted
/// credit / balance? Matches the stable phrase family rather than any
/// one provider's exact wording — string matching is unavoidable here
/// because the signal is not in the HTTP status.
fn looks_like_credit_exhaustion(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    b.contains("creditserror")
        || b.contains("insufficient balance")
        || b.contains("insufficient credit")
        || b.contains("insufficient funds")
        || b.contains("out of credit")
}

/// The default upstream [`Executor`] — dispatches a canonical
/// [`Prompt`] to the wire protocol of the resolved [`RoutingTarget`] over
/// HTTP and parses the response back into a canonical [`GenerateResult`] /
/// stream of [`StreamPart`]s.
///
/// Build with [`HttpExecutor::with_defaults`] for sensible timeout defaults
/// or [`HttpExecutor::new`] with a custom [`HttpTimeouts`]. Use
/// [`with_provider_timeouts`](Self::with_provider_timeouts) to attach
/// per-provider overrides.
pub struct HttpExecutor {
    clients: RwLock<HttpClientSet>,
    dispatch: OutboundDispatch,
    auth_appliers: AuthAppliers,
}

struct HttpClientSet {
    /// Client used for any provider without a per-provider override, plus its
    /// timeouts (for the per-request `total` cap, which is not a client
    /// setting).
    default_client: reqwest::Client,
    default_timeouts: HttpTimeouts,
    /// Per-provider clients keyed by `provider_name`, each paired with the
    /// resolved timeouts it was built from. Built once at construction; empty
    /// in the common single-timeout deployment.
    provider_clients: HashMap<String, (HttpTimeouts, reqwest::Client)>,
}

/// Build a reqwest client from the connection-level timeout knobs. `total` is
/// deliberately not applied here — it is a per-request deadline set via
/// [`reqwest::RequestBuilder::timeout`], not a client-builder setting.
fn build_http_client(timeouts: &HttpTimeouts) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(timeouts.connect)
        .read_timeout(timeouts.read)
        .pool_idle_timeout(timeouts.pool_idle)
        .tcp_keepalive(timeouts.tcp_keepalive)
        .build()
        .map_err(|e| BitrouterError::internal(format!("building HTTP client: {e}")))
}

fn build_http_client_set(
    default_timeouts: HttpTimeouts,
    per_provider: HashMap<String, HttpTimeouts>,
) -> Result<HttpClientSet> {
    let default_client = build_http_client(&default_timeouts)?;
    let mut provider_clients = HashMap::new();
    for (name, timeouts) in per_provider {
        if timeouts == default_timeouts {
            continue;
        }
        let client = build_http_client(&timeouts)?;
        provider_clients.insert(name, (timeouts, client));
    }
    Ok(HttpClientSet {
        default_client,
        default_timeouts,
        provider_clients,
    })
}

impl HttpExecutor {
    /// Build an executor with the given upstream timeout configuration and the
    /// default [`OutboundDispatch::builtin`] registry. Use
    /// [`with_dispatch`](Self::with_dispatch) instead when you want to
    /// register a custom provider (e.g. AWS Bedrock).
    pub fn new(timeouts: HttpTimeouts) -> Result<Self> {
        Self::with_dispatch(timeouts, OutboundDispatch::builtin())
    }

    /// Build an executor with the given upstream timeout configuration and a
    /// custom outbound-dispatch registry. The dispatch table is consulted
    /// once per request (via [`RoutingTarget::api_protocol`]) to find the
    /// adapter that renders the request body + parses the response and the
    /// transport that builds the URL + applies auth.
    pub fn with_dispatch(timeouts: HttpTimeouts, dispatch: OutboundDispatch) -> Result<Self> {
        Self::with_dispatch_and_auth(timeouts, dispatch, AuthAppliers::new())
    }

    /// Build an executor with custom timeouts, dispatch, **and** a registry
    /// of per-provider [`AuthApplier`](crate::language_model::AuthApplier)s.
    /// When a target's `provider_name` matches a registered applier, that
    /// applier replaces `Transport::authorise` for the request (OAuth, SigV4,
    /// any custom credential flow).
    pub fn with_dispatch_and_auth(
        timeouts: HttpTimeouts,
        dispatch: OutboundDispatch,
        auth_appliers: AuthAppliers,
    ) -> Result<Self> {
        Self::with_provider_timeouts(timeouts, HashMap::new(), dispatch, auth_appliers)
    }

    /// Build an executor with a global default [`HttpTimeouts`] plus a set of
    /// per-provider overrides keyed by `provider_name`. Each override gets its
    /// own reqwest client (connect/read/pool/keepalive are client-build-time
    /// settings, so a differing tuple needs a distinct client); an override
    /// equal to the default is skipped. Providers absent from the map use the
    /// default client. This is how the app honours the `upstream.timeouts`
    /// block and per-provider `timeouts:` overrides.
    pub fn with_provider_timeouts(
        default_timeouts: HttpTimeouts,
        per_provider: HashMap<String, HttpTimeouts>,
        dispatch: OutboundDispatch,
        auth_appliers: AuthAppliers,
    ) -> Result<Self> {
        let clients = build_http_client_set(default_timeouts, per_provider)?;
        Ok(Self {
            clients: RwLock::new(clients),
            dispatch,
            auth_appliers,
        })
    }

    /// Replace the global/per-provider timeout clients in-place. Existing
    /// in-flight requests keep the cloned client they already selected; new
    /// requests use the freshly built set.
    pub fn reload_provider_timeouts(
        &self,
        default_timeouts: HttpTimeouts,
        per_provider: HashMap<String, HttpTimeouts>,
    ) -> Result<()> {
        let clients = build_http_client_set(default_timeouts, per_provider)?;
        let mut guard = match self.clients.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = clients;
        Ok(())
    }

    /// Pick the client + timeouts for `target`: a per-provider override when one
    /// is registered for its `provider_name`, else the default pair.
    fn client_for(&self, target: &RoutingTarget) -> (reqwest::Client, HttpTimeouts) {
        let guard = match self.clients.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match guard.provider_clients.get(&target.provider_name) {
            Some((timeouts, client)) => (client.clone(), timeouts.clone()),
            None => (guard.default_client.clone(), guard.default_timeouts.clone()),
        }
    }

    /// Build an executor with default timeouts and the built-in dispatch.
    pub fn with_defaults() -> Result<Self> {
        Self::new(HttpTimeouts::default())
    }

    /// Apply the per-provider [`AuthApplier`](crate::language_model::AuthApplier)
    /// if one is registered for `target.provider_name`, else fall through to
    /// `Transport::authorise`. Shared by both `execute` and `execute_stream`.
    async fn apply_auth(
        &self,
        request: reqwest::Request,
        target: &RoutingTarget,
        transport: &Arc<dyn crate::language_model::protocol::Transport>,
    ) -> Result<reqwest::Request> {
        if let Some(applier) = self.auth_appliers.lookup(&target.provider_name) {
            applier.apply(request, target).await
        } else {
            transport.authorise(request, target).await
        }
    }

    /// Run the per-provider [`AuthApplier::prepare_body`] hook on the freshly
    /// rendered request body when an applier is registered for the target's
    /// provider. No-op otherwise. Shared by `execute` and `execute_stream` so
    /// subscription-OAuth body shaping happens identically on both paths.
    async fn shape_request_body(
        &self,
        body: &mut serde_json::Value,
        target: &RoutingTarget,
    ) -> Result<()> {
        if let Some(applier) = self.auth_appliers.lookup(&target.provider_name) {
            applier.prepare_body(body, target).await?;
        }
        Ok(())
    }

    async fn build_authenticated_request(
        &self,
        client: &reqwest::Client,
        timeouts: &HttpTimeouts,
        url: &str,
        body: &serde_json::Value,
        target: &RoutingTarget,
        transport: &Arc<dyn crate::language_model::protocol::Transport>,
        ctx: &PipelineContext,
        trace_headers: Option<&http::HeaderMap>,
    ) -> Result<reqwest::Request> {
        let mut builder = client.post(url).json(body);
        if let Some(total) = timeouts.total {
            builder = builder.timeout(total);
        }
        let mut request = builder
            .build()
            .map_err(|e| BitrouterError::internal(format!("building request: {e}")))?;
        forward_inbound_anthropic_beta(&mut request, target, ctx);
        let mut request = self.apply_auth(request, target, transport).await?;
        merge_outbound_trace_headers(&mut request, trace_headers);
        Ok(request)
    }

    async fn refresh_auth_after_unauthorized(
        &self,
        target: &RoutingTarget,
        rejected_authorization: Option<&reqwest::header::HeaderValue>,
    ) -> Result<bool> {
        let Some(applier) = self.auth_appliers.lookup(&target.provider_name) else {
            return Ok(false);
        };
        applier
            .refresh_after_unauthorized(target, rejected_authorization)
            .await
    }

    fn no_dispatch_error(target: &RoutingTarget) -> BitrouterError {
        BitrouterError::internal(format!(
            "no outbound dispatch registered for protocol '{}' (target provider '{}'); \
             register an OutboundAdapter + Transport via OutboundDispatch::register",
            target.api_protocol, target.provider_name,
        ))
    }

    /// Refuse to silently drop a caller-supplied `response_format` when the
    /// resolved outbound protocol cannot honour it. Built-in adapters all
    /// support it; only out-of-tree [`ApiProtocol::Custom`] targets that
    /// haven't implemented translation hit this 400.
    fn check_response_format(
        prompt: &Prompt,
        adapter: &Arc<dyn crate::language_model::protocol::OutboundAdapter>,
        target: &RoutingTarget,
    ) -> Result<()> {
        if prompt.response_format.is_some() && !adapter.supports_response_format() {
            return Err(BitrouterError::bad_request(format!(
                "response_format requested but outbound protocol '{}' \
                 (target provider '{}') does not support structured outputs",
                target.api_protocol, target.provider_name,
            )));
        }
        Ok(())
    }
}

/// Merge any outbound headers that an `ObserveHook::on_hop_start` stashed
/// on the context (typically W3C `traceparent` / `tracestate`) into the
/// outbound request, **after** auth has been applied so observability
/// never silently overrides credential headers. Caller-set headers do
/// overwrite same-name auth headers — but the propagator-injected set
/// only ever names W3C trace headers, which auth appliers never touch.
///
/// Spec: <https://www.w3.org/TR/trace-context/>
fn merge_outbound_trace_headers(request: &mut reqwest::Request, headers: Option<&http::HeaderMap>) {
    let Some(headers) = headers else {
        return;
    };
    let dest = request.headers_mut();
    for (name, value) in headers.iter() {
        dest.insert(name.clone(), value.clone());
    }
}

/// Forward the inbound `anthropic-beta` header(s) to a Messages-protocol
/// upstream.
///
/// Anthropic clients (notably Claude Code) gate request-*body* features —
/// `context_management`, interleaved thinking, fine-grained tool streaming — on
/// `anthropic-beta` values. The canonical decode→re-encode preserves those body
/// fields (they ride through `extra`), but builds a fresh outbound request with
/// no beta header, so without this forward the upstream rejects the now-orphaned
/// field with a 400 ("Extra inputs are not permitted"). Scoped to Messages
/// upstreams because the header is meaningless to other wire protocols; the
/// provider's `AuthApplier` runs afterwards and may merge in any
/// credential-required betas (e.g. the Claude Pro/Max OAuth ones).
fn forward_inbound_anthropic_beta(
    request: &mut reqwest::Request,
    target: &RoutingTarget,
    ctx: &PipelineContext,
) {
    if target.api_protocol != ApiProtocol::Messages {
        return;
    }
    let inbound: Vec<_> = ctx
        .headers()
        .get_all("anthropic-beta")
        .iter()
        .cloned()
        .collect();
    for value in inbound {
        request.headers_mut().append("anthropic-beta", value);
    }
}

#[async_trait]
impl Executor for HttpExecutor {
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> Result<ExecutionResult> {
        let (adapter, transport) = self
            .dispatch
            .lookup(&target.api_protocol)
            .ok_or_else(|| Self::no_dispatch_error(target))?;

        Self::check_response_format(prompt, adapter, target)?;

        let mut upstream_prompt = prompt.clone();
        upstream_prompt.model = target.service_id.clone();
        upstream_prompt.stream = false;
        let mut body = adapter.render_request_for_target(&upstream_prompt, target)?;
        self.shape_request_body(&mut body, target).await?;
        let url = transport.endpoint_url(target, false);
        let trace_headers = ctx.take_outbound_trace_headers();

        let (client, timeouts) = self.client_for(target);
        let started = Instant::now();
        let mut attempted_auth_refresh = false;
        let text = loop {
            let request = self
                .build_authenticated_request(
                    &client,
                    &timeouts,
                    &url,
                    &body,
                    target,
                    transport,
                    ctx,
                    trace_headers.as_ref(),
                )
                .await?;
            let rejected_authorization = request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .cloned();
            let response = client.execute(request).await.map_err(|e| {
                if e.is_timeout() {
                    BitrouterError::UpstreamTimeout
                } else {
                    BitrouterError::Upstream {
                        status: 502,
                        message: format!("request to {} failed: {e}", target.provider_name),
                    }
                }
            })?;

            let status = response.status();
            let retry_after =
                parse_retry_after(response.headers().get(reqwest::header::RETRY_AFTER));
            let text = response
                .text()
                .await
                .map_err(|e| upstream_body_error("reading upstream body", e))?;

            if status.is_success() {
                break text;
            }
            if status == reqwest::StatusCode::UNAUTHORIZED
                && !attempted_auth_refresh
                && self
                    .refresh_auth_after_unauthorized(target, rejected_authorization.as_ref())
                    .await?
            {
                attempted_auth_refresh = true;
                continue;
            }
            return Err(classify_upstream_error(status.as_u16(), &text, retry_after));
        };

        let json: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| BitrouterError::Upstream {
                status: 502,
                message: format!("upstream returned non-JSON body: {e}"),
            })?;
        let result = parse_upstream_success(adapter.as_ref(), json)?;
        let elapsed = started.elapsed().as_millis() as u64;

        Ok(ExecutionResult {
            provider_id: target.provider_name.clone(),
            model_id: target.service_id.clone(),
            account_label: target.account_label.clone(),
            result,
            latency_ms: elapsed,
            generation_time_ms: elapsed,
            server_tool_calls: Vec::new(),
        })
    }

    async fn execute_stream(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> Result<StreamPartStream> {
        let (adapter, transport) = self
            .dispatch
            .lookup(&target.api_protocol)
            .ok_or_else(|| Self::no_dispatch_error(target))?;

        Self::check_response_format(prompt, adapter, target)?;

        let mut upstream_prompt = prompt.clone();
        upstream_prompt.model = target.service_id.clone();
        upstream_prompt.stream = true;
        let mut body = adapter.render_request_for_target(&upstream_prompt, target)?;
        self.shape_request_body(&mut body, target).await?;
        let url = transport.endpoint_url(target, true);
        let trace_headers = ctx.take_outbound_trace_headers();

        let (client, timeouts) = self.client_for(target);
        let mut attempted_auth_refresh = false;
        let response = loop {
            let request = self
                .build_authenticated_request(
                    &client,
                    &timeouts,
                    &url,
                    &body,
                    target,
                    transport,
                    ctx,
                    trace_headers.as_ref(),
                )
                .await?;
            let rejected_authorization = request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .cloned();
            let response = client.execute(request).await.map_err(|e| {
                if e.is_timeout() {
                    BitrouterError::UpstreamTimeout
                } else {
                    BitrouterError::Upstream {
                        status: 502,
                        message: format!("stream request to {} failed: {e}", target.provider_name),
                    }
                }
            })?;

            let status = response.status();
            let retry_after =
                parse_retry_after(response.headers().get(reqwest::header::RETRY_AFTER));
            if status.is_success() {
                break response;
            }
            let text = response.text().await.unwrap_or_default();
            if status == reqwest::StatusCode::UNAUTHORIZED
                && !attempted_auth_refresh
                && self
                    .refresh_auth_after_unauthorized(target, rejected_authorization.as_ref())
                    .await?
            {
                attempted_auth_refresh = true;
                continue;
            }
            return Err(classify_upstream_error(status.as_u16(), &text, retry_after));
        };

        // Parse the upstream SSE byte stream into canonical stream parts via
        // the protocol's stateful decoder.
        let mut decoder = adapter.stream_decoder();
        let byte_stream = response.bytes_stream();

        let stream = async_stream::stream! {
            use eventsource_stream::Eventsource;
            let mut events = byte_stream.eventsource();
            while let Some(event) = events.next().await {
                match event {
                    Ok(ev) => {
                        let sse = SseEvent {
                            event: if ev.event.is_empty() { None } else { Some(ev.event) },
                            data: ev.data,
                        };
                        match decoder.decode(&sse) {
                            Ok(parts) => {
                                for p in parts {
                                    yield Ok(p);
                                }
                            }
                            Err(e) => {
                                yield Err(BitrouterError::UpstreamInvalidResponse {
                                    message: e.to_string(),
                                });
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        // A read-timeout that fires mid-stream arrives here as a
                        // transport error — recover the reqwest timeout signal so
                        // it maps to UpstreamTimeout (504), not a blanket 502.
                        let is_timeout = matches!(
                            &e,
                            eventsource_stream::EventStreamError::Transport(re) if re.is_timeout()
                        );
                        yield Err(stream_transport_error(is_timeout, &e));
                        return;
                    }
                }
            }
            match decoder.finish() {
                Ok(parts) => {
                    for p in parts {
                        yield Ok(p);
                    }
                }
                Err(e) => yield Err(BitrouterError::UpstreamInvalidResponse {
                    message: e.to_string(),
                }),
            }
        };

        Ok(Box::pin(stream))
    }
}

/// Routes outbound requests to one of several [`Executor`] implementations,
/// keyed by [`RoutingTarget::api_protocol`].
///
/// Use this when some providers use the built-in [`HttpExecutor`] +
/// [`OutboundDispatch`] (HTTP / JSON / per-protocol auth header) and others
/// bypass that machinery entirely — typically because they use a vendor SDK
/// that owns the transport itself (illustrated below with a hypothetical
/// `aws-sdk-bedrockruntime`-backed executor for AWS Bedrock's native Converse
/// API). No built-in provider needs this today — BitRouter's `aws-bedrock`
/// provider reaches Bedrock's OpenAI-compatible `bedrock-mantle` endpoints over
/// the default `HttpExecutor` instead.
///
/// The `default` executor handles every protocol that is **not** explicitly
/// registered. The four built-in protocols (`openai` / `responses` /
/// `anthropic` / `google`) should remain on the default `HttpExecutor`; only
/// route `ApiProtocol::Custom(_)` protocols away from it.
///
/// ```no_run
/// use std::sync::Arc;
/// use bitrouter_sdk::App;
/// use bitrouter_sdk::language_model::{
///     ApiProtocol, DispatchExecutor, Executor, HttpExecutor, StaticRoutingTable,
/// };
///
/// # async fn run() -> bitrouter_sdk::Result<()> {
/// # struct BedrockExecutor;
/// # #[async_trait::async_trait]
/// # impl Executor for BedrockExecutor {
/// #     async fn execute(
/// #         &self,
/// #         _: &bitrouter_sdk::language_model::RoutingTarget,
/// #         _: &bitrouter_sdk::language_model::Prompt,
/// #         _: &bitrouter_sdk::language_model::PipelineContext,
/// #     ) -> bitrouter_sdk::Result<bitrouter_sdk::language_model::ExecutionResult> {
/// #         unimplemented!()
/// #     }
/// #     async fn execute_stream(
/// #         &self,
/// #         _: &bitrouter_sdk::language_model::RoutingTarget,
/// #         _: &bitrouter_sdk::language_model::Prompt,
/// #         _: &bitrouter_sdk::language_model::PipelineContext,
/// #     ) -> bitrouter_sdk::Result<bitrouter_sdk::language_model::StreamPartStream> {
/// #         unimplemented!()
/// #     }
/// # }
/// let http: Arc<dyn Executor> = Arc::new(HttpExecutor::with_defaults()?);
/// let bedrock: Arc<dyn Executor> = Arc::new(BedrockExecutor);
/// let executor = DispatchExecutor::new(http)
///     .with(ApiProtocol::Custom("bedrock-claude".into()), bedrock);
///
/// let _app = App::builder()
///     .language_model(|lm| {
///         lm.routing_table(Arc::new(StaticRoutingTable::new()))
///           .executor(Arc::new(executor));
///     })
///     .build()?;
/// # Ok(()) }
/// ```
pub struct DispatchExecutor {
    by_protocol: HashMap<ApiProtocol, Arc<dyn Executor>>,
    default: Arc<dyn Executor>,
}

impl DispatchExecutor {
    /// Build a dispatcher with `default` handling every unregistered protocol.
    /// Typically pass an [`HttpExecutor`] here.
    pub fn new(default: Arc<dyn Executor>) -> Self {
        Self {
            by_protocol: HashMap::new(),
            default,
        }
    }

    /// Route requests with `target.api_protocol == protocol` to `executor`.
    /// Subsequent calls with the same `protocol` overwrite the previous entry.
    /// Returns `self` so calls can be chained at construction.
    pub fn with(mut self, protocol: ApiProtocol, executor: Arc<dyn Executor>) -> Self {
        self.register(protocol, executor);
        self
    }

    /// Imperative form of [`with`](Self::with).
    pub fn register(&mut self, protocol: ApiProtocol, executor: Arc<dyn Executor>) {
        self.by_protocol.insert(protocol, executor);
    }
}

#[async_trait]
impl Executor for DispatchExecutor {
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> Result<ExecutionResult> {
        let executor = self
            .by_protocol
            .get(&target.api_protocol)
            .cloned()
            .unwrap_or_else(|| self.default.clone());
        executor.execute(target, prompt, ctx).await
    }

    async fn execute_stream(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> Result<StreamPartStream> {
        let executor = self
            .by_protocol
            .get(&target.api_protocol)
            .cloned()
            .unwrap_or_else(|| self.default.clone());
        executor.execute_stream(target, prompt, ctx).await
    }
}

#[cfg(test)]
mod error_classification_tests {
    use super::*;
    use crate::language_model::protocol::{
        chat_completions::ChatCompletionsAdapter, generate_content::GenerateContentAdapter,
        messages::MessagesAdapter, responses::ResponsesAdapter,
    };

    #[test]
    fn credit_exhaustion_401_maps_to_payment_required() {
        // opencode signals a drained balance with a 401 + CreditsError
        // body — must map to PaymentRequired so failover drops to the
        // next account rather than treating it as an auth failure.
        let body =
            r#"{"type":"error","error":{"type":"CreditsError","message":"Insufficient balance."}}"#;
        match classify_upstream_error(401, body, None) {
            BitrouterError::UpstreamPaymentRequired => {}
            other => panic!("expected UpstreamPaymentRequired, got {other:?}"),
        }
    }

    #[test]
    fn plain_401_stays_an_upstream_error() {
        // A genuine auth failure (no credit signal) must NOT become
        // PaymentRequired — it should fail the request, not silently
        // fall through to the next account.
        match classify_upstream_error(401, r#"{"error":"invalid api key"}"#, None) {
            BitrouterError::Upstream { status, .. } => assert_eq!(status, 401),
            other => panic!("expected Upstream(401), got {other:?}"),
        }
    }

    #[test]
    fn server_error_stays_an_upstream_error() {
        match classify_upstream_error(503, "service unavailable", None) {
            BitrouterError::Upstream { status, .. } => assert_eq!(status, 503),
            other => panic!("expected Upstream(503), got {other:?}"),
        }
    }

    #[test]
    fn upstream_429_has_a_distinct_safe_error() {
        match classify_upstream_error(429, r#"{"secret":"provider quota"}"#, Some(17)) {
            BitrouterError::UpstreamRateLimited { retry_after } => {
                assert_eq!(retry_after, Some(17));
            }
            other => panic!("expected UpstreamRateLimited, got {other:?}"),
        }
    }

    #[test]
    fn malformed_success_is_upstream_502_for_every_builtin_protocol() {
        let adapters: [&dyn OutboundAdapter; 4] = [
            &ChatCompletionsAdapter,
            &MessagesAdapter,
            &ResponsesAdapter,
            &GenerateContentAdapter,
        ];
        for adapter in adapters {
            let error = parse_upstream_success(adapter, serde_json::json!({}))
                .expect_err("empty success body must not parse");
            assert!(
                matches!(error, BitrouterError::UpstreamInvalidResponse { .. }),
                "{} returned {error:?}",
                adapter.protocol()
            );
            assert_eq!(error.status(), 502);
        }
    }

    #[test]
    fn retry_after_accepts_seconds_http_date_and_rejects_invalid_values() {
        let seconds = reqwest::header::HeaderValue::from_static("42");
        assert_eq!(parse_retry_after(Some(&seconds)), Some(42));

        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(120);
        let date =
            reqwest::header::HeaderValue::from_str(&httpdate::fmt_http_date(future)).unwrap();
        let parsed = parse_retry_after(Some(&date)).unwrap();
        assert!((119..=120).contains(&parsed), "parsed delay was {parsed}");

        let invalid = reqwest::header::HeaderValue::from_static("soon-ish");
        assert_eq!(parse_retry_after(Some(&invalid)), None);
        assert_eq!(parse_retry_after(None), None);
    }

    #[test]
    fn credit_phrase_family_is_recognised() {
        assert!(looks_like_credit_exhaustion("Insufficient balance"));
        assert!(looks_like_credit_exhaustion(
            "INSUFFICIENT CREDIT remaining"
        ));
        assert!(looks_like_credit_exhaustion("you are out of credits"));
        assert!(looks_like_credit_exhaustion(
            r#"{"error":{"type":"CreditsError"}}"#
        ));
        assert!(!looks_like_credit_exhaustion("invalid request: bad model"));
    }

    #[test]
    fn mid_stream_timeout_maps_to_upstream_timeout() {
        // A read-timeout that fires *after* the SSE stream is open surfaces as
        // a transport error inside the decode loop. It must be classified as
        // UpstreamTimeout (504), not a generic 502 — otherwise the coarse
        // stream-idle guard is mislabelled once streaming starts.
        match stream_transport_error(true, "connection timed out") {
            BitrouterError::UpstreamTimeout => {}
            other => panic!("expected UpstreamTimeout, got {other:?}"),
        }
    }

    #[test]
    fn mid_stream_non_timeout_stays_a_502() {
        // A parse / non-timeout transport error keeps the existing 502 mapping
        // and preserves the underlying message.
        match stream_transport_error(false, "malformed SSE frame") {
            BitrouterError::Upstream { status, message } => {
                assert_eq!(status, 502);
                assert!(message.contains("malformed SSE frame"), "got {message:?}");
            }
            other => panic!("expected Upstream(502), got {other:?}"),
        }
    }
}

#[cfg(test)]
mod beta_forward_tests {
    use super::*;
    use crate::caller::CallerContext;
    use crate::language_model::types::Prompt;
    use crate::language_model::{Message, PipelineRequest, Role};

    fn ctx_with_beta(beta: Option<&str>) -> PipelineContext {
        let mut headers = http::HeaderMap::new();
        if let Some(b) = beta {
            headers.insert("anthropic-beta", http::HeaderValue::from_str(b).unwrap());
        }
        let prompt = Prompt {
            model: "claude".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message {
                role: Role::User,
                content: vec![],
            }],
            tools: vec![],
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        PipelineContext::new(PipelineRequest {
            request_id: "t".into(),
            model: "claude".into(),
            caller: CallerContext::local(),
            headers,
            prompt,
            inbound_protocol: None,
        })
    }

    fn target(proto: ApiProtocol) -> RoutingTarget {
        RoutingTarget {
            provider_name: "anthropic".into(),
            service_id: "claude-haiku".into(),
            api_base: "https://api.anthropic.com/v1".into(),
            api_key: String::new(),
            api_protocol: proto,
            chat_token_limit_field: None,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
        }
    }

    fn fresh_request() -> reqwest::Request {
        reqwest::Client::new()
            .post("https://api.anthropic.com/v1/messages")
            .build()
            .unwrap()
    }

    #[test]
    fn forwards_anthropic_beta_to_messages_upstream() {
        let mut request = fresh_request();
        forward_inbound_anthropic_beta(
            &mut request,
            &target(ApiProtocol::Messages),
            &ctx_with_beta(Some("context-management-2025-06-27")),
        );
        let got: Vec<_> = request
            .headers()
            .get_all("anthropic-beta")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        assert_eq!(got, vec!["context-management-2025-06-27"]);
    }

    #[test]
    fn does_not_forward_to_non_messages_upstream() {
        // A messages→chat translation must not leak the Anthropic-only header.
        let mut request = fresh_request();
        forward_inbound_anthropic_beta(
            &mut request,
            &target(ApiProtocol::ChatCompletions),
            &ctx_with_beta(Some("context-management-2025-06-27")),
        );
        assert!(request.headers().get("anthropic-beta").is_none());
    }

    #[test]
    fn no_inbound_beta_is_a_noop() {
        let mut request = fresh_request();
        forward_inbound_anthropic_beta(
            &mut request,
            &target(ApiProtocol::Messages),
            &ctx_with_beta(None),
        );
        assert!(request.headers().get("anthropic-beta").is_none());
    }
}

#[cfg(test)]
mod client_selection_tests {
    use super::*;
    use crate::caller::CallerContext;
    use crate::language_model::types::ApiProtocol;
    use crate::language_model::{GenerationParams, Message, PipelineRequest, Role};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn target(provider: &str) -> RoutingTarget {
        RoutingTarget {
            provider_name: provider.into(),
            service_id: "m".into(),
            api_base: "https://api.example.com".into(),
            api_key: String::new(),
            api_protocol: ApiProtocol::ChatCompletions,
            chat_token_limit_field: None,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
        }
    }

    #[test]
    fn per_provider_override_selected_by_name_else_default() {
        let default = HttpTimeouts::default();
        let mut overrides = HashMap::new();
        overrides.insert(
            "slow".to_string(),
            HttpTimeouts {
                read: Duration::from_secs(300),
                ..HttpTimeouts::default()
            },
        );
        let exec = HttpExecutor::with_provider_timeouts(
            default.clone(),
            overrides,
            OutboundDispatch::builtin(),
            AuthAppliers::new(),
        )
        .expect("build executor");

        // A provider with an override resolves to its own timeouts…
        let (_, slow) = exec.client_for(&target("slow"));
        assert_eq!(slow.read, Duration::from_secs(300));
        // …and one absent from the map falls back to the default.
        let (_, other) = exec.client_for(&target("openai"));
        assert_eq!(other.read, default.read);
    }

    #[test]
    fn override_equal_to_default_builds_no_extra_client() {
        // An override identical to the default must not create a redundant
        // per-provider client — the provider resolves to the default pair.
        let default = HttpTimeouts::default();
        let mut overrides = HashMap::new();
        overrides.insert("same".to_string(), default.clone());
        let exec = HttpExecutor::with_provider_timeouts(
            default,
            overrides,
            OutboundDispatch::builtin(),
            AuthAppliers::new(),
        )
        .expect("build executor");
        let clients = exec.clients.read().expect("client set lock");
        assert!(
            clients.provider_clients.is_empty(),
            "an override equal to the default should be skipped"
        );
    }

    #[test]
    fn reload_provider_timeouts_replaces_selected_timeouts() {
        let default = HttpTimeouts::default();
        let exec = HttpExecutor::with_provider_timeouts(
            default.clone(),
            HashMap::new(),
            OutboundDispatch::builtin(),
            AuthAppliers::new(),
        )
        .expect("build executor");

        let (_, before) = exec.client_for(&target("slow"));
        assert_eq!(before.read, default.read);

        let mut overrides = HashMap::new();
        overrides.insert(
            "slow".to_string(),
            HttpTimeouts {
                read: Duration::from_secs(450),
                ..default.clone()
            },
        );
        exec.reload_provider_timeouts(default, overrides)
            .expect("reload timeout clients");

        let (_, after) = exec.client_for(&target("slow"));
        assert_eq!(after.read, Duration::from_secs(450));
    }

    #[tokio::test]
    async fn non_streaming_body_read_timeout_maps_to_upstream_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("test server addr");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request_buf = [0_u8; 1024];
            let _ = socket.read(&mut request_buf).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\n\
                      content-type: application/json\r\n\
                      content-length: 1024\r\n\
                      \r\n\
                      {\"id\":\"partial\"",
                )
                .await
                .expect("write partial response");
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let exec = HttpExecutor::new(HttpTimeouts {
            read: Duration::from_millis(75),
            ..HttpTimeouts::default()
        })
        .expect("build executor");
        let target = RoutingTarget {
            provider_name: "slow".into(),
            service_id: "m".into(),
            api_base: format!("http://{addr}/v1"),
            api_key: "k".into(),
            api_protocol: ApiProtocol::ChatCompletions,
            chat_token_limit_field: None,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
        };
        let prompt = Prompt {
            model: "m".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message::text(Role::User, "hi")],
            tools: vec![],
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        let ctx = PipelineContext::new(PipelineRequest::new(
            "m",
            CallerContext::local(),
            prompt.clone(),
        ));

        let err =
            tokio::time::timeout(Duration::from_secs(3), exec.execute(&target, &prompt, &ctx))
                .await
                .expect("executor should return before outer timeout")
                .expect_err("partial stalled body should fail");
        server.abort();

        match err {
            BitrouterError::UpstreamTimeout => {}
            other => panic!("expected UpstreamTimeout, got {other:?}"),
        }
    }
}
