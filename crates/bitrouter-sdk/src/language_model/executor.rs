//! The `Executor` — the component that turns a resolved `RoutingTarget` plus a
//! `Prompt` into an upstream call. Ships the trait, a `MockExecutor` for tests,
//! and `HttpExecutor` — the real protocol-aware HTTP executor.

use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use futures_core::Stream;

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::{BitrouterError, Result};
use crate::language_model::auth::AuthAppliers;
use crate::language_model::protocol::{OutboundDispatch, SseEvent};
use crate::language_model::types::{
    ApiProtocol, ExecutionResult, GenerateResult, Prompt, RoutingTarget, StreamPart,
};

/// A boxed stream of canonical stream parts.
pub type StreamPartStream = Pin<Box<dyn Stream<Item = Result<StreamPart>> + Send>>;

/// Performs the actual upstream call for one routing target.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Execute a non-streaming request against `target`.
    async fn execute(&self, target: &RoutingTarget, prompt: &Prompt) -> Result<ExecutionResult>;

    /// Start a streaming request against `target`.
    async fn execute_stream(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
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
            content: vec![Content::Text { text: text.into() }],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                ..Default::default()
            }),
            finish_reason: Some(FinishReason::Stop),
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
    async fn execute(&self, target: &RoutingTarget, _prompt: &Prompt) -> Result<ExecutionResult> {
        match self.next()? {
            MockResponse::Generate(result) => Ok(ExecutionResult {
                provider_id: target.provider_name.clone(),
                model_id: target.service_id.clone(),
                result,
                latency_ms: 1,
                generation_time_ms: 1,
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
/// no timeouts, so a slow provider could hang a request forever. These four
/// knobs are configured together.
#[derive(Debug, Clone)]
pub struct HttpTimeouts {
    /// TCP connect timeout.
    pub connect: Duration,
    /// Per-read (response body) timeout.
    pub read: Duration,
    /// How long an idle pooled connection is kept.
    pub pool_idle: Duration,
    /// TCP keepalive probe interval.
    pub tcp_keepalive: Duration,
}

impl Default for HttpTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            read: Duration::from_secs(120),
            pool_idle: Duration::from_secs(90),
            tcp_keepalive: Duration::from_secs(60),
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

/// The default upstream [`Executor`] — dispatches a canonical
/// [`Prompt`] to the wire protocol of the resolved [`RoutingTarget`] over
/// HTTP and parses the response back into a canonical [`GenerateResult`] /
/// stream of [`StreamPart`]s.
///
/// Build with [`HttpExecutor::with_defaults`] for sensible timeout defaults
/// or [`HttpExecutor::new`] with a custom [`HttpTimeouts`].
pub struct HttpExecutor {
    client: reqwest::Client,
    dispatch: OutboundDispatch,
    auth_appliers: AuthAppliers,
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
        let client = reqwest::Client::builder()
            .connect_timeout(timeouts.connect)
            .read_timeout(timeouts.read)
            .pool_idle_timeout(timeouts.pool_idle)
            .tcp_keepalive(timeouts.tcp_keepalive)
            .build()
            .map_err(|e| BitrouterError::internal(format!("building HTTP client: {e}")))?;
        Ok(Self {
            client,
            dispatch,
            auth_appliers,
        })
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

#[async_trait]
impl Executor for HttpExecutor {
    async fn execute(&self, target: &RoutingTarget, prompt: &Prompt) -> Result<ExecutionResult> {
        let (adapter, transport) = self
            .dispatch
            .lookup(&target.api_protocol)
            .ok_or_else(|| Self::no_dispatch_error(target))?;

        Self::check_response_format(prompt, adapter, target)?;

        let mut upstream_prompt = prompt.clone();
        upstream_prompt.model = target.service_id.clone();
        upstream_prompt.stream = false;
        let body = adapter.render_request(&upstream_prompt)?;
        let url = transport.endpoint_url(target, false);

        let started = Instant::now();
        let request = self
            .client
            .post(&url)
            .json(&body)
            .build()
            .map_err(|e| BitrouterError::internal(format!("building request: {e}")))?;
        let request = self.apply_auth(request, target, transport).await?;
        let response = self.client.execute(request).await.map_err(|e| {
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
        let text = response
            .text()
            .await
            .map_err(|e| BitrouterError::Upstream {
                status: 502,
                message: format!("reading upstream body: {e}"),
            })?;

        if !status.is_success() {
            return Err(BitrouterError::Upstream {
                status: status.as_u16(),
                message: truncate_upstream_message(&text),
            });
        }

        let json: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| BitrouterError::Upstream {
                status: 502,
                message: format!("upstream returned non-JSON body: {e}"),
            })?;
        let result = adapter.parse_response(json)?;
        let elapsed = started.elapsed().as_millis() as u64;

        Ok(ExecutionResult {
            provider_id: target.provider_name.clone(),
            model_id: target.service_id.clone(),
            result,
            latency_ms: elapsed,
            generation_time_ms: elapsed,
        })
    }

    async fn execute_stream(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
    ) -> Result<StreamPartStream> {
        let (adapter, transport) = self
            .dispatch
            .lookup(&target.api_protocol)
            .ok_or_else(|| Self::no_dispatch_error(target))?;

        Self::check_response_format(prompt, adapter, target)?;

        let mut upstream_prompt = prompt.clone();
        upstream_prompt.model = target.service_id.clone();
        upstream_prompt.stream = true;
        let body = adapter.render_request(&upstream_prompt)?;
        let url = transport.endpoint_url(target, true);

        let request = self
            .client
            .post(&url)
            .json(&body)
            .build()
            .map_err(|e| BitrouterError::internal(format!("building request: {e}")))?;
        let request = self.apply_auth(request, target, transport).await?;
        let response = self.client.execute(request).await.map_err(|e| {
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
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(BitrouterError::Upstream {
                status: status.as_u16(),
                message: truncate_upstream_message(&text),
            });
        }

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
                                yield Err(e);
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        yield Err(BitrouterError::Upstream {
                            status: 502,
                            message: format!("upstream stream error: {e}"),
                        });
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
                Err(e) => yield Err(e),
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
/// that owns the transport itself (e.g. `aws-sdk-bedrockruntime` for AWS
/// Bedrock).
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
/// #     ) -> bitrouter_sdk::Result<bitrouter_sdk::language_model::ExecutionResult> {
/// #         unimplemented!()
/// #     }
/// #     async fn execute_stream(
/// #         &self,
/// #         _: &bitrouter_sdk::language_model::RoutingTarget,
/// #         _: &bitrouter_sdk::language_model::Prompt,
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
    async fn execute(&self, target: &RoutingTarget, prompt: &Prompt) -> Result<ExecutionResult> {
        let executor = self
            .by_protocol
            .get(&target.api_protocol)
            .cloned()
            .unwrap_or_else(|| self.default.clone());
        executor.execute(target, prompt).await
    }

    async fn execute_stream(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
    ) -> Result<StreamPartStream> {
        let executor = self
            .by_protocol
            .get(&target.api_protocol)
            .cloned()
            .unwrap_or_else(|| self.default.clone());
        executor.execute_stream(target, prompt).await
    }
}
