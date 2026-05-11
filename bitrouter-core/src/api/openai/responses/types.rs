//! Request and response types for the OpenAI Responses API format.
//!
//! All types carry both `Serialize` and `Deserialize` so they can be used
//! bidirectionally — the proxy layer deserialises incoming client requests and
//! serialises outgoing responses, while the provider layer serialises upstream
//! requests and deserialises upstream responses.

use serde::{Deserialize, Serialize};

// ── Request ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    pub input: ResponsesInput,
    /// Top-level developer prompt (Codex CLI sends the entire system prompt
    /// here rather than as a system message). Mapped to a System message at
    /// the head of the prompt in `to_call_options`.
    /// <https://platform.openai.com/docs/api-reference/responses/create#responses-create-instructions>
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ResponsesTool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ResponsesToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<ResponsesTextConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesTool {
    pub r#type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesInput {
    Text(String),
    Items(Vec<ResponsesInputItem>),
}

/// Input items for the Responses API.
///
/// Uses `#[serde(untagged)]` so proxy clients can omit the `"type"` field on
/// message items. Variants are ordered most-specific-first so serde tries the
/// variant with more required fields before the less specific one.
///
/// Each inner struct carries a `type` field with a default value so that
/// serialisation always includes the discriminator required by the upstream API.
///
/// The final `Unknown` variant is a permissive catch-all that lets unfamiliar
/// item types (e.g. `reasoning`, `web_search_call`, `local_shell_call`,
/// `image_generation_call`) deserialize without rejecting the whole request.
/// Codex CLI surfaces all of these in `input` on multi-turn requests; the
/// upstream models bitrouter routes to can't consume them, so we accept and
/// silently drop them downstream.
/// <https://platform.openai.com/docs/api-reference/responses/create>
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesInputItem {
    FunctionCall(ResponsesInputFunctionCall),
    FunctionCallOutput(ResponsesInputFunctionCallOutput),
    Message(ResponsesInputMessage),
    Unknown(serde_json::Value),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesInputFunctionCall {
    #[serde(rename = "type", default = "default_function_call_type")]
    pub item_type: String,
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

fn default_function_call_type() -> String {
    "function_call".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesInputFunctionCallOutput {
    #[serde(rename = "type", default = "default_function_call_output_type")]
    pub item_type: String,
    pub call_id: String,
    /// Wire encoding may be either a plain string or an object with
    /// `content` / `content_items` (the structured form Codex sends for
    /// multimodal tool outputs). We accept any JSON value here and stringify
    /// downstream so the routed model receives a plain-text representation.
    /// <https://platform.openai.com/docs/api-reference/responses/create#input>
    pub output: serde_json::Value,
}

fn default_function_call_output_type() -> String {
    "function_call_output".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesInputMessage {
    #[serde(rename = "type", default = "default_message_type")]
    pub item_type: String,
    pub role: String,
    #[serde(default)]
    pub content: Option<ResponsesInputContent>,
}

fn default_message_type() -> String {
    "message".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesInputContent {
    Text(String),
    Parts(Vec<ResponsesInputContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesInputContentPart {
    InputText { text: String },
    OutputText { text: String },
    InputImage { image_url: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesToolChoice {
    Mode(String),
    Named {
        #[serde(rename = "type")]
        kind: String,
        name: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesTextConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<ResponsesTextFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesTextFormat {
    Text,
    JsonObject,
    JsonSchema {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        schema: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },
}

// ── Response ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesResponse {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    pub created_at: i64,
    pub model: String,
    #[serde(default)]
    pub output: Vec<ResponsesOutputItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ResponsesUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incomplete_details: Option<ResponsesIncompleteDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponsesApiError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesOutputItem {
    Message {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        content: Vec<ResponsesOutputContent>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
    FunctionCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        call_id: String,
        name: String,
        arguments: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesOutputContent {
    OutputText {
        text: String,
    },
    Refusal {
        refusal: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesUsage {
    // Strict consumers (Codex CLI) require these three fields as non-null
    // integers when the `usage` object is present — they're typed `i64` in
    // codex-rs/codex-api/src/sse/responses.rs::ResponseCompletedUsage and
    // any `null` value fails the response.completed parse. Skip-on-None so
    // the streaming converter can construct a partial usage object only
    // when at least one field is known; in practice the converter always
    // sets all three together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens_details: Option<ResponsesInputTokensDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens_details: Option<ResponsesOutputTokensDetails>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesInputTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesOutputTokensDetails {
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesIncompleteDetails {
    #[serde(default)]
    pub reason: Option<String>,
}

// ── Error ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesApiError {
    pub message: String,
    #[serde(rename = "type", default)]
    pub error_type: Option<String>,
    #[serde(default)]
    pub param: Option<String>,
    #[serde(default)]
    pub code: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesErrorEnvelope {
    pub error: ResponsesApiError,
}

// ── Streaming ───────────────────────────────────────────────────────────────

/// Flat SSE event structure used by the proxy layer to emit Responses-format
/// server-sent events to the client.  This is **not** the same as the
/// provider's inbound stream-event enum which parses upstream SSE.
///
/// The Responses streaming protocol assigns a monotonic `sequence_number` to
/// every event and wraps the start/end of the stream in `response.created` /
/// `response.completed` envelopes that carry the full response object. Codex
/// and other strict clients reject streams that omit these.
/// <https://platform.openai.com/docs/api-reference/responses-streaming>
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesStreamEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    /// Monotonic event index within the stream; required on every event.
    pub sequence_number: u64,
    /// Full response payload — present on `response.created`,
    /// `response.in_progress`, `response.completed`, `response.failed`,
    /// `response.incomplete`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponsesResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_index: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_index: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
    /// Final authoritative text on `response.output_text.done` /
    /// `response.reasoning_text.done`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    /// Item payload — present on `response.output_item.added`/`done`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<serde_json::Value>,
    /// Content part payload — present on `response.content_part.added`/`done`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part: Option<serde_json::Value>,
}
