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
    /// OpenAI-style Chat Completions (`POST /v1/chat/completions`).
    #[default]
    ChatCompletions,
    /// Anthropic-style Messages (`POST /v1/messages`).
    Messages,
    /// Google-style Generate Content (`POST …:generateContent`).
    GenerateContent,
    /// Responses.
    Responses,
    /// An externally-registered protocol identified by its registration name
    /// (e.g. `"bedrock-claude"`). The SDK does not serve `Custom` protocols
    /// inbound; they are outbound-only by design.
    Custom(String),
}

impl ApiProtocol {
    /// Stable string name for this protocol (`"chat_completions"`, `"messages"`, …, or
    /// the inner string for [`Custom`](Self::Custom)). Used as the wire-format
    /// representation in YAML config and as the registry key for outbound
    /// dispatch.
    pub fn as_str(&self) -> &str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Messages => "messages",
            Self::GenerateContent => "generate_content",
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
            "chat_completions" => Self::ChatCompletions,
            "messages" => Self::Messages,
            "generate_content" => Self::GenerateContent,
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
    /// A media / file part — image, audio, video, or document. Modelled à la the
    /// Vercel AI SDK `LanguageModelV3File`: a single media-typed part rather than
    /// a per-modality variant, identified by its IANA `media_type`.
    /// <https://github.com/vercel/ai/blob/main/packages/provider/src/language-model/v3/language-model-v3-file.ts>
    File {
        /// IANA media type, e.g. `image/png`, `audio/mpeg`, `application/pdf`.
        media_type: String,
        /// The payload — inline base64 bytes or a URL the upstream fetches.
        data: DataContent,
        /// Original filename, when the provider supplies or requires it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        /// Provider-specific per-part fields preserved verbatim (e.g. an image
        /// `detail` hint). Mirrors the AI SDK's per-part `providerMetadata`.
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        extra: HashMap<String, serde_json::Value>,
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

/// The payload of a [`Content::File`] part. The Vercel AI SDK models this as
/// `Uint8Array | string | URL`; a faithful-passthrough router never decodes the
/// bytes, so we keep only the two wire forms every provider accepts: an inline
/// base64 blob or a URL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DataContent {
    /// Base64-encoded inline bytes (no `data:` URI prefix).
    Base64 {
        /// The base64 payload.
        data: String,
    },
    /// A URL the upstream fetches directly.
    Url {
        /// The resource URL.
        url: String,
    },
}

impl DataContent {
    /// Render as a single URL string: inline base64 bytes become a `data:` URI;
    /// a URL passes through unchanged. For wire forms that carry media in one
    /// URL field (e.g. OpenAI `image_url.url`).
    pub fn to_url(&self, media_type: &str) -> String {
        match self {
            DataContent::Base64 { data } => format!("data:{media_type};base64,{data}"),
            DataContent::Url { url } => url.clone(),
        }
    }

