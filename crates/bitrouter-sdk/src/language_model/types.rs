//! Routing targets, execution records and pipeline envelopes.
//!
//! Model content, prompts, results and events are owned by `bitrouter_ai::types`.

use crate::caller::CallerContext;
use bitrouter_ai::target::ModelTarget;
use bitrouter_ai::types::{
    ApiProtocol, AuthScheme, ChatCompletionsCompatibility, ChatTokenLimitField, GenerateResult,
    ModelCompatibility, Prompt, ReasoningEffortConfig, ServerToolCall,
};

/// The result of executing one routing target — the upstream response plus
/// timing. Written into `PipelineContext` after Stage 3.
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    /// The provider id that served the request.
    pub provider_id: String,
    /// The model/service id at that provider.
    pub model_id: String,
    /// Which account of a multi-account provider served the request —
    /// `None` for a single-credential provider. Reflects any failover
    /// hop, so it can differ from the chain's primary account.
    pub account_label: Option<String>,
    /// The generation result.
    pub result: GenerateResult,
    /// End-to-end request duration in milliseconds.
    pub request_duration_ms: u64,
    /// Time spent in the final provider-facing operation.
    pub upstream_duration_ms: Option<u64>,
    /// Server-tool calls observed during this execution (router-executed and
    /// provider-executed). Empty for a plain single-turn upstream call.
    /// Observability only.
    pub server_tool_calls: Vec<ServerToolCall>,
}

/// One provider-configured outbound HTTP header policy.
///
/// The routing layer resolves the provider's YAML entry into this validated
/// wire representation. [`crate::language_model::HttpExecutor`] applies it to
/// every request for the target: an explicitly allowed inbound value wins over
/// [`Self::default`], while a rule without either suppresses that header.
#[derive(Clone, PartialEq, Eq)]
pub struct OutboundHeaderRule {
    name: http::HeaderName,
    default: Option<http::HeaderValue>,
    passthrough: bool,
}

impl OutboundHeaderRule {
    /// Validate and construct an outbound header rule.
    pub fn new(
        name: impl AsRef<str>,
        default: Option<&str>,
        passthrough: bool,
    ) -> crate::Result<Self> {
        let raw_name = name.as_ref();
        let name = http::HeaderName::from_bytes(raw_name.as_bytes()).map_err(|error| {
            crate::BitrouterError::bad_request(format!(
                "invalid provider header name '{raw_name}': {error}"
            ))
        })?;
        if is_reserved_provider_header(&name) {
            return Err(crate::BitrouterError::bad_request(format!(
                "provider header '{}' is reserved",
                name.as_str()
            )));
        }
        let default = default
            .map(|value| {
                http::HeaderValue::from_str(value).map_err(|error| {
                    crate::BitrouterError::bad_request(format!(
                        "invalid value for provider header '{}': {error}",
                        name.as_str()
                    ))
                })
            })
            .transpose()?;
        Ok(Self {
            name,
            default,
            passthrough,
        })
    }

    /// The canonical, case-insensitive HTTP field name.
    pub fn name(&self) -> &http::HeaderName {
        &self.name
    }

    /// The configured static fallback value, if any.
    pub fn default(&self) -> Option<&http::HeaderValue> {
        self.default.as_ref()
    }

    /// Whether the same inbound request header may override the default.
    pub fn passthrough(&self) -> bool {
        self.passthrough
    }
}

impl std::fmt::Debug for OutboundHeaderRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboundHeaderRule")
            .field("name", &self.name)
            .field("has_default", &self.default.is_some())
            .field("passthrough", &self.passthrough)
            .finish()
    }
}

fn is_reserved_provider_header(name: &http::HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization"
            | "proxy-authorization"
            | "x-api-key"
            | "x-goog-api-key"
            | "host"
            | "content-length"
            | "content-type"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "trailer"
            | "te"
            | "upgrade"
            | "traceparent"
            | "tracestate"
            | "x-bitrouter-request-id"
    )
}

/// One hop in a fallback chain: a concrete provider + model + connection info.
///
/// The `Debug` impl redacts `api_key` and `api_key_override` (v0 audit S9):
/// any `tracing::error!(?target, ...)` call by a future contributor would
/// otherwise leak the upstream credential into structured logs.
#[derive(Clone)]
pub struct RoutingTarget {
    /// Provider id (config key).
    pub provider_name: String,
    /// Model / service id at the provider (may differ from the request model).
    pub service_id: String,
    /// Upstream API base URL.
    pub api_base: String,
    /// Upstream API key.
    pub api_key: String,
    /// The wire protocol this target speaks.
    pub api_protocol: ApiProtocol,
    /// Provider/model override for the Chat Completions token-limit field.
    /// `None` preserves the inbound spelling, then falls back to legacy
    /// `max_tokens` when the request originated on another protocol.
    pub chat_token_limit_field: Option<ChatTokenLimitField>,
    /// Whether this Chat Completions target accepts `store`.
    pub chat_supports_store: Option<bool>,
    /// Whether this Chat Completions target accepts `stream_options`.
    pub chat_supports_stream_options: Option<bool>,
    /// Exact qualitative reasoning-effort support for this provider/model.
    /// `None` means unknown, not unsupported.
    pub reasoning_effort: Option<ReasoningEffortConfig>,
    /// Which account of a multi-account provider this target came from
    /// — `None` for a single-credential provider. Surfaced in the
    /// request log so an operator can see which subscription served a
    /// request; carries no routing behaviour itself.
    pub account_label: Option<String>,
    /// Per-request key override. Set by a `RouteHook` that wants to
    /// substitute the caller's own provider key (e.g. BYOK) for this hop.
    /// The SDK itself is opinion-free about whether or how such a hook
    /// exists; it just honours the override when set.
    pub api_key_override: Option<String>,
    /// Per-request api-base override, paired with `api_key_override`.
    pub api_base_override: Option<String>,
    /// How the credential is presented to this target. Consulted by
    /// transports that support more than one scheme — today only the Messages
    /// transport (`x-api-key` vs `Authorization: Bearer`); others ignore it.
    /// Defaults to [`AuthScheme::XApiKey`].
    pub auth_scheme: AuthScheme,
    /// Validated static/default and inbound-passthrough header policies for
    /// every request to this provider.
    pub headers: Vec<OutboundHeaderRule>,
}

