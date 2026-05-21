//! Core data types for the `language_model` protocol: the canonical internal
//! representation (`Prompt` / `StreamPart` / `GenerateResult`) plus routing and
//! pipeline I/O types.
//!
//! These are deliberately minimal in Phase 1 — Phase 2 fills in the full
//! protocol-conversion surface (tool calls, reasoning variants, content blocks).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::caller::CallerContext;

/// The wire protocol an upstream provider speaks.
///
/// The four built-in variants are bidirectional — the SDK both serves them to
/// clients (via an
/// [`InboundAdapter`](crate::language_model::protocol::InboundAdapter)) and
/// calls them upstream (via an
/// [`OutboundAdapter`](crate::language_model::protocol::OutboundAdapter)).
///
/// [`Custom`](Self::Custom) is an extension point for *outbound-only*
/// platform providers (AWS Bedrock, Azure OpenAI, Vertex AI, …). Such a
/// provider lives in its own crate and registers an `OutboundAdapter` +
/// [`Transport`](crate::language_model::protocol::Transport) on the
/// executor's
/// [`OutboundDispatch`](crate::language_model::protocol::OutboundDispatch).
/// The name passed to `Custom` is the registration key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum ApiProtocol {
    /// OpenAI Chat Completions.
    #[default]
    Openai,
    /// Anthropic Messages.
    Anthropic,
    /// Google Generative AI.
    Google,
    /// OpenAI Responses.
    Responses,
    /// An externally-registered protocol identified by its registration name
    /// (e.g. `"bedrock-claude"`). The SDK does not serve `Custom` protocols
    /// inbound; they are outbound-only by design.
    Custom(String),
}

impl ApiProtocol {
    /// Stable string name for this protocol (`"openai"`, `"anthropic"`, …, or
    /// the inner string for [`Custom`](Self::Custom)). Used as the wire-format
    /// representation in YAML config and as the registry key for outbound
    /// dispatch.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
            Self::Google => "google",
            Self::Responses => "responses",
            Self::Custom(name) => name.as_str(),
        }
    }
}

impl std::fmt::Display for ApiProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ApiProtocol {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ApiProtocol {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(match s.as_str() {
            "openai" => Self::Openai,
            "anthropic" => Self::Anthropic,
            "google" => Self::Google,
            "responses" => Self::Responses,
            _ => Self::Custom(s),
        })
    }
}

/// A conversation role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// System / developer instruction.
    System,
    /// End user.
    User,
    /// Model output.
    Assistant,
    /// Tool / function result.
    Tool,
}

/// One content block within a message. Ordered — mixed text + tool-call
/// sequences must preserve their order (v0 #416 regression).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    /// Plain text.
    Text {
        /// The text body.
        text: String,
    },
    /// Model reasoning / thinking content (kept distinct so it is never
    /// silently dropped — v0 #454-1 regression).
    Reasoning {
        /// The reasoning text.
        text: String,
    },
    /// A tool/function call requested by the model.
    ToolCall {
        /// Provider-assigned call id.
        id: String,
        /// Tool name.
        name: String,
        /// JSON-encoded arguments.
        arguments: String,
    },
    /// A tool/function result supplied back to the model.
    ToolResult {
        /// The call id this result answers.
        call_id: String,
        /// Result body. May itself be structured (v0 #364).
        content: String,
    },
}

/// A single message in the conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// The speaker role.
    pub role: Role,
    /// Ordered content blocks.
    pub content: Vec<Content>,
}

impl Message {
    /// Build a plain-text message.
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![Content::Text { text: text.into() }],
        }
    }
}

/// A tool/function the model may call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    /// Tool name.
    pub name: String,
    /// Human description.
    pub description: Option<String>,
    /// JSON Schema for the tool's parameters.
    pub parameters: serde_json::Value,
}

/// Constraint on the shape of the model's response.
///
/// Today the only variant is [`Self::JsonSchema`]; future variants (`json_object`,
/// `text`, `regex`) can be added without breaking existing call sites.
///
/// Each inbound adapter promotes the provider-native field into this typed
/// slot at `parse_request` time (e.g. OpenAI Chat's `response_format`,
/// Anthropic's `output_config.format`, Google's `generationConfig.responseSchema`).
/// Each outbound adapter renders it back into the upstream's native shape on
/// `render_request`. Cross-protocol routing therefore works automatically:
/// an OpenAI-Chat client asking for `json_schema` against an Anthropic upstream
/// emits `output_config.format`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Constrain output to a JSON Schema.
    JsonSchema {
        /// Schema name. Required by OpenAI Chat / Responses; ignored by
        /// Anthropic and Google.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Strict-mode flag. OpenAI-only; Anthropic and Google are always strict.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
        /// The JSON Schema.
        schema: serde_json::Value,
    },
}

/// Sampling / generation parameters, carried verbatim where possible.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GenerationParams {
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus sampling cutoff.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Maximum tokens to generate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Reasoning effort hint (`low` / `medium` / `high`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Any provider-specific extras passed through untouched.
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// The canonical internal request representation. Inbound protocol adapters
/// parse into this; outbound adapters render from it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prompt {
    /// Requested model name (post preset/variant stripping).
    pub model: String,
    /// System instruction, if any. Distinct from the message list because some
    /// providers (Anthropic) carry it out-of-band.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Conversation messages, in order.
    pub messages: Vec<Message>,
    /// Tools available to the model.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tools: Vec<Tool>,
    /// Generation parameters.
    #[serde(default)]
    pub params: GenerationParams,
    /// Constraint on the model's response shape (e.g. a JSON Schema). Inbound
    /// adapters promote the provider-native field into this slot; outbound
    /// adapters render it back natively.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    /// Whether the caller requested a streaming response.
    pub stream: bool,
}

