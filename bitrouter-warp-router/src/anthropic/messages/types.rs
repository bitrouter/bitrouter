//! Request and response types for the Anthropic Messages API format.

use serde::{Deserialize, Serialize};

// ── Request ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    pub max_tokens: u32,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub top_k: Option<u32>,
    #[serde(default)]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    pub stream: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<AnthropicMessageContent>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum AnthropicMessageContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
}

// ── Response ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct MessagesResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub response_type: String,
    pub role: String,
    pub content: Vec<MessagesResponseContent>,
    pub model: String,
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    pub usage: MessagesUsage,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessagesResponseContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessagesUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

// ── Streaming ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct MessagesStreamEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<MessagesStreamDelta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<MessagesStreamMessage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessagesStreamDelta {
    #[serde(rename = "type")]
    pub delta_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessagesStreamMessage {
    pub id: String,
    pub model: String,
    pub role: String,
}