    /// Parse a URL-or-`data:`-URI into an optional embedded media type and the
    /// payload. `data:<mt>;base64,<payload>` yields `(Some(mt), Base64)`; any
    /// other string becomes `(None, Url)`.
    pub fn from_url(value: &str) -> (Option<String>, DataContent) {
        if let Some(rest) = value.strip_prefix("data:")
            && let Some((meta, payload)) = rest.split_once(',')
            && meta.contains(";base64")
        {
            let media_type = meta
                .split(';')
                .next()
                .filter(|m| !m.is_empty())
                .map(str::to_string);
            return (
                media_type,
                DataContent::Base64 {
                    data: payload.to_string(),
                },
            );
        }
        (
            None,
            DataContent::Url {
                url: value.to_string(),
            },
        )
    }
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
/// slot at `parse_request` time (e.g. Chat Completions' `response_format`,
/// Messages' `output_config.format`, Generate Content's `generationConfig.responseSchema`).
/// Each outbound adapter renders it back into the upstream's native shape on
/// `render_request`. Cross-protocol routing therefore works automatically:
/// a Chat Completions client asking for `json_schema` against a Messages upstream
/// emits `output_config.format`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Constrain output to a JSON Schema.
    JsonSchema {
        /// Schema name. Required by Chat Completions / Responses; ignored by
        /// Messages and Generate Content.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Strict-mode flag. Chat Completions / Responses only; Messages and Generate Content are always strict.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
        /// The JSON Schema.
        schema: serde_json::Value,
    },
}

/// An optional inference feature a request may require and a provider may
/// advertise. Capabilities are API-agnostic: the same capability maps to a
/// different wire parameter in each protocol (structured outputs is Chat
/// Completions' `response_format`, Messages' `output_config.format`, Generate
/// Content's `responseSchema`, Responses' `text.format`). The serde
/// representation (e.g. `structured_outputs`) is the stable token used to match
/// a request's needs against a provider's advertised capabilities.
///
/// A capability-aware [`RoutingTable`](crate::language_model::routing::RoutingTable)
/// can read the set a request requires ([`Prompt::required_capabilities`]) and
/// restrict the chain to providers advertising all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// JSON-Schema-constrained output — [`ResponseFormat::JsonSchema`].
    StructuredOutputs,
    /// Tool / function calling — the request supplies a non-empty tool list.
    Tools,
    /// Extended reasoning / thinking — the request sets a reasoning-effort hint.
    Reasoning,
    /// Provider-side web search / browsing.
    WebSearch,
    /// Token log-probabilities in the response.
    Logprobs,
    /// Image input (vision) — a message carries an `image/*` file part.
    ImageInput,
    /// Audio input — a message carries an `audio/*` file part.
    AudioInput,
    /// Video input — a message carries a `video/*` file part.
    VideoInput,
    /// Document / file input — a message carries a non-image/audio/video file
    /// part (e.g. `application/pdf`).
    FileInput,
    /// Image output (generation) — the request asks for an `image` output
    /// modality.
    ImageOutput,
    /// Audio output (speech) — the request asks for an `audio` output modality.
    AudioOutput,
}

impl Capability {
    /// The stable token string (e.g. `"structured_outputs"`), equal to this
    /// enum's serde representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::StructuredOutputs => "structured_outputs",
            Self::Tools => "tools",
            Self::Reasoning => "reasoning",
            Self::WebSearch => "web_search",
            Self::Logprobs => "logprobs",
            Self::ImageInput => "image_input",
            Self::AudioInput => "audio_input",
            Self::VideoInput => "video_input",
            Self::FileInput => "file_input",
            Self::ImageOutput => "image_output",
            Self::AudioOutput => "audio_output",
        }
    }

    /// The input-modality capability implied by a file part's IANA media type;
    /// everything that is not image/audio/video is treated as a document.
    fn from_input_media_type(media_type: &str) -> Capability {
        match media_type.split('/').next() {
            Some("image") => Capability::ImageInput,
            Some("audio") => Capability::AudioInput,
            Some("video") => Capability::VideoInput,
            _ => Capability::FileInput,
        }
    }

    /// The output-modality capability implied by a requested response modality;
    /// `Text` implies no extra capability.
    fn from_output_modality(modality: Modality) -> Option<Capability> {
        match modality {
            Modality::Image => Some(Capability::ImageOutput),
            Modality::Audio => Some(Capability::AudioOutput),
            Modality::Text => None,
        }
    }
}

/// A content modality. Used for the requested output modalities
/// ([`GenerationParams::response_modalities`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    /// Text.
    Text,
    /// Image.
    Image,
    /// Audio.
    Audio,
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
    /// Requested output modalities (OpenAI `modalities` / Gemini
    /// `responseModalities`). Empty means text-only (the default). Drives
    /// `image_output` / `audio_output` capability detection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub response_modalities: Vec<Modality>,
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