impl std::fmt::Debug for RoutingTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutingTarget")
            .field("provider_name", &self.provider_name)
            .field("service_id", &self.service_id)
            .field("api_base", &self.api_base)
            .field("api_key", &redacted(&self.api_key))
            .field("api_protocol", &self.api_protocol)
            .field("chat_token_limit_field", &self.chat_token_limit_field)
            .field("chat_supports_store", &self.chat_supports_store)
            .field(
                "chat_supports_stream_options",
                &self.chat_supports_stream_options,
            )
            .field("reasoning_effort", &self.reasoning_effort)
            .field("account_label", &self.account_label)
            .field(
                "api_key_override",
                &self.api_key_override.as_deref().map(redacted),
            )
            .field("api_base_override", &self.api_base_override)
            .field("auth_scheme", &self.auth_scheme)
            .field("headers", &self.headers)
            .finish()
    }
}

fn redacted(s: &str) -> &'static str {
    if s.is_empty() {
        "<empty>"
    } else {
        "<redacted>"
    }
}

impl RoutingTarget {
    /// Snapshot the effective connection for a codec/transport operation.
    /// Account selection, overrides and header policy remain owned by the SDK.
    pub fn model_target(&self) -> ModelTarget {
        ModelTarget {
            provider_name: self.provider_name.clone(),
            service_id: self.service_id.clone(),
            api_protocol: self.api_protocol.clone(),
            api_base: self.effective_api_base().to_owned(),
            api_key: self.effective_api_key().to_owned(),
            // Hosted inference uses configured keys before the saved Cloud login.
            // Subscription providers retain their stored-account-first fallback policy.
            credential_priority: if self.api_key_override.is_some()
                || (self.provider_name == "bitrouter" && !self.api_key.is_empty())
            {
                bitrouter_ai::target::CredentialPriority::Explicit
            } else {
                bitrouter_ai::target::CredentialPriority::Fallback
            },
            account_label: self.account_label.clone(),
            auth_scheme: self.auth_scheme,
            compatibility: ModelCompatibility {
                chat_completions: ChatCompletionsCompatibility {
                    token_limit_field: self.chat_token_limit_field,
                    supports_store: self.chat_supports_store,
                    supports_stream_options: self.chat_supports_stream_options,
                },
            },
        }
    }

    /// The effective API key (override wins).
    pub fn effective_api_key(&self) -> &str {
        self.api_key_override.as_deref().unwrap_or(&self.api_key)
    }

    /// The effective API base (override wins).
    pub fn effective_api_base(&self) -> &str {
        self.api_base_override.as_deref().unwrap_or(&self.api_base)
    }
}

/// Input to the `language_model` pipeline. Built by an inbound protocol adapter.
#[derive(Debug, Clone)]
pub struct PipelineRequest {
    /// Unique request id (generated if the inbound adapter has none).
    pub request_id: String,
    /// Caller-supplied model selector before ingress transforms. This retains
    /// explicit route/preset intent when later transforms rewrite `model`.
    pub original_model: String,
    /// The raw requested model string (may carry `@preset` / `:variant`).
    pub model: String,
    /// The authenticated (or synthesised) caller.
    pub caller: CallerContext,
    /// Inbound HTTP headers.
    pub headers: http::HeaderMap,
    /// The canonical request body.
    pub prompt: Prompt,
    /// The wire protocol the request arrived on, when known — set by the HTTP
    /// server from the endpoint that was hit. Lets the router prefer a
    /// same-protocol (native) upstream so a faithful round-trip replaces a
    /// lossy cross-protocol translation. `None` for callers that build a
    /// request directly (no native preference is then applied).
    pub inbound_protocol: Option<ApiProtocol>,
}

impl PipelineRequest {
    /// Build a request with a fresh uuid request id.
    pub fn new(model: impl Into<String>, caller: CallerContext, prompt: Prompt) -> Self {
        let model = model.into();
        Self {
            request_id: uuid::Uuid::new_v4().to_string(),
            original_model: model.clone(),
            model,
            caller,
            headers: http::HeaderMap::new(),
            prompt,
            inbound_protocol: None,
        }
    }
}

/// Output of a non-streaming pipeline run.
#[derive(Debug, Clone)]
pub struct PipelineResponse {
    /// The request id this answers.
    pub request_id: String,
    /// The generation result.
    pub result: GenerateResult,
}