/// Token usage counts. Counts use `0` (not `null`) for "known to be zero";
/// missing usage is represented by `Option<Usage>` being `None` upstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt / input tokens.
    pub prompt_tokens: u64,
    /// Completion / output tokens.
    pub completion_tokens: u64,
    /// Reasoning tokens (subset of `completion_tokens` on most providers).
    pub reasoning_tokens: u64,
    /// Cache-read input tokens — already-cached prompt content that the
    /// provider served from cache. Subset of `prompt_tokens`. Maps to
    /// Anthropic's `usage.cache_read_input_tokens`
    /// (<https://docs.anthropic.com/en/api/messages>) and to OpenAI Chat's
    /// `usage.prompt_tokens_details.cached_tokens`. Default 0 when the
    /// upstream reports no cache stats.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cache_read_tokens: u64,
    /// Cache-write input tokens — prompt content written to the cache this
    /// turn. Subset of `prompt_tokens`. Maps to Anthropic's
    /// `usage.cache_creation_input_tokens`.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cache_write_tokens: u64,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

impl Usage {
    /// Total tokens (prompt + completion).
    pub fn total(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

/// Why generation stopped.
///
/// `Other` and `Error` are escape valves: a finish reason the canonical set
/// doesn't model (kept verbatim for observability), or a mid-stream upstream
/// failure surfaced through the canonical IR rather than abruptly aborting
/// the stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Natural stop.
    Stop,
    /// Hit the max-tokens limit.
    Length,
    /// Stopped to emit a tool call.
    ToolCalls,
    /// Stopped by a content filter / guardrail.
    ContentFilter,
    /// An upstream-provided reason this enum doesn't model — kept verbatim
    /// (e.g. Anthropic's `pause_turn`, an unrecognised OpenAI value).
    Other(String),
    /// Generation aborted mid-stream because upstream returned a stream error
    /// event. Carries the upstream message for observability and for
    /// outbound encoders that want to emit a terminal error frame.
    Error(String),
}

/// A complete non-streaming generation result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerateResult {
    /// Ordered content blocks of the model's reply.
    pub content: Vec<Content>,
    /// Token usage, if the provider reported it.
    pub usage: Option<Usage>,
    /// Finish reason, if the provider reported it.
    pub finish_reason: Option<FinishReason>,
}

/// One part of a streaming response, in canonical internal form. `StreamHook`
/// operates on a `Stream<Item = StreamPart>` before outbound protocol
/// conversion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StreamPart {
    /// An incremental chunk of assistant text.
    TextDelta {
        /// The text fragment.
        text: String,
    },
    /// An incremental chunk of reasoning / thinking text.
    ReasoningDelta {
        /// The reasoning fragment.
        text: String,
    },
    /// An incremental chunk of a tool call's arguments.
    ToolCallDelta {
        /// The call id this delta belongs to.
        id: String,
        /// Tool name (sent once, on the first delta).
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Arguments fragment.
        arguments: String,
    },
    /// A usage report. May arrive mid-stream (per-checkpoint) or only at the end.
    Usage {
        /// The usage counts.
        usage: Usage,
    },
    /// The terminal part: generation finished.
    Finish {
        /// Why generation stopped.
        reason: FinishReason,
    },
    /// Terminal lifecycle part for OpenAI Responses — preserves the response id
    /// and status that a bare [`StreamPart::Finish`] would lose.
    /// Only the Responses decoder emits this; the other protocols' encoders
    /// treat it as a terminal part equivalent to `Finish`.
    ResponseCompleted {
        /// The provider's response id.
        id: String,
        /// The terminal status (`completed` / `incomplete` / `failed`).
        status: String,
        /// Final usage, if the provider reported it.
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
}

impl StreamPart {
    /// Whether this part terminates the stream (`Finish` or `ResponseCompleted`).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            StreamPart::Finish { .. } | StreamPart::ResponseCompleted { .. }
        )
    }
}

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
    /// End-to-end latency in milliseconds.
    pub latency_ms: u64,
    /// Upstream generation time in milliseconds.
    pub generation_time_ms: u64,
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
}

impl std::fmt::Debug for RoutingTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutingTarget")
            .field("provider_name", &self.provider_name)
            .field("service_id", &self.service_id)
            .field("api_base", &self.api_base)
            .field("api_key", &redacted(&self.api_key))
            .field("api_protocol", &self.api_protocol)
            .field("account_label", &self.account_label)
            .field(
                "api_key_override",
                &self.api_key_override.as_deref().map(redacted),
            )
            .field("api_base_override", &self.api_base_override)
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
    /// The raw requested model string (may carry `@preset` / `:variant`).
    pub model: String,
    /// The authenticated (or synthesised) caller.
    pub caller: CallerContext,
    /// Inbound HTTP headers.
    pub headers: http::HeaderMap,
    /// The canonical request body.
    pub prompt: Prompt,
}

impl PipelineRequest {
    /// Build a request with a fresh uuid request id.
    pub fn new(model: impl Into<String>, caller: CallerContext, prompt: Prompt) -> Self {
        let model = model.into();
        Self {
            request_id: uuid::Uuid::new_v4().to_string(),
            model,
            caller,
            headers: http::HeaderMap::new(),
            prompt,
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
