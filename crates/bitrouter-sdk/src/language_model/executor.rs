//! The `Executor` — the component that turns a resolved `RoutingTarget` plus a
//! `Prompt` into an upstream call. Ships the trait, a `MockExecutor` for tests,
//! and `HttpExecutor` — the real protocol-aware HTTP executor.

use std::pin::Pin;
use std::sync::{Mutex, RwLock};
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use futures::StreamExt;
use futures_core::Stream;

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::{BitrouterError, Result};
use crate::language_model::context::PipelineContext;
use crate::language_model::context::ProviderContinuation;
use crate::language_model::context::RequireContinuationAuthority;
use crate::language_model::context::SuppressProviderContinuation;
use crate::language_model::types::{ExecutionResult, RoutingTarget};
use bitrouter_ai::auth::{
    AppliedAuth, AuthAppliers, AuthOperation, ContinuationAuthority, CredentialAuthority,
    normalize_auth_extension_error,
};
use bitrouter_ai::client::{HttpTimeouts, ModelClient, parse_retry_after};
use bitrouter_ai::decisions::{DecisionRequest, DecisionResult};
use bitrouter_ai::protocol::OutboundDispatch;
use bitrouter_ai::protocol::decisions::{DecisionsCodec, DecisionsTransport};
use bitrouter_ai::types::{ApiProtocol, GenerateResult, Prompt, StreamPart};
use tokio_util::sync::CancellationToken;

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
    /// Validate conversion before an observable provider attempt.
    /// Custom executors add their own representation rules here. No I/O or
    /// credential resolution belongs in preflight; execution checks again.
    /// Return the categorical assessment so eligible effects can be observed.
    /// The pipeline rechecks refusals even if a custom executor returns Ok.
    fn preflight(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        _stream: bool,
    ) -> Result<bitrouter_ai::conversion::ConversionReport> {
        let report = bitrouter_ai::conversion::request_admission(prompt, &target.api_protocol);
        report.require_admitted()?;
        Ok(report)
    }

    /// Validate native Decisions support without authentication or I/O.
    fn preflight_decisions(
        &self,
        _target: &RoutingTarget,
        _request: &DecisionRequest,
    ) -> Result<()> {
        Err(BitrouterError::bad_request(
            "executor does not support Decisions",
        ))
    }

    /// Execute a native Decisions request. Custom executors opt in explicitly.
    async fn execute_decisions(
        &self,
        _target: &RoutingTarget,
        _request: &DecisionRequest,
        _ctx: &PipelineContext,
    ) -> Result<ExecutionResult> {
        Err(BitrouterError::bad_request(
            "executor does not support Decisions",
        ))
    }

    /// Execute a non-streaming generation request against `target`.
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
    /// A successful native Decisions result.
    Decisions(DecisionResult),
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
        use bitrouter_ai::types::{Content, FinishReason, Usage};
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
            .map_err(|_| BitrouterError::internal("mock executor lock poisoned"))?
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
                result: result.into(),
                request_duration_ms: 1,
                upstream_duration_ms: Some(1),
                server_tool_calls: Vec::new(),
            }),
            MockResponse::Stream(_) | MockResponse::Decisions(_) => Err(BitrouterError::internal(
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
            MockResponse::Generate(_) | MockResponse::Decisions(_) => {
                Err(BitrouterError::internal(
                    "MockExecutor: scripted a non-streaming response for a streaming call",
                ))
            }
            MockResponse::Error(e) => Err(e),
        }
    }

    fn preflight_decisions(&self, target: &RoutingTarget, request: &DecisionRequest) -> Result<()> {
        if target.api_protocol != ApiProtocol::Decisions {
            return Err(BitrouterError::bad_request(
                "Decisions requires a Decisions target",
            ));
        }
        DecisionsCodec::render_request(request)?;
        Ok(())
    }

    async fn execute_decisions(
        &self,
        target: &RoutingTarget,
        request: &DecisionRequest,
        _ctx: &PipelineContext,
    ) -> Result<ExecutionResult> {
        self.preflight_decisions(target, request)?;
        match self.next()? {
            MockResponse::Decisions(result) => Ok(ExecutionResult {
                provider_id: target.provider_name.clone(),
                model_id: target.service_id.clone(),
                account_label: target.account_label.clone(),
                result: result.into(),
                request_duration_ms: 1,
                upstream_duration_ms: Some(1),
                server_tool_calls: Vec::new(),
            }),
            MockResponse::Error(error) => Err(error),
            MockResponse::Generate(_) | MockResponse::Stream(_) => Err(BitrouterError::internal(
                "mock response operation does not match Decisions",
            )),
        }
    }
}

// ===== real HTTP executor =====

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

fn bounded_upstream_detail(body: &str) -> Option<String> {
    let body = body.trim();
    if body.is_empty() {
        None
    } else {
        Some(truncate_upstream_message(body))
    }
}

fn normalize_upstream_error_payload(body: &str) -> serde_json::Value {
    let parsed = match serde_json::from_str::<serde_json::Value>(body) {
        Ok(value) => value,
        Err(_) => return serde_json::Value::String(body.to_string()),
    };
    let nested_error = parsed
        .as_object()
        .and_then(|object| object.get("error"))
        .filter(|error| !error.is_null())
        .cloned();
    match nested_error.unwrap_or(parsed) {
        value @ (serde_json::Value::Object(_) | serde_json::Value::String(_)) => value,
        other => serde_json::Value::String(other.to_string()),
    }
}

/// Turn a non-2xx upstream response into the right [`BitrouterError`].
///
/// Most non-2xx maps to [`BitrouterError::Upstream`] carrying the status.
/// Request rejection, rate limiting, and credit exhaustion use distinct
/// variants so callers can apply explicit fallback and response policies.
pub(crate) fn classify_upstream_error(
    status: u16,
    body: &str,
    retry_after: Option<u64>,
) -> BitrouterError {
    if status == 429 {
        return BitrouterError::UpstreamRateLimited {
            retry_after,
            detail: bounded_upstream_detail(body),
        };
    }
    // RFC 9110 section 15.5.1 defines 400 as the server being unable or
    // unwilling to process the request because of a perceived client error:
    // <https://www.rfc-editor.org/rfc/rfc9110#section-15.5.1>.
    if status == 400 {
        return BitrouterError::UpstreamBadRequest {
            error: normalize_upstream_error_payload(body),
        };
    }
    if matches!(status, 401..=403) && looks_like_credit_exhaustion(body) {
        return BitrouterError::UpstreamPaymentRequired {
            detail: bounded_upstream_detail(body),
        };
    }
    BitrouterError::Upstream {
        status,
        message: truncate_upstream_message(body),
    }
}

struct ProviderContinuationSubstitution {
    native: String,
    public_or_redacted: String,
}

struct UpstreamErrorScrubber {
    redactor: bitrouter_ai::diagnostics::DiagnosticRedactor,
}