impl Prompt {
    /// The [`Capability`]s this request requires, derived from which optional
    /// features it actually uses. A capability-aware routing table can use this
    /// to restrict the fallback chain to providers that advertise all of these,
    /// instead of silently degrading the request.
    ///
    /// Detected from the canonical [`Prompt`]: `structured_outputs` (a
    /// `response_format`), `tools` (a non-empty tool list), `reasoning` (a
    /// reasoning-effort hint), the input modalities of any [`Content::File`]
    /// parts (`image_input` / `audio_input` / `video_input` / `file_input`), and
    /// the requested output modalities (`image_output` / `audio_output`).
    /// `web_search` and `logprobs` ride [`GenerationParams::extra`] and are not
    /// expressible here, so they stay catalog-only and never gate routing.
    pub fn required_capabilities(&self) -> Vec<Capability> {
        let mut caps = Vec::new();
        if self.response_format.is_some() {
            caps.push(Capability::StructuredOutputs);
        }
        if !self.tools.is_empty() {
            caps.push(Capability::Tools);
        }
        if self.params.reasoning_effort.is_some() {
            caps.push(Capability::Reasoning);
        }
        // Input modalities: each file part contributes the capability implied by
        // its media type, deduplicated (one `image_input` for several images).
        for content in self.messages.iter().flat_map(|m| &m.content) {
            if let Content::File { media_type, .. } = content {
                let cap = Capability::from_input_media_type(media_type);
                if !caps.contains(&cap) {
                    caps.push(cap);
                }
            }
        }
        // Output modalities: from the requested response modalities.
        for modality in &self.params.response_modalities {
            if let Some(cap) = Capability::from_output_modality(*modality)
                && !caps.contains(&cap)
            {
                caps.push(cap);
            }
        }
        caps
    }
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
    /// Messages' `usage.cache_read_input_tokens`
    /// (<https://docs.anthropic.com/en/api/messages>) and to Chat Completions'
    /// `usage.prompt_tokens_details.cached_tokens`. Default 0 when the
    /// upstream reports no cache stats.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cache_read_tokens: u64,
    /// Cache-write input tokens — prompt content written to the cache this
    /// turn. Subset of `prompt_tokens`. Maps to Messages'
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

/// Structured detail about why generation stopped. Today this is populated only
/// for refusals — Anthropic returns a `stop_details` object alongside
/// `stop_reason: "refusal"` (surfaced canonically as
/// [`FinishReason::ContentFilter`]) carrying the policy `category` and a
/// human-readable `explanation`. Both are optional: a refusal may map to no
/// named category, and the explanation can be absent or non-stable.
///
/// <https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons#refusal-categories>
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopDetails {
    /// Policy category that triggered the stop (e.g. `"cyber"`, `"bio"`).
    /// `None` when the stop maps to no named category.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Human-readable description of the stop. Not guaranteed stable — surface
    /// it, don't branch on it. `None` when unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
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
    /// Provider-assigned response id (e.g. Chat Completions `chatcmpl-...`,
    /// Messages `msg_...`, Responses `resp_...`, Generate Content
    /// `responseId`). Carried so observability can stamp it onto the OTel
    /// `gen_ai.response.id` semconv attribute and operators can correlate
    /// against the upstream provider's own logs. `None` when the provider
    /// did not surface one.
    ///
    /// Spec: <https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-spans/>
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// Structured stop detail (e.g. a refusal category) when the provider
    /// supplies one. Maps to Anthropic's `stop_details`. `None` for providers
    /// or responses that carry no detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_details: Option<StopDetails>,
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
    /// The provider-assigned response id, surfaced once near the start of
    /// the stream — Chat Completions' top-level `id`, Anthropic's
    /// `message_start.message.id`, Generate Content's `responseId`. (Responses
    /// carries its id on the terminal [`Self::ResponseCompleted`] instead.)
    ///
    /// Not client-facing: outbound encoders drop it (the client gets an id
    /// the inbound encoder generates). Observability uses it to stamp the
    /// GenAI semconv `gen_ai.response.id` attribute on the trace so the
    /// streaming path matches the non-streaming one.
    ///
    /// Spec: <https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-spans/>
    ResponseStarted {
        /// The provider's response id.
        id: String,
    },
    /// The terminal part: generation finished.
    Finish {
        /// Why generation stopped.
        reason: FinishReason,
    },
    /// Terminal lifecycle part for Responses — preserves the response id
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

/// How an upstream's credential is presented on the wire.
///
/// Consulted by transports that support more than one credential scheme.
/// Today that is only the Messages transport, which sends the key as either
/// `x-api-key` (Anthropic's native scheme) or `Authorization: Bearer`. The
/// Chat Completions transport (always `Authorization: Bearer`) and Generate
/// Content transport (always `x-goog-api-key`) have a single fixed scheme and
/// ignore this.
///
/// Exactly one scheme is ever sent — never both: the Anthropic API rejects a
/// request that carries `x-api-key` and `Authorization` together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum AuthScheme {
    /// `x-api-key: <key>` — Anthropic's native scheme; the default.
    #[default]
    #[serde(rename = "x-api-key")]
    XApiKey,
    /// `Authorization: Bearer <key>`.
    #[serde(rename = "bearer")]
    Bearer,
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
    /// How the credential is presented to this target. Consulted by
    /// transports that support more than one scheme — today only the Messages
    /// transport (`x-api-key` vs `Authorization: Bearer`); others ignore it.
    /// Defaults to [`AuthScheme::XApiKey`].
    pub auth_scheme: AuthScheme,
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
            .field("auth_scheme", &self.auth_scheme)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn bare_prompt() -> Prompt {
        Prompt {
            model: "m".into(),
            system: None,
            messages: vec![],
            tools: vec![],
            params: GenerationParams::default(),
            response_format: None,
            stream: false,
        }
    }

    #[test]
    fn plain_request_requires_no_capabilities() {
        assert!(bare_prompt().required_capabilities().is_empty());
    }

    #[test]
    fn response_format_requires_structured_outputs() {
        let mut p = bare_prompt();
        p.response_format = Some(ResponseFormat::JsonSchema {
            name: None,
            strict: None,
            schema: serde_json::json!({ "type": "object" }),
        });
        assert_eq!(
            p.required_capabilities(),
            vec![Capability::StructuredOutputs]
        );
    }

    #[test]
    fn tools_require_tools_capability() {
        let mut p = bare_prompt();
        p.tools = vec![Tool {
            name: "get_weather".into(),
            description: None,
            parameters: serde_json::json!({ "type": "object" }),
        }];
        assert_eq!(p.required_capabilities(), vec![Capability::Tools]);
    }

    #[test]
    fn reasoning_effort_requires_reasoning_capability() {
        let mut p = bare_prompt();
        p.params.reasoning_effort = Some("high".to_string());
        assert_eq!(p.required_capabilities(), vec![Capability::Reasoning]);
    }

    #[test]
    fn multiple_features_require_all_their_capabilities() {
        let mut p = bare_prompt();
        p.response_format = Some(ResponseFormat::JsonSchema {
            name: None,
            strict: None,
            schema: serde_json::json!({ "type": "object" }),
        });
        p.tools = vec![Tool {
            name: "t".into(),
            description: None,
            parameters: serde_json::json!({}),
        }];
        p.params.reasoning_effort = Some("low".to_string());
        let caps = p.required_capabilities();
        assert!(caps.contains(&Capability::StructuredOutputs));
        assert!(caps.contains(&Capability::Tools));
        assert!(caps.contains(&Capability::Reasoning));
        assert_eq!(caps.len(), 3);
    }

    fn user_file_prompt(media_type: &str) -> Prompt {
        let mut p = bare_prompt();
        p.messages = vec![Message {
            role: Role::User,
            content: vec![Content::File {
                media_type: media_type.to_string(),
                data: DataContent::Base64 {
                    data: "AAAA".to_string(),
                },
                filename: None,
                extra: Default::default(),
            }],
        }];
        p
    }

    #[test]
    fn image_file_requires_image_input() {
        assert_eq!(
            user_file_prompt("image/png").required_capabilities(),
            vec![Capability::ImageInput]
        );
    }

    #[test]
    fn audio_file_requires_audio_input() {
        assert_eq!(
            user_file_prompt("audio/mpeg").required_capabilities(),
            vec![Capability::AudioInput]
        );
    }

    #[test]
    fn video_file_requires_video_input() {
        assert_eq!(
            user_file_prompt("video/mp4").required_capabilities(),
            vec![Capability::VideoInput]
        );
    }

    #[test]
    fn document_file_requires_file_input() {
        assert_eq!(
            user_file_prompt("application/pdf").required_capabilities(),
            vec![Capability::FileInput]
        );
    }

    #[test]
    fn repeated_image_parts_dedupe_to_one_image_input() {
        let img = || Content::File {
            media_type: "image/png".to_string(),
            data: DataContent::Url {
                url: "https://example.invalid/a.png".to_string(),
            },
            filename: None,
            extra: Default::default(),
        };
        let mut p = bare_prompt();
        p.messages = vec![Message {
            role: Role::User,
            content: vec![img(), img()],
        }];
        assert_eq!(p.required_capabilities(), vec![Capability::ImageInput]);
    }

    #[test]
    fn response_modalities_require_output_capabilities() {
        let mut p = bare_prompt();
        p.params.response_modalities = vec![Modality::Text, Modality::Image, Modality::Audio];
        // Text implies no capability; image / audio each map to an output cap.
        assert_eq!(
            p.required_capabilities(),
            vec![Capability::ImageOutput, Capability::AudioOutput]
        );
    }

    #[test]
    fn file_content_serde_round_trips() {
        let original = Content::File {
            media_type: "image/png".to_string(),
            data: DataContent::Url {
                url: "https://example.invalid/y.png".to_string(),
            },
            filename: Some("y.png".to_string()),
            extra: Default::default(),
        };
        let value = serde_json::to_value(&original).unwrap();
        assert_eq!(value["type"], "file");
        assert_eq!(value["media_type"], "image/png");
        assert_eq!(value["data"]["kind"], "url");
        assert_eq!(value["data"]["url"], "https://example.invalid/y.png");
        let back: Content = serde_json::from_value(value).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn capability_token_matches_registry_vocabulary() {
        // These tokens must stay byte-identical to the registry Zod enum and
        // the cloud's imported `Capability` — they are the cross-repo contract.
        assert_eq!(Capability::StructuredOutputs.as_str(), "structured_outputs");
        assert_eq!(Capability::Tools.as_str(), "tools");
        assert_eq!(Capability::Reasoning.as_str(), "reasoning");
        assert_eq!(Capability::WebSearch.as_str(), "web_search");
        assert_eq!(Capability::Logprobs.as_str(), "logprobs");
        assert_eq!(Capability::ImageInput.as_str(), "image_input");
        assert_eq!(Capability::AudioInput.as_str(), "audio_input");
        assert_eq!(Capability::VideoInput.as_str(), "video_input");
        assert_eq!(Capability::FileInput.as_str(), "file_input");
        assert_eq!(Capability::ImageOutput.as_str(), "image_output");
        assert_eq!(Capability::AudioOutput.as_str(), "audio_output");
    }

    #[test]
    fn extra_only_features_are_not_auto_detected() {
        // web_search / logprobs ride `params.extra` and are advertise-only (not
        // gated) — a request carrying them must still require no capabilities.
        let mut p = bare_prompt();
        p.params
            .extra
            .insert("logprobs".into(), serde_json::json!(true));
        p.params
            .extra
            .insert("web_search_options".into(), serde_json::json!({}));
        assert!(p.required_capabilities().is_empty());
    }
}
