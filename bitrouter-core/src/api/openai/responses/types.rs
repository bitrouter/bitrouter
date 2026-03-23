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
/// message items.  Variants are ordered most-specific-first so serde tries the
/// variant with more required fields before the less specific one.
///
/// Each inner struct carries a `type` field with a default value so that
/// serialisation always includes the discriminator required by the upstream API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesInputItem {
    FunctionCall(ResponsesInputFunctionCall),
    FunctionCallOutput(ResponsesInputFunctionCallOutput),
    Message(ResponsesInputMessage),
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
    pub output: String,
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
    #[serde(default)]
    pub input_tokens: Option<u32>,
    #[serde(default)]
    pub output_tokens: Option<u32>,
    #[serde(default)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesStreamEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_index: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_index: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<serde_json::Value>,
}