impl UpstreamErrorScrubber {
    fn new(continuation: Option<ProviderContinuationSubstitution>) -> Self {
        let mut scrubber = Self {
            redactor: bitrouter_ai::diagnostics::DiagnosticRedactor::default(),
        };
        if let Some(continuation) = continuation {
            scrubber
                .redactor
                .add_replacement(continuation.native, continuation.public_or_redacted);
        }
        scrubber
    }

    fn capture_request_credentials(&mut self, request: &reqwest::Request, target: &RoutingTarget) {
        self.redactor
            .capture_request_credentials(request, target.effective_api_key());
    }

    fn capture_effective_target_key(&mut self, target: &RoutingTarget) {
        self.redactor.add_replacement(
            target.effective_api_key().to_owned(),
            "[redacted credential]".to_owned(),
        );
    }

    fn scrub_text(&self, text: &str) -> String {
        self.redactor.scrub_text(text)
    }
    fn scrub_value(&self, value: &mut serde_json::Value) {
        self.redactor.scrub_value(value);
    }
    fn scrub_body(&self, body: &str) -> String {
        self.redactor.scrub_body(body)
    }

    fn scrub_error(&self, error: BitrouterError) -> BitrouterError {
        match error {
            BitrouterError::BadRequest { message } => BitrouterError::BadRequest {
                message: self.scrub_text(&message),
            },
            BitrouterError::Unauthorized(message) => {
                BitrouterError::Unauthorized(self.scrub_text(&message))
            }
            BitrouterError::PaymentRequired(message) => {
                BitrouterError::PaymentRequired(self.scrub_text(&message))
            }
            BitrouterError::UpstreamPaymentRequired { detail } => {
                BitrouterError::UpstreamPaymentRequired {
                    detail: detail.map(|detail| self.scrub_text(&detail)),
                }
            }
            BitrouterError::Forbidden(message) => {
                BitrouterError::Forbidden(self.scrub_text(&message))
            }
            BitrouterError::NotFound(message) => {
                BitrouterError::NotFound(self.scrub_text(&message))
            }
            error @ BitrouterError::RateLimited { .. } => error,
            BitrouterError::UpstreamRateLimited {
                retry_after,
                detail,
            } => BitrouterError::UpstreamRateLimited {
                retry_after,
                detail: detail.map(|detail| self.scrub_text(&detail)),
            },
            BitrouterError::UpstreamBadRequest { mut error } => {
                self.scrub_value(&mut error);
                BitrouterError::UpstreamBadRequest { error }
            }
            BitrouterError::UpstreamPolicyViolation { message } => {
                BitrouterError::UpstreamPolicyViolation {
                    message: self.scrub_text(&message),
                }
            }
            BitrouterError::Upstream { status, message } => BitrouterError::Upstream {
                status,
                message: self.scrub_text(&message),
            },
            BitrouterError::UpstreamInvalidResponse { message } => {
                BitrouterError::UpstreamInvalidResponse {
                    message: self.scrub_text(&message),
                }
            }
            BitrouterError::UpstreamAuth {
                status,
                www_authenticate,
                required_scope,
            } => BitrouterError::UpstreamAuth {
                status,
                www_authenticate: www_authenticate.map(|value| self.scrub_text(&value)),
                required_scope: required_scope.map(|value| self.scrub_text(&value)),
            },
            BitrouterError::Internal(message) => {
                BitrouterError::Internal(self.scrub_text(&message))
            }
            error @ (BitrouterError::Incompatible { .. }
            | BitrouterError::UpstreamTimeout
            | BitrouterError::UpstreamUnavailable
            | BitrouterError::Cancelled) => error,
        }
    }
}

