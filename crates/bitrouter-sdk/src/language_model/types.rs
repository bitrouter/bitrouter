//! Core data types for the `language_model` protocol: the canonical internal
//! representation (`Prompt` / `StreamPart` / `GenerateResult`) plus routing and
//! pipeline I/O types.
//!
//! These are deliberately minimal in Phase 1 — Phase 2 fills in the full
//! protocol-conversion surface (tool calls, reasoning variants, content blocks).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::caller::CallerContext;

/// The wire protocol an upstream provider speaks. v0 had
/// `Openai/Anthropic/Google/Mcp/Rest/Acp`; v1 promotes OpenAI Responses to its
/// own first-class variant (see 005).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
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
    /// Reasoning tokens (subset of completion on most providers).
    pub reasoning_tokens: u64,
}

impl Usage {
    /// Total tokens (prompt + completion).
    pub fn total(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

/// Why generation stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// and status that a bare [`StreamPart::Finish`] would lose (005 §2.3).
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
    /// The generation result.
    pub result: GenerateResult,
    /// End-to-end latency in milliseconds.
    pub latency_ms: u64,
    /// Upstream generation time in milliseconds.
    pub generation_time_ms: u64,
}

/// One hop in a fallback chain: a concrete provider + model + connection info.
#[derive(Debug, Clone)]
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
    /// BYOK key override. Set by `ByokRouteHook`. **Never** used to infer
    /// `byok_used` — that signal comes from the `ByokKeyApplied` event
    /// (cloud #235 lesson).
    pub api_key_override: Option<String>,
    /// BYOK api-base override, paired with `api_key_override`.
    pub api_base_override: Option<String>,
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
    /// Final charge in micro-USD (0 for BYOK / unsettled).
    pub final_charge_micro_usd: i64,
}
