//! Public chat completions request contracts.

use crate::types::{ChatStreamOptions, ReasoningEffort};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Chat Completions request body
/// (<https://platform.openai.com/docs/api-reference/chat/create>).
///
/// Public so clients and extensions can derive an
/// OpenAPI schema from the canonical wire shape without redeclaring it.
#[derive(Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(transform = super::omit_serialized_default))]
    pub tools: Vec<ChatTool>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub temperature: Option<f64>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub top_p: Option<f64>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub max_completion_tokens: Option<u32>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Output-format constraint. The `json_schema` variant is the
    /// structured-output contract; `json_object` is the legacy JSON mode and
    /// `text` is the default. Promoted to the canonical `response_format` slot
    /// at parse time (`json_object` / `text` pass through opaquely).
    /// <https://platform.openai.com/docs/guides/structured-outputs>
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub response_format: Option<ChatResponseFormat>,
    /// Deterministic-sampling seed. Promoted to the canonical `seed` slot so it
    /// translates across protocols (e.g. to a Gemini upstream's
    /// `generationConfig.seed`) instead of no-op'ing as a top-level key on a
    /// nested-config wire.
    /// <https://platform.openai.com/docs/api-reference/chat/create#chat-create-seed>
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub seed: Option<i64>,
    /// Stop sequences — a single string or an array of up to four. Promoted to
    /// the canonical `stop` slot.
    /// <https://platform.openai.com/docs/api-reference/chat/create#chat-create-stop>
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub stop: Option<serde_json::Value>,
    /// Presence penalty. Promoted to the canonical `presence_penalty` slot.
    /// <https://platform.openai.com/docs/api-reference/chat/create#chat-create-presence_penalty>
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub presence_penalty: Option<f64>,
    /// Frequency penalty. Promoted to the canonical `frequency_penalty` slot.
    /// <https://platform.openai.com/docs/api-reference/chat/create#chat-create-frequency_penalty>
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub frequency_penalty: Option<f64>,
    #[serde(default)]
    pub stream: bool,
    /// Whether OpenAI may retain the request and response.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub store: Option<bool>,
    /// Whether the model may issue tool calls in parallel.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub parallel_tool_calls: Option<bool>,
    /// Streaming response controls.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub stream_options: Option<ChatStreamOptions>,
    /// Every other field — `tool_choice`, `n`, `logit_bias`, `logprobs`,
    /// `top_logprobs`, `user`, … —
    /// survives parse/render via `extra`. v0 passed these through; v1 must too.
    /// Skipped from the published schema so the documented contract is the set
    /// of typed fields; pass-through behavior is preserved at runtime.
    #[serde(flatten)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Chat Completions `response_format`
/// (<https://platform.openai.com/docs/guides/structured-outputs>) — a closed
/// union over OpenAI's three output modes.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatResponseFormat {
    /// Free-form text — the default.
    Text,
    /// Legacy JSON mode: valid JSON with no schema.
    JsonObject,
    /// JSON constrained to a schema.
    JsonSchema {
        /// The schema spec.
        json_schema: ChatJsonSchema,
    },
}

/// The `json_schema` payload of [`ChatResponseFormat::JsonSchema`].
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatJsonSchema {
    /// Schema name (OpenAI requires it).
    pub name: String,
    /// Optional schema description — extra LLM guidance OpenAI passes through to
    /// the model. Promoted to the canonical `response_format` description.
    /// <https://platform.openai.com/docs/guides/structured-outputs>
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Strict-mode flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    /// The JSON Schema.
    pub schema: serde_json::Value,
    /// Unmodelled wire fields retained for service validation and round trips.
    #[serde(flatten)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub extra: HashMap<String, serde_json::Value>,
}

/// One element of [`ChatRequest`]'s `messages` array — a chat turn carrying
/// role + optional content + optional tool calls / tool-call reply id.
#[derive(Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatMessage {
    pub role: String,
    /// `content` may be a plain string or an array of content parts, or absent
    /// (an assistant turn that is purely `tool_calls`).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub content: Option<serde_json::Value>,
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(transform = super::omit_serialized_default))]
    pub tool_calls: Vec<ChatToolCall>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub tool_call_id: Option<String>,
    /// Reasoning content — **not** in OpenAI's Chat Completions spec; this is
    /// a vendor extension used by DeepSeek
    /// (<https://api-docs.deepseek.com/api/create-chat-completion>), Moonshot
    /// (Kimi), Qwen, and other OpenAI-compatible providers that expose the
    /// model's chain-of-thought on the Chat envelope. v0 #454-1: it must
    /// not be dropped. `parse_response` additionally accepts `reasoning`
    /// (some OpenRouter passthroughs) and `thinking` (Aliyun) as aliases.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub reasoning_content: Option<String>,
    /// Unmodelled wire fields retained for service validation and round trips.
    #[serde(flatten)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub extra: HashMap<String, serde_json::Value>,
}

/// One assistant tool-call entry on a [`ChatMessage`].
#[derive(Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatToolCall {
    pub id: String,
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(transform = super::omit_serialized_default))]
    pub function: ChatFunctionCall,
    /// Provider continuity extensions attached to this specific call.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub extra_content: Option<serde_json::Value>,
    /// Unmodelled wire fields retained for service validation and round trips.
    #[serde(flatten)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub extra: HashMap<String, serde_json::Value>,
}

/// The `function` payload of a [`ChatToolCall`]: name + raw JSON argument string.
#[derive(Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatFunctionCall {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub arguments: String,
    /// Unmodelled wire fields retained for service validation and round trips.
    #[serde(flatten)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub extra: HashMap<String, serde_json::Value>,
}

/// One element of [`ChatRequest`]'s `tools` array — a `{ function: { … } }` envelope.
#[derive(Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatTool {
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(transform = super::omit_serialized_default))]
    pub function: ChatToolFunction,
    /// Unmodelled wire fields retained for service validation and round trips.
    #[serde(flatten)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub extra: HashMap<String, serde_json::Value>,
}

/// The `function` payload of a [`ChatTool`]: name + description + JSON-Schema
/// parameters + optional `strict` flag.
/// <https://platform.openai.com/docs/guides/function-calling#strict-mode>
#[derive(Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatToolFunction {
    pub name: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: serde_json::Value,
    /// OpenAI strict-mode flag (V3 `strict`). Captured so it is not lost across
    /// the canonical boundary.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub strict: Option<bool>,
    /// Unmodelled wire fields retained for service validation and round trips.
    #[serde(flatten)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub extra: HashMap<String, serde_json::Value>,
}

impl std::fmt::Debug for ChatToolCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatToolCall")
            .field("id", &self.id)
            .field("function", &self.function)
            .field("extra_content", &"<redacted>")
            .finish()
    }
}

impl std::fmt::Debug for ChatMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatMessage")
            .field("role", &self.role)
            .field("content", &self.content)
            .field("tool_calls", &self.tool_calls)
            .field("tool_call_id", &self.tool_call_id)
            .field("reasoning_content", &self.reasoning_content)
            .field("extra", &"<redacted>")
            .finish()
    }
}