fn apply_provider_continuation(
    body: &mut serde_json::Value,
    target: &RoutingTarget,
    ctx: &PipelineContext,
) -> Result<Option<ProviderContinuationSubstitution>> {
    if ctx.extension::<SuppressProviderContinuation>().is_some() {
        if target.api_protocol != ApiProtocol::Responses {
            return Err(BitrouterError::internal(
                "detached provider continuation requires a Responses target",
            ));
        }
        let Some(object) = body.as_object_mut() else {
            return Err(BitrouterError::internal(
                "Responses request body must be an object",
            ));
        };
        object.remove("previous_response_id");
        return Ok(None);
    }
    let Some(continuation) = ctx.extension::<ProviderContinuation>() else {
        return Ok(None);
    };
    if !continuation.matches_target(target) {
        return Err(BitrouterError::internal(
            "provider continuation target mismatch",
        ));
    }
    if target.api_protocol != ApiProtocol::Responses {
        return Err(BitrouterError::internal(
            "provider continuation requires a Responses target",
        ));
    }
    let Some(object) = body.as_object_mut() else {
        return Err(BitrouterError::internal(
            "Responses request body must be an object",
        ));
    };
    let public_or_redacted = object
        .get("previous_response_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| id.starts_with("brc_"))
        .unwrap_or("[redacted provider continuation]")
        .to_owned();
    let native = continuation.response_id().to_owned();
    object.insert(
        "previous_response_id".to_owned(),
        serde_json::Value::String(native.clone()),
    );
    Ok(Some(ProviderContinuationSubstitution {
        native,
        public_or_redacted,
    }))
}

fn validate_continuation_authority(
    target: &RoutingTarget,
    ctx: &PipelineContext,
    actual: Option<&ContinuationAuthority>,
) -> Result<()> {
    if target.api_protocol == ApiProtocol::Responses
        && ctx.extension::<RequireContinuationAuthority>().is_some()
        && actual.is_none()
    {
        return Err(BitrouterError::bad_request(
            "native Responses continuation authority unavailable for dynamic authentication",
        ));
    }
    if let Some(continuation) = ctx.extension::<ProviderContinuation>()
        && actual != Some(continuation.credential_authority())
    {
        return Err(BitrouterError::bad_request(
            "provider continuation credential authority changed before dispatch",
        ));
    }
    Ok(())
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
    dispatch: Arc<OutboundDispatch>,
    auth_appliers: AuthAppliers,
}

struct HttpClientSet {
    /// Client used for any provider without a per-provider override, plus its
    /// timeouts (for the per-request `total` cap, which is not a client
    /// setting).
    default_client: ModelClient,
    default_timeouts: HttpTimeouts,
    /// Per-provider clients keyed by `provider_name`, each paired with the
    /// resolved timeouts it was built from. Built once at construction; empty
    /// in the common single-timeout deployment.
    provider_clients: HashMap<String, (HttpTimeouts, ModelClient)>,
}

/// A fully constructed upstream-client replacement that has not yet become
/// visible to requests.
///
/// Building a `reqwest::Client` can fail, while installing an already-built
/// set only swaps a lock-protected value. Keeping those stages separate lets a
/// caller validate an entire reload candidate before changing another live
/// subsystem.
pub struct PreparedProviderTimeouts(HttpClientSet);

enum SelectedRequest<'a> {
    Generation(&'a Prompt),
    Decisions(&'a DecisionRequest),
}

/// Immutable inputs reused each time an authenticated upstream request is
/// rebuilt, including after a provider refreshes an expired credential.
struct RequestBuildInput<'a> {
    client: &'a ModelClient,
    url: &'a str,
    body: &'a serde_json::Value,
    target: &'a RoutingTarget,
    transport: &'a Arc<dyn bitrouter_ai::protocol::Transport>,
    ctx: &'a PipelineContext,
    trace_headers: Option<&'a http::HeaderMap>,
}

fn build_http_client_set(
    default_timeouts: HttpTimeouts,
    per_provider: HashMap<String, HttpTimeouts>,
    dispatch: Arc<OutboundDispatch>,
) -> Result<HttpClientSet> {
    let default_client =
        ModelClient::with_dispatch(default_timeouts.clone(), Arc::clone(&dispatch))?;
    let mut provider_clients = HashMap::new();
    for (name, timeouts) in per_provider {
        if timeouts == default_timeouts {
            continue;
        }
        let client = ModelClient::with_dispatch(timeouts.clone(), Arc::clone(&dispatch))?;
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
    /// of per-provider [`AuthApplier`](bitrouter_ai::auth::AuthApplier)s.
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
        let dispatch = Arc::new(dispatch);
        let clients = build_http_client_set(default_timeouts, per_provider, Arc::clone(&dispatch))?;
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
        let prepared = self.prepare_provider_timeouts(default_timeouts, per_provider)?;
        self.commit_provider_timeouts(prepared);
        Ok(())
    }

    /// Build replacement timeout clients without making them live.
    pub fn prepare_provider_timeouts(
        &self,
        default_timeouts: HttpTimeouts,
        per_provider: HashMap<String, HttpTimeouts>,
    ) -> Result<PreparedProviderTimeouts> {
        let clients =
            build_http_client_set(default_timeouts, per_provider, Arc::clone(&self.dispatch))?;
        Ok(PreparedProviderTimeouts(clients))
    }

    /// Make a previously prepared timeout-client set visible to new requests.
    /// Existing requests retain the client they selected before this swap.
    pub fn commit_provider_timeouts(&self, prepared: PreparedProviderTimeouts) {
        let mut guard = match self.clients.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = prepared.0;
    }

    /// Pick the client + timeouts for `target`: a per-provider override when one
    /// is registered for its `provider_name`, else the default pair.
    fn client_for(&self, target: &RoutingTarget) -> (ModelClient, HttpTimeouts) {
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

    /// Apply the per-provider [`AuthApplier`](bitrouter_ai::auth::AuthApplier)
    /// if one is registered for `target.provider_name`, else fall through to
    /// `Transport::authorise`. Shared by both `execute` and `execute_stream`.
    async fn apply_auth(
        &self,
        request: reqwest::Request,
        target: &RoutingTarget,
        transport: &Arc<dyn bitrouter_ai::protocol::Transport>,
    ) -> Result<AppliedAuth> {
        if let Some(applier) = self.auth_appliers.lookup(&target.provider_name) {
            applier
                .apply_with_authority(request, &target.model_target())
                .await
                .map_err(|error| {
                    normalize_auth_extension_error(error, AuthOperation::RequestAuthentication)
                        .into()
                })
        } else {
            let request = transport.authorise(request, &target.model_target()).await?;
            let credential = target
                .api_key_override
                .as_deref()
                .unwrap_or(target.api_key.as_str());
            Ok(AppliedAuth::proven(
                request,
                CredentialAuthority::derive("static-transport-credential", credential),
            ))
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
            applier
                .prepare_body(body, &target.model_target())
                .await
                .map_err(|error| {
                    BitrouterError::from(normalize_auth_extension_error(
                        error,
                        AuthOperation::BodyPreparation,
                    ))
                })?;
        }
        Ok(())
    }

    async fn build_authenticated_request(
        &self,
        input: &RequestBuildInput<'_>,
    ) -> Result<reqwest::Request> {
        let mut request = input.client.build_request(input.url, input.body)?;
        forward_inbound_anthropic_beta(&mut request, input.target, input.ctx);
        let applied = self
            .apply_auth(request, input.target, input.transport)
            .await?;
        let (mut request, mut credential_authority) = applied.into_parts();
        apply_provider_headers(&mut request, input.target, input.ctx);
        merge_outbound_trace_headers(&mut request, input.trace_headers);
        inject_outbound_request_id(&mut request, input.ctx)?;
        credential_authority =
            credential_authority.filter(|authority| authority.validates_final_request(&request));
        validate_continuation_authority(input.target, input.ctx, credential_authority.as_ref())?;
        input.ctx.record_credential_authority(credential_authority);
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
            .refresh_after_unauthorized(&target.model_target(), rejected_authorization)
            .await
            .map_err(|error| {
                BitrouterError::from(normalize_auth_extension_error(
                    error,
                    AuthOperation::Refresh,
                ))
            })
    }

    fn no_dispatch_error(target: &RoutingTarget) -> BitrouterError {
        BitrouterError::internal(format!(
            "no outbound dispatch registered for protocol '{}' (target provider '{}'); \
             register an OutboundAdapter + Transport via OutboundDispatch::register",
            target.api_protocol, target.provider_name,
        ))
    }

    /// Send either native JSON operation through the same selected-target policy.
    /// Every authentication recovery renders and shapes a fresh body.
    async fn execute_json(
        &self,
        target: &RoutingTarget,
        input: SelectedRequest<'_>,
        transport: &Arc<dyn bitrouter_ai::protocol::Transport>,
        ctx: &PipelineContext,
    ) -> Result<(String, UpstreamErrorScrubber, u64)> {
        let (client, _) = self.client_for(target);
        let cancellation = CancellationToken::new();
        let url = transport.endpoint_url(&target.model_target(), false);
        let trace_headers = ctx.take_outbound_trace_headers();
        let mut scrubber = UpstreamErrorScrubber::new(None);
        scrubber.capture_effective_target_key(target);
        let started = Instant::now();
        let mut refreshed = false;
        loop {
            let mut body = match input {
                SelectedRequest::Generation(prompt) => {
                    client.render_request(&target.model_target(), prompt, false)?
                }
                SelectedRequest::Decisions(request) => {
                    client.render_decision_request(&target.model_target(), request)?
                }
            };
            self.shape_request_body(&mut body, target)
                .await
                .map_err(|error| scrubber.scrub_error(error))?;
            match input {
                SelectedRequest::Generation(_) => {
                    if let Some(continuation) = apply_provider_continuation(&mut body, target, ctx)?
                    {
                        scrubber
                            .redactor
                            .add_replacement(continuation.native, continuation.public_or_redacted);
                    }
                }
                SelectedRequest::Decisions(_) => {
                    DecisionsCodec::parse_request(body.clone())?;
                }
            }
            let request = self
                .build_authenticated_request(&RequestBuildInput {
                    client: &client,
                    url: &url,
                    body: &body,
                    target,
                    transport,
                    ctx,
                    trace_headers: trace_headers.as_ref(),
                })
                .await
                .map_err(|error| scrubber.scrub_error(error))?;
            scrubber.capture_request_credentials(&request, target);
            let rejected_authorization = request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .cloned();
            let response = client
                .send(request, &cancellation)
                .await
                .map_err(|error| scrubber.scrub_error(error.into()))?;
            let status = response.status();
            let retry_after =
                parse_retry_after(response.headers().get(reqwest::header::RETRY_AFTER));
            let text = ModelClient::read_body(response, &cancellation)
                .await
                .map_err(|error| scrubber.scrub_error(error.into()))?;
            if status.is_success() {
                return Ok((text, scrubber, started.elapsed().as_millis() as u64));
            }
            if status == reqwest::StatusCode::UNAUTHORIZED
                && !refreshed
                && self
                    .refresh_auth_after_unauthorized(target, rejected_authorization.as_ref())
                    .await
                    .map_err(|error| scrubber.scrub_error(error))?
            {
                refreshed = true;
                continue;
            }
            return Err(classify_upstream_error(
                status.as_u16(),
                &scrubber.scrub_body(&text),
                retry_after,
            ));
        }
    }

    /// The ChatGPT/Codex backend accepts only streaming Responses requests,
    /// while compatibility callers may require one non-streaming result.
    /// AI selects the SSE requirement and owns canonical result collection.
    /// The SDK retains request policy and the pipeline execution envelope.
    async fn execute_streaming_generation(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> Result<ExecutionResult> {
        let started = Instant::now();
        let result = bitrouter_ai::stream::collect::collect_generate(
            self.execute_stream(target, prompt, ctx).await?,
        )
        .await?;
        let elapsed = started.elapsed().as_millis() as u64;
        Ok(ExecutionResult {
            provider_id: target.provider_name.clone(),
            model_id: target.service_id.clone(),
            account_label: target.account_label.clone(),
            result: result.into(),
            request_duration_ms: elapsed,
            upstream_duration_ms: Some(elapsed),
            server_tool_calls: Vec::new(),
        })
    }
}

/// Apply the selected provider's header rules after authentication. This lets
/// explicit provider compatibility headers replace transport defaults while
/// reserved authentication, framing, tracing, and request-id fields remain
/// outside this mechanism. The rules themselves were validated when the route
/// was built.
///
/// HTTP field semantics: <https://www.rfc-editor.org/rfc/rfc9110.html#section-5>
fn apply_provider_headers(
    request: &mut reqwest::Request,
    target: &RoutingTarget,
    ctx: &PipelineContext,
) {
    for rule in &target.headers {
        let name = rule.name();
        request.headers_mut().remove(name);
        if rule.passthrough() {
            let inbound = ctx
                .headers()
                .get_all(name)
                .iter()
                .cloned()
                .collect::<Vec<_>>();
            if !inbound.is_empty() {
                for value in inbound {
                    request.headers_mut().append(name.clone(), value);
                }
                continue;
            }
        }
        if let Some(value) = rule.default() {
            request.headers_mut().insert(name.clone(), value.clone());
        }
    }
}

/// Merge any outbound headers that an `ObserveHook::on_hop_start` stashed
/// on the context (typically W3C `traceparent` / `tracestate`) into the
/// outbound request after auth has been applied. `PipelineContext` admits
/// only exact W3C trace field names into this map; the executor nevertheless
/// revalidates final wire authentication after this merge and request-id
/// injection, immediately before returning the request for dispatch.
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

/// Bind every upstream hop to the pipeline's stable request identity.
///
/// The value comes from the pipeline context, not from an arbitrary outbound
/// header, so retries and fallback hops share the same reconciliation key.
fn inject_outbound_request_id(request: &mut reqwest::Request, ctx: &PipelineContext) -> Result<()> {
    let value = http::HeaderValue::from_str(ctx.request_id()).map_err(|error| {
        BitrouterError::internal(format!("invalid pipeline request id header: {error}"))
    })?;
    request
        .headers_mut()
        .insert("x-bitrouter-request-id", value);
    Ok(())
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
    fn preflight(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        stream: bool,
    ) -> Result<bitrouter_ai::conversion::ConversionReport> {
        let (client, _) = self.client_for(target);
        client
            .render_request_with_report(&target.model_target(), prompt, stream)
            .map(|(_, report)| report)
            .map_err(Into::into)
    }

    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> Result<ExecutionResult> {
        if bitrouter_ai::providers::codex::requires_streaming(&target.model_target()) {
            return self.execute_streaming_generation(target, prompt, ctx).await;
        }
        let (adapter, transport) = self
            .dispatch
            .lookup(&target.api_protocol)
            .ok_or_else(|| Self::no_dispatch_error(target))?;

        let (text, error_scrubber, elapsed) = self
            .execute_json(target, SelectedRequest::Generation(prompt), transport, ctx)
            .await?;

        let result = ModelClient::parse_response(adapter.as_ref(), &target.api_protocol, &text)
            .map_err(|error| error_scrubber.scrub_error(BitrouterError::from(error)))?;
        Ok(ExecutionResult {
            provider_id: target.provider_name.clone(),
            model_id: target.service_id.clone(),
            account_label: target.account_label.clone(),
            result: result.into(),
            request_duration_ms: elapsed,
            upstream_duration_ms: Some(elapsed),
            server_tool_calls: Vec::new(),
        })
    }

    fn preflight_decisions(&self, target: &RoutingTarget, request: &DecisionRequest) -> Result<()> {
        let (client, _) = self.client_for(target);
        client.render_decision_request(&target.model_target(), request)?;
        Ok(())
    }

    async fn execute_decisions(
        &self,
        target: &RoutingTarget,
        request: &DecisionRequest,
        ctx: &PipelineContext,
    ) -> Result<ExecutionResult> {
        self.preflight_decisions(target, request)?;
        let transport: Arc<dyn bitrouter_ai::protocol::Transport> = Arc::new(DecisionsTransport);
        let (text, scrubber, elapsed) = self
            .execute_json(target, SelectedRequest::Decisions(request), &transport, ctx)
            .await?;
        let result = ModelClient::parse_decision_response(&text, request).map_err(|error| {
            let error = scrubber.redactor.scrub_error(error);
            if error.is_completed_decision_failure() {
                ctx.record_decision_failure_usage(error.decision_usage().cloned());
            }
            BitrouterError::from(error)
        })?;
        Ok(ExecutionResult {
            provider_id: target.provider_name.clone(),
            model_id: target.service_id.clone(),
            account_label: target.account_label.clone(),
            result: result.into(),
            request_duration_ms: elapsed,
            upstream_duration_ms: Some(elapsed),
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

        let (client, _) = self.client_for(target);
        let cancellation = CancellationToken::new();
        let mut body = client.render_request(&target.model_target(), prompt, true)?;
        self.shape_request_body(&mut body, target).await?;
        let continuation_substitution = apply_provider_continuation(&mut body, target, ctx)?;
        let mut error_scrubber = UpstreamErrorScrubber::new(continuation_substitution);
        error_scrubber.capture_effective_target_key(target);
        let url = transport.endpoint_url(&target.model_target(), true);
        let trace_headers = ctx.take_outbound_trace_headers();

        let request_input = RequestBuildInput {
            client: &client,
            url: &url,
            body: &body,
            target,
            transport,
            ctx,
            trace_headers: trace_headers.as_ref(),
        };
        let mut attempted_auth_refresh = false;
        let response = loop {
            let request = self
                .build_authenticated_request(&request_input)
                .await
                .map_err(|error| error_scrubber.scrub_error(error))?;
            error_scrubber.capture_request_credentials(&request, target);
            let rejected_authorization = request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .cloned();
            let response = client
                .send(request, &cancellation)
                .await
                .map_err(|error| error_scrubber.scrub_error(BitrouterError::from(error)))?;

            let status = response.status();
            let retry_after =
                parse_retry_after(response.headers().get(reqwest::header::RETRY_AFTER));
            if status.is_success() {
                break response;
            }
            let text = ModelClient::read_body(response, &cancellation)
                .await
                .map_err(|error| error_scrubber.scrub_error(BitrouterError::from(error)))?;
            if status == reqwest::StatusCode::UNAUTHORIZED
                && !attempted_auth_refresh
                && self
                    .refresh_auth_after_unauthorized(target, rejected_authorization.as_ref())
                    .await
                    .map_err(|error| error_scrubber.scrub_error(error))?
            {
                attempted_auth_refresh = true;
                continue;
            }
            let scrubbed = error_scrubber.scrub_body(&text);
            return Err(classify_upstream_error(
                status.as_u16(),
                &scrubbed,
                retry_after,
            ));
        };

        let stream = ModelClient::decode_stream(Arc::clone(adapter), response, cancellation).map(
            move |part| {
                part.map_err(|error| error_scrubber.scrub_error(BitrouterError::from(error)))
            },
        );

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
/// use bitrouter_ai::types::ApiProtocol;
/// use bitrouter_sdk::App;
/// use bitrouter_sdk::language_model::{
///     DispatchExecutor, Executor, HttpExecutor, StaticRoutingTable,
/// };
///
/// # async fn run() -> bitrouter_sdk::Result<()> {
/// # struct BedrockExecutor;
/// # #[async_trait::async_trait]
/// # impl Executor for BedrockExecutor {
/// #     async fn execute(
/// #         &self,
/// #         _: &bitrouter_sdk::language_model::RoutingTarget,
/// #         _: &bitrouter_ai::types::Prompt,
/// #         _: &bitrouter_sdk::language_model::PipelineContext,
/// #     ) -> bitrouter_sdk::Result<bitrouter_sdk::language_model::ExecutionResult> {
/// #         unimplemented!()
/// #     }
/// #     async fn execute_stream(
/// #         &self,
/// #         _: &bitrouter_sdk::language_model::RoutingTarget,
/// #         _: &bitrouter_ai::types::Prompt,
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
    fn preflight(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        stream: bool,
    ) -> Result<bitrouter_ai::conversion::ConversionReport> {
        let executor = self
            .by_protocol
            .get(&target.api_protocol)
            .unwrap_or(&self.default);
        executor.preflight(target, prompt, stream)
    }

    fn preflight_decisions(&self, target: &RoutingTarget, request: &DecisionRequest) -> Result<()> {
        self.by_protocol
            .get(&target.api_protocol)
            .unwrap_or(&self.default)
            .preflight_decisions(target, request)
    }

    async fn execute_decisions(
        &self,
        target: &RoutingTarget,
        request: &DecisionRequest,
        ctx: &PipelineContext,
    ) -> Result<ExecutionResult> {
        self.by_protocol
            .get(&target.api_protocol)
            .unwrap_or(&self.default)
            .execute_decisions(target, request, ctx)
            .await
    }

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
    use bitrouter_ai::protocol::OutboundAdapter;
    use bitrouter_ai::protocol::chat_completions::ChatCompletionsAdapter;
    use bitrouter_ai::protocol::generate_content::GenerateContentAdapter;
    use bitrouter_ai::protocol::messages::MessagesAdapter;
    use bitrouter_ai::protocol::responses::ResponsesAdapter;

    #[test]
    fn credit_exhaustion_401_maps_to_payment_required() {
        // opencode signals a drained balance with a 401 + CreditsError
        // body — must map to PaymentRequired so failover drops to the
        // next account rather than treating it as an auth failure.
        let body =
            r#"{"type":"error","error":{"type":"CreditsError","message":"Insufficient balance."}}"#;
        match classify_upstream_error(401, body, None) {
            BitrouterError::UpstreamPaymentRequired { detail } => {
                assert_eq!(detail.as_deref(), Some(body));
            }
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
            BitrouterError::UpstreamRateLimited {
                retry_after,
                detail,
            } => {
                assert_eq!(retry_after, Some(17));
                assert_eq!(detail.as_deref(), Some(r#"{"secret":"provider quota"}"#));
            }
            other => panic!("expected UpstreamRateLimited, got {other:?}"),
        }
    }

    #[test]
    fn upstream_error_payload_selection_prefers_non_null_nested_error() {
        assert_eq!(
            normalize_upstream_error_payload(
                r#"{"error":{"message":"bad","param":"max_tokens"},"request_id":"req_1"}"#,
            ),
            serde_json::json!({"message": "bad", "param": "max_tokens"})
        );
        assert_eq!(
            normalize_upstream_error_payload(r#"{"error":"bad temperature"}"#),
            serde_json::json!("bad temperature")
        );
    }

    #[test]
    fn upstream_error_payload_selection_uses_whole_object_without_non_null_error() {
        assert_eq!(
            normalize_upstream_error_payload(r#"{"message":"bad","status":400}"#),
            serde_json::json!({"message": "bad", "status": 400})
        );
        assert_eq!(
            normalize_upstream_error_payload(r#"{"error":null,"message":"bad"}"#),
            serde_json::json!({"error": null, "message": "bad"})
        );
    }

    #[test]
    fn upstream_error_payload_selection_normalizes_non_object_values_to_strings() {
        for (body, expected) in [
            (r#""bad request""#, "bad request"),
            ("not json", "not json"),
            ("[1,2]", "[1,2]"),
            ("42", "42"),
            ("true", "true"),
            ("null", "null"),
        ] {
            assert_eq!(
                normalize_upstream_error_payload(body),
                serde_json::Value::String(expected.to_string())
            );
        }
    }

    #[test]
    fn upstream_400_carries_selected_error_payload() {
        match classify_upstream_error(
            400,
            r#"{"error":{"message":"max_tokens rejected"},"ignored":"value"}"#,
            None,
        ) {
            BitrouterError::UpstreamBadRequest { error } => {
                assert_eq!(error, serde_json::json!({"message": "max_tokens rejected"}));
            }
            other => panic!("expected UpstreamBadRequest, got {other:?}"),
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
            let error = ModelClient::parse_response(adapter, &adapter.protocol(), "{}")
                .map_err(BitrouterError::from)
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
    fn stream_decoder_preserves_typed_upstream_policy_violation() {
        let error = BitrouterError::from(bitrouter_ai::error::ModelError::PolicyViolation {
            message: "provider detail must stay internal".to_string(),
        });

        assert!(matches!(
            error,
            BitrouterError::UpstreamPolicyViolation { .. }
        ));
        assert_eq!(error.status(), 403);
        assert_eq!(error.error_code(), "upstream_policy_violation");
        assert_eq!(error.public_message(), "upstream content policy violation");
    }

    #[test]
    fn stream_decoder_preserves_explicit_upstream_status() {
        let error = BitrouterError::from(bitrouter_ai::error::ModelError::Provider {
            status: 401,
            message: "chat completions stream error".to_string(),
        });

        assert!(matches!(
            error,
            BitrouterError::Upstream { status: 401, .. }
        ));
    }

    #[test]
    fn stream_decoder_still_wraps_generic_parse_errors_as_upstream_502() {
        let error = BitrouterError::from(bitrouter_ai::error::ModelError::InvalidResponse {
            message: "malformed provider event".into(),
        });

        assert!(matches!(
            error,
            BitrouterError::UpstreamInvalidResponse { .. }
        ));
        assert_eq!(error.status(), 502);
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
        match BitrouterError::from(bitrouter_ai::error::ModelError::Timeout) {
            BitrouterError::UpstreamTimeout => {}
            other => panic!("expected UpstreamTimeout, got {other:?}"),
        }
    }

    #[test]
    fn mid_stream_non_timeout_stays_a_502() {
        // A parse / non-timeout transport error keeps the existing 502 mapping
        // and preserves the underlying message.
        match BitrouterError::from(bitrouter_ai::error::ModelError::Decode {
            message: "upstream stream error: malformed SSE frame".into(),
        }) {
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
    use crate::language_model::PipelineRequest;
    use crate::language_model::types::OutboundHeaderRule;
    use bitrouter_ai::types::Prompt;
    use bitrouter_ai::types::{Message, Role};

    fn ctx_with_headers(headers: http::HeaderMap) -> PipelineContext {
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
            original_model: "claude".into(),
            model: "claude".into(),
            caller: CallerContext::local(),
            headers,
            input: crate::language_model::types::PipelineInput::Generation(Box::new(prompt)),
            inbound_protocol: None,
        })
    }

    fn ctx_with_beta(beta: Option<&str>) -> PipelineContext {
        let mut headers = http::HeaderMap::new();
        if let Some(b) = beta
            && let Ok(value) = http::HeaderValue::from_str(b)
        {
            headers.insert("anthropic-beta", value);
        }
        ctx_with_headers(headers)
    }

    fn target(proto: ApiProtocol) -> RoutingTarget {
        RoutingTarget {
            provider_name: "anthropic".into(),
            service_id: "claude-haiku".into(),
            api_base: "https://api.anthropic.com/v1".into(),
            api_key: String::new(),
            api_protocol: proto,
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
            reasoning_effort: None,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
            headers: Vec::new(),
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

    #[test]
    fn forwards_pipeline_request_id_to_every_upstream() {
        let mut request = fresh_request();
        let ctx = ctx_with_beta(None);

        inject_outbound_request_id(&mut request, &ctx).unwrap();

        assert_eq!(
            request
                .headers()
                .get("x-bitrouter-request-id")
                .and_then(|value| value.to_str().ok()),
            Some("t")
        );
    }

    #[test]
    fn pipeline_request_id_replaces_untrusted_outbound_value() {
        let mut request = fresh_request();
        request.headers_mut().insert(
            "x-bitrouter-request-id",
            http::HeaderValue::from_static("untrusted"),
        );

        inject_outbound_request_id(&mut request, &ctx_with_beta(None)).unwrap();

        assert_eq!(
            request
                .headers()
                .get("x-bitrouter-request-id")
                .and_then(|value| value.to_str().ok()),
            Some("t")
        );
    }

    #[test]
    fn provider_headers_apply_passthrough_default_and_rejection() -> crate::Result<()> {
        let mut inbound = http::HeaderMap::new();
        inbound.append(
            "x-opencode-session",
            http::HeaderValue::from_static("request-session-a"),
        );
        inbound.append(
            "x-opencode-session",
            http::HeaderValue::from_static("request-session-b"),
        );
        inbound.insert(
            "user-agent",
            http::HeaderValue::from_static("untrusted-agent"),
        );
        inbound.insert("x-rejected", http::HeaderValue::from_static("untrusted"));
        let ctx = ctx_with_headers(inbound);
        let mut target = target(ApiProtocol::ChatCompletions);
        target.headers = vec![
            OutboundHeaderRule::new("x-opencode-session", Some("static-session"), true)?,
            OutboundHeaderRule::new("user-agent", Some("my-agent/1.0"), false)?,
            OutboundHeaderRule::new("x-rejected", None::<&str>, false)?,
            OutboundHeaderRule::new("x-static-only", Some("static"), true)?,
        ];
        let mut request = fresh_request();
        request
            .headers_mut()
            .insert("x-rejected", http::HeaderValue::from_static("transport"));

        apply_provider_headers(&mut request, &target, &ctx);

        let sessions = request
            .headers()
            .get_all("x-opencode-session")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect::<Vec<_>>();
        assert_eq!(sessions, vec!["request-session-a", "request-session-b"]);
        assert_eq!(request.headers()["user-agent"], "my-agent/1.0");
        assert_eq!(request.headers()["x-static-only"], "static");
        assert!(request.headers().get("x-rejected").is_none());
        Ok(())
    }

    #[tokio::test]
    async fn authenticated_request_applies_provider_headers() -> crate::Result<()> {
        let mut inbound = http::HeaderMap::new();
        inbound.insert(
            "x-opencode-session",
            http::HeaderValue::from_static("request-session"),
        );
        let ctx = ctx_with_headers(inbound);
        let mut target = target(ApiProtocol::ChatCompletions);
        target.api_key = "provider-secret".to_string();
        target.headers = vec![OutboundHeaderRule::new(
            "x-opencode-session",
            Some("static-session"),
            true,
        )?];
        let executor = HttpExecutor::with_defaults()?;
        let (_, transport) = executor
            .dispatch
            .lookup(&target.api_protocol)
            .ok_or_else(|| BitrouterError::internal("chat transport was not registered"))?;
        let (client, _) = executor.client_for(&target);
        let body = serde_json::json!({"model": "claude-haiku"});
        let request = executor
            .build_authenticated_request(&RequestBuildInput {
                client: &client,
                url: "https://api.example/v1/chat/completions",
                body: &body,
                target: &target,
                transport,
                ctx: &ctx,
                trace_headers: None,
            })
            .await?;

        assert_eq!(
            request.headers()[reqwest::header::AUTHORIZATION],
            "Bearer provider-secret"
        );
        assert_eq!(request.headers()["x-opencode-session"], "request-session");
        assert_eq!(request.headers()["x-bitrouter-request-id"], "t");
        Ok(())
    }

    #[test]
    fn provider_headers_cannot_take_over_auth_or_internal_fields() {
        for name in [
            "authorization",
            "x-api-key",
            "x-goog-api-key",
            "content-length",
            "traceparent",
            "x-bitrouter-request-id",
        ] {
            let error = OutboundHeaderRule::new(name, Some("value"), true)
                .err()
                .unwrap_or_else(|| BitrouterError::internal("reserved provider header accepted"));
            assert!(error.to_string().contains("is reserved"), "got: {error}");
        }
    }
}

#[cfg(test)]
mod provider_continuation_tests {
    use super::*;
    use crate::caller::CallerContext;
    use crate::language_model::PipelineRequest;
    use crate::language_model::context::{
        ProviderContinuation, RequireContinuationAuthority, SuppressProviderContinuation,
    };
    use bitrouter_ai::types::{GenerationParams, Message, Role};

    fn responses_target(provider: &str) -> RoutingTarget {
        RoutingTarget {
            provider_name: provider.into(),
            service_id: "gpt-5".into(),
            api_base: "https://api.example/v1".into(),
            api_key: "key".into(),
            api_protocol: ApiProtocol::Responses,
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
            reasoning_effort: None,
            account_label: Some("primary".into()),
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
            headers: Vec::new(),
        }
    }

    fn plain_context() -> PipelineContext {
        let prompt = Prompt {
            model: "gpt-5".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message::text(Role::User, "continue")],
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        PipelineContext::new(PipelineRequest::new(
            "gpt-5",
            CallerContext::local(),
            prompt,
        ))
    }

    fn context(target: &RoutingTarget) -> PipelineContext {
        let mut ctx = plain_context();
        ctx.insert_extension(Arc::new(ProviderContinuation::new(
            "resp-native-secret".into(),
            target,
            ContinuationAuthority::new(
                CredentialAuthority::derive("test/static", "secret-key"),
                bitrouter_ai::types::AuthScheme::Bearer,
            ),
        )));
        ctx
    }

    #[test]
    fn authority_requirement_fails_closed_only_for_responses_targets() {
        let mut ctx = plain_context();
        ctx.insert_extension(Arc::new(RequireContinuationAuthority));
        let responses = responses_target("legacy-dynamic");
        let error = validate_continuation_authority(&responses, &ctx, None).unwrap_err();
        assert!(error.to_string().contains("authority unavailable"));

        let mut chat = responses;
        chat.api_protocol = ApiProtocol::ChatCompletions;
        validate_continuation_authority(&chat, &ctx, None).unwrap();
    }

    #[test]
    fn effective_target_key_is_scrubbed_from_custom_header_without_sensitive_name() {
        let target = responses_target("openai");
        let mut request = reqwest::Client::new()
            .post("https://api.example/v1/responses")
            .build()
            .expect("request");
        request.headers_mut().insert(
            "x-provider-session",
            reqwest::header::HeaderValue::from_static("key"),
        );
        request.headers_mut().insert(
            "x-provider-region",
            reqwest::header::HeaderValue::from_static("ordinary-region"),
        );
        let mut scrubber = UpstreamErrorScrubber::new(None);
        scrubber.capture_request_credentials(&request, &target);

        let scrubbed = scrubber.scrub_body("key ordinary-region");
        assert!(!scrubbed.contains("key"));
        assert!(scrubbed.contains("ordinary-region"));
    }

    #[test]
    fn effective_target_key_is_seeded_before_authenticated_request_build() {
        for (api_key, api_key_override, sensitive) in [
            ("static-key-private", None, "static-key-private"),
            (
                "unused-static-private",
                Some("override-key-private"),
                "override-key-private",
            ),
        ] {
            let mut target = responses_target("openai");
            target.api_key = api_key.to_owned();
            target.api_key_override = api_key_override.map(str::to_owned);
            let mut scrubber = UpstreamErrorScrubber::new(None);
            scrubber.capture_effective_target_key(&target);

            let error = scrubber.scrub_error(BitrouterError::Internal(format!(
                "authenticated request build failed for {sensitive}"
            )));
            assert!(
                !format!("{error:?}").contains(sensitive),
                "effective target credential leaked from pre-wire auth failure"
            );
        }
    }

    #[test]
    fn encoded_sensitive_query_value_is_scrubbed_without_touching_ordinary_query_values() {
        let target = responses_target("openai");
        let request = reqwest::Client::new()
            .post("https://api.example/v1/responses?api-key=abc%2F%2B%25&api-version=2026-08-01")
            .build()
            .expect("request");
        let mut scrubber = UpstreamErrorScrubber::new(None);
        scrubber.capture_request_credentials(&request, &target);

        let scrubbed = scrubber.scrub_body("raw=abc%2F%2B%25 decoded=abc/+% 2026-08-01");
        assert!(!scrubbed.contains("abc%2F%2B%25"));
        assert!(!scrubbed.contains("abc/+%"));
        assert!(scrubbed.contains("2026-08-01"));

        let transport_error = scrubber.scrub_error(BitrouterError::Upstream {
            status: 502,
            message:
                "request failed for ?api-key=abc%2F%2B%25&api-version=2026-08-01 decoded=abc/+%"
                    .to_owned(),
        });
        let BitrouterError::Upstream { message, .. } = transport_error else {
            panic!("expected transport-shaped upstream error");
        };
        assert!(!message.contains("abc%2F%2B%25"));
        assert!(!message.contains("abc/+%"));
        assert!(message.contains("2026-08-01"));
    }

    #[test]
    fn plain_error_is_scrubbed_before_generic_upstream_truncation() {
        let native = "native-private-sentinel";
        let credential = "credential-private-sentinel";
        let mut scrubber = UpstreamErrorScrubber::new(Some(ProviderContinuationSubstitution {
            native: native.to_owned(),
            public_or_redacted: "brc_public".to_owned(),
        }));
        scrubber
            .redactor
            .add_replacement(credential.to_owned(), "[redacted credential]".to_owned());
        let body = format!(
            "{native} {credential} {} {native} {credential}",
            "x".repeat(1_500)
        );
        let scrubbed = scrubber.scrub_body(&body);
        let error = classify_upstream_error(503, &scrubbed, None);
        let BitrouterError::Upstream { message, .. } = error else {
            panic!("expected generic upstream error");
        };
        assert!(!message.contains(native));
        assert!(!message.contains(credential));
        assert!(message.contains("brc_public"));
        assert!(message.contains("[truncated]"));
    }

    #[test]
    fn continuation_override_rewrites_only_the_bound_responses_target() {
        let target = responses_target("openai");
        let ctx = context(&target);
        let mut body = serde_json::json!({"previous_response_id": "gateway-id"});
        let substitution = apply_provider_continuation(&mut body, &target, &ctx)
            .unwrap()
            .expect("continuation substitution");
        assert_eq!(body["previous_response_id"], "resp-native-secret");
        assert_eq!(substitution.native, "resp-native-secret");
        assert_eq!(
            substitution.public_or_redacted,
            "[redacted provider continuation]"
        );

        let error = match apply_provider_continuation(
            &mut serde_json::json!({"previous_response_id": "gateway-id"}),
            &responses_target("other"),
            &ctx,
        ) {
            Err(error) => error,
            Ok(_) => panic!("mismatched continuation target was accepted"),
        };
        assert!(error.to_string().contains("target mismatch"));
    }

    #[test]
    fn stock_responses_adapter_suppresses_only_the_detached_parent() {
        let target = responses_target("balanced");
        let mut ctx = plain_context();
        ctx.insert_extension(Arc::new(SuppressProviderContinuation));
        let visible_input = serde_json::json!([
            {"type": "function_call", "call_id": "call-1", "name": "inspect", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call-1", "output": "ok"}
        ]);
        let tools = serde_json::json!([{
            "type": "function",
            "name": "inspect",
            "parameters": {"type": "object"}
        }]);
        let mut body = serde_json::json!({
            "previous_response_id": "brc_public",
            "input": visible_input,
            "tools": tools,
            "parallel_tool_calls": false
        });

        let result = apply_provider_continuation(&mut body, &target, &ctx);

        assert!(matches!(result, Ok(None)));
        assert!(body.get("previous_response_id").is_none());
        assert_eq!(body.get("input"), Some(&visible_input));
        assert_eq!(body.get("tools"), Some(&tools));
        assert_eq!(
            body.get("parallel_tool_calls"),
            Some(&serde_json::Value::Bool(false))
        );
    }
}

#[cfg(test)]
mod client_selection_tests {
    use super::*;
    use crate::caller::CallerContext;
    use crate::language_model::PipelineRequest;
    use bitrouter_ai::types::ApiProtocol;
    use bitrouter_ai::types::{GenerationParams, Message, Role};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn target(provider: &str) -> RoutingTarget {
        RoutingTarget {
            provider_name: provider.into(),
            service_id: "m".into(),
            api_base: "https://api.example.com".into(),
            api_key: String::new(),
            api_protocol: ApiProtocol::ChatCompletions,
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
            reasoning_effort: None,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
            headers: Vec::new(),
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
            chat_supports_store: None,
            chat_supports_stream_options: None,
            reasoning_effort: None,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
            headers: Vec::new(),
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

#[cfg(test)]
mod openai_codex_stream_bridge_tests {
    use super::*;
    use crate::caller::CallerContext;
    use crate::language_model::PipelineRequest;
    use bitrouter_ai::types::{Content, FinishReason, UsageOrigin};
    use bitrouter_ai::types::{GenerationParams, Message, Role};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn read_request_body(socket: &mut tokio::net::TcpStream) -> serde_json::Value {
        let mut received = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = socket.read(&mut buffer).await.expect("read request");
            assert!(count > 0, "request ended before headers were complete");
            received.extend_from_slice(&buffer[..count]);
            let Some(header_end) = received.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&received[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .expect("content-length header");
            let body_start = header_end + 4;
            if received.len() < body_start + content_length {
                continue;
            }
            return serde_json::from_slice(&received[body_start..body_start + content_length])
                .expect("JSON request body");
        }
    }

    #[tokio::test]
    async fn non_streaming_codex_request_uses_streaming_upstream_and_aggregates()
    -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("test server addr");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let body = read_request_body(&mut socket).await;
            if body.get("stream") != Some(&serde_json::Value::Bool(true)) {
                let error = r#"{"detail":"Stream must be set to true"}"#;
                let response = format!(
                    "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                    error.len(),
                    error
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write stream-required response");
                return;
            }

            let events = concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_bridge\"}}\n\n",
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\"}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"bridge ok\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_bridge\",\"status\":\"completed\",\"usage\":{\"input_tokens\":12,\"output_tokens\":4,\"input_tokens_details\":{\"cached_tokens\":5},\"output_tokens_details\":{\"reasoning_tokens\":1}}}}\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
                events.len(),
                events
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write SSE response");
        });

        let executor = HttpExecutor::with_defaults().expect("build executor");
        let target = RoutingTarget {
            provider_name: "openai-codex".into(),
            service_id: "gpt-5.6-terra".into(),
            api_base: format!("http://{addr}"),
            api_key: "unused".into(),
            api_protocol: ApiProtocol::Responses,
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
            reasoning_effort: None,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
            headers: Vec::new(),
        };
        let prompt = Prompt {
            model: "gpt-5.6-terra".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message::text(Role::User, "return one action")],
            tools: vec![],
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        let context = PipelineContext::new(PipelineRequest::new(
            "gpt-5.6-terra",
            CallerContext::local(),
            prompt.clone(),
        ));

        let result = executor
            .execute(&target, &prompt, &context)
            .await
            .expect("openai-codex non-stream request should be bridged");
        server.await.expect("test server");

        assert_eq!(
            result
                .result
                .generation()
                .ok_or_else(|| crate::error::BitrouterError::internal(
                    "expected generation fixture"
                ))?
                .content,
            vec![Content::Text {
                text: "bridge ok".into(),
                provider_metadata: Default::default(),
            }]
        );
        assert_eq!(
            result
                .result
                .generation()
                .ok_or_else(|| crate::error::BitrouterError::internal(
                    "expected generation fixture"
                ))?
                .finish_reason,
            Some(FinishReason::Stop)
        );
        assert_eq!(
            result
                .result
                .generation()
                .ok_or_else(|| crate::error::BitrouterError::internal(
                    "expected generation fixture"
                ))?
                .response_id
                .as_deref(),
            Some("resp_bridge")
        );
        let usage = result
            .result
            .generation()
            .ok_or_else(|| crate::error::BitrouterError::internal("expected generation fixture"))?
            .usage
            .as_ref()
            .expect("provider usage");
        assert_eq!(usage.prompt_tokens, 12);
        assert_eq!(usage.cache_read_tokens, 5);
        assert_eq!(usage.cache_write_tokens, 0);
        assert_eq!(usage.completion_tokens, 4);
        assert_eq!(usage.reasoning_tokens, 1);
        assert_eq!(usage.origin, UsageOrigin::ProviderReported);

        Ok(())
    }
}
