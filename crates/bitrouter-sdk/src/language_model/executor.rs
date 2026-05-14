//! The `Executor` — the component that turns a resolved `RoutingTarget` plus a
//! `Prompt` into an upstream call. Ships the trait, a `MockExecutor` for tests,
//! and `HttpExecutor` — the real protocol-aware HTTP executor.

use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use futures_core::Stream;

use crate::error::{BitrouterError, Result};
use crate::language_model::protocol::{SseEvent, adapter_for};
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
                reasoning_tokens: 0,
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
pub struct HttpExecutor {
    client: reqwest::Client,
}

impl HttpExecutor {
    /// Build an executor with the given upstream timeout configuration (#394).
    pub fn new(timeouts: HttpTimeouts) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(timeouts.connect)
            .read_timeout(timeouts.read)
            .pool_idle_timeout(timeouts.pool_idle)
            .tcp_keepalive(timeouts.tcp_keepalive)
            .build()
            .map_err(|e| BitrouterError::internal(format!("building HTTP client: {e}")))?;
        Ok(Self { client })
    }

    /// Build an executor with default timeouts.
    pub fn with_defaults() -> Result<Self> {
        Self::new(HttpTimeouts::default())
    }

    /// The upstream endpoint URL for a target. Google encodes the model and the
    /// streaming mode in the path; the others use a fixed path.
    ///
    /// Endpoint paths, per the official API references:
    /// - OpenAI Chat: `POST {base}/chat/completions`
    ///   <https://platform.openai.com/docs/api-reference/chat/create>
    /// - OpenAI Responses: `POST {base}/responses`
    ///   <https://platform.openai.com/docs/api-reference/responses/create>
    /// - Anthropic Messages: `POST {base}/messages`
    ///   <https://docs.anthropic.com/en/api/messages>
    /// - Google: `POST {base}/models/{model}:generateContent` (or
    ///   `:streamGenerateContent?alt=sse` for SSE streaming)
    ///   <https://ai.google.dev/api/generate-content>
    fn endpoint_url(target: &RoutingTarget, stream: bool) -> String {
        let base = target.effective_api_base().trim_end_matches('/');
        match target.api_protocol {
            ApiProtocol::Openai => format!("{base}/chat/completions"),
            ApiProtocol::Responses => format!("{base}/responses"),
            ApiProtocol::Anthropic => format!("{base}/messages"),
            ApiProtocol::Google => {
                let verb = if stream {
                    "streamGenerateContent?alt=sse"
                } else {
                    "generateContent"
                };
                format!("{base}/models/{}:{verb}", target.service_id)
            }
        }
    }

    /// Apply the protocol's auth headers to a request builder.
    fn auth_headers(
        builder: reqwest::RequestBuilder,
        target: &RoutingTarget,
    ) -> reqwest::RequestBuilder {
        let key = target.effective_api_key();
        match target.api_protocol {
            ApiProtocol::Openai | ApiProtocol::Responses => builder.bearer_auth(key),
            // Anthropic Messages auth — official: https://docs.anthropic.com/en/api/messages
            ApiProtocol::Anthropic => builder
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01"),
            // Google Generative AI auth — official: https://ai.google.dev/api/rest
            ApiProtocol::Google => builder.header("x-goog-api-key", key),
        }
    }

    /// Render the canonical prompt into the target's wire format, with the
    /// upstream model id substituted and the streaming flag set.
    fn render_body(
        target: &RoutingTarget,
        prompt: &Prompt,
        stream: bool,
    ) -> Result<serde_json::Value> {
        let mut upstream_prompt = prompt.clone();
        upstream_prompt.model = target.service_id.clone();
        upstream_prompt.stream = stream;
        adapter_for(target.api_protocol).render_request(&upstream_prompt)
    }
}

#[async_trait]
impl Executor for HttpExecutor {
    async fn execute(&self, target: &RoutingTarget, prompt: &Prompt) -> Result<ExecutionResult> {
        let url = Self::endpoint_url(target, false);
        let body = Self::render_body(target, prompt, false)?;
        let started = Instant::now();

        let builder = Self::auth_headers(self.client.post(&url).json(&body), target);
        let response = builder.send().await.map_err(|e| {
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
                message: text,
            });
        }

        let json: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| BitrouterError::Upstream {
                status: 502,
                message: format!("upstream returned non-JSON body: {e}"),
            })?;
        let result = adapter_for(target.api_protocol).parse_response(json)?;
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
        let url = Self::endpoint_url(target, true);
        let body = Self::render_body(target, prompt, true)?;

        let builder = Self::auth_headers(self.client.post(&url).json(&body), target);
        let response = builder.send().await.map_err(|e| {
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
                message: text,
            });
        }

        // Parse the upstream SSE byte stream into canonical stream parts via
        // the protocol's stateful decoder.
        let protocol = target.api_protocol;
        let mut decoder = adapter_for(protocol).stream_decoder();
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
