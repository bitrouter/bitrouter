//! Request and response types for the OpenAI Responses API format.

use serde::{Deserialize, Serialize};

// ── Request ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    pub input: ResponsesInput,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub stream: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ResponsesInput {
    Text(String),
    Messages(Vec<ResponsesInputMessage>),
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesInputMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<ResponsesInputContent>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ResponsesInputContent {
    Text(String),
    Parts(Vec<ResponsesInputContentPart>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ResponsesInputContentPart {
    #[serde(rename = "input_text")]
    InputText { text: String },
}

// ── Response ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ResponsesResponse {
    pub id: String,
    pub object: String,
    pub created_at: i64,
    pub model: String,
    pub output: Vec<ResponsesOutputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<ResponsesUsage>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponsesOutputItem {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String,
    pub role: String,
    pub content: Vec<ResponsesOutputContent>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponsesOutputContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponsesUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
}

// ── Streaming ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
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
}
