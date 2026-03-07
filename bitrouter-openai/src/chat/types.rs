use std::collections::HashMap;

use bitrouter_core::{
    errors::{BitrouterError, Result},
    models::{
        language::{
            call_options::LanguageModelResponseFormat,
            finish_reason::LanguageModelFinishReason,
            tool::LanguageModelTool,
            tool_choice::LanguageModelToolChoice,
            usage::{LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage},
        },
        shared::{provider::ProviderMetadata, types::JsonValue},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub(super) const OPENAI_PROVIDER_NAME: &str = "openai";
pub(super) const STREAM_TEXT_ID: &str = "text";

// ── Response types ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChatCompletionResponse {
    pub id: String,
    pub choices: Vec<OpenAiChatCompletionChoice>,
    pub created: i64,
    pub model: String,
    #[serde(default)]
    pub system_fingerprint: Option<String>,
    #[serde(default)]
    pub usage: Option<OpenAiUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChatCompletionChoice {
    pub index: u32,
    pub message: OpenAiChatCompletionMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChatCompletionMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub refusal: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<OpenAiMessageToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiMessageToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: OpenAiToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiToolFunction {
    pub name: String,
    pub arguments: String,
}

// ── Chunk / streaming response types ────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChatCompletionChunk {
    pub id: String,
    pub choices: Vec<OpenAiChatCompletionChunkChoice>,
    pub created: i64,
    pub model: String,
    #[serde(default)]
    pub usage: Option<OpenAiUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChatCompletionChunkChoice {
    pub index: u32,
    #[serde(default)]
    pub delta: OpenAiChunkDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpenAiChunkDelta {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<OpenAiChunkDeltaToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChunkDeltaToolCall {
    pub index: u32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub function: Option<OpenAiChunkDeltaToolFunction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChunkDeltaToolFunction {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

// ── Usage types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiUsage {
    #[serde(default)]
    pub prompt_tokens: Option<u32>,
    #[serde(default)]
    pub completion_tokens: Option<u32>,
    #[serde(default)]
    pub total_tokens: Option<u32>,
    #[serde(default)]
    pub prompt_tokens_details: Option<OpenAiPromptTokensDetails>,
    #[serde(default)]
    pub completion_tokens_details: Option<OpenAiCompletionTokensDetails>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiPromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiCompletionTokensDetails {
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
}

impl From<OpenAiUsage> for LanguageModelUsage {
    fn from(usage: OpenAiUsage) -> Self {
        let raw = serde_json::to_value(&usage).ok();
        let reasoning_tokens = usage
            .completion_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens);
        LanguageModelUsage {
            input_tokens: LanguageModelInputTokens {
                total: usage.prompt_tokens,
                no_cache: usage
                    .prompt_tokens_details
                    .as_ref()
                    .and_then(|d| d.cached_tokens)
                    .map(|cached| usage.prompt_tokens.unwrap_or(cached).saturating_sub(cached)),
                cache_read: usage
                    .prompt_tokens_details
                    .as_ref()
                    .and_then(|d| d.cached_tokens),
                cache_write: None,
            },
            output_tokens: LanguageModelOutputTokens {
                total: usage.completion_tokens,
                text: usage.completion_tokens,
                reasoning: reasoning_tokens,
            },
            raw,
        }
    }
}

// ── Error types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiErrorEnvelope {
    pub error: OpenAiApiError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiApiError {
    pub message: String,
    #[serde(rename = "type", default)]
    pub error_type: Option<String>,
    #[serde(default)]
    pub param: Option<String>,
    #[serde(default)]
    pub code: Option<JsonValue>,
}

// ── Request types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChatCompletionsRequest {
    pub model: String,
    pub messages: Vec<OpenAiChatMessageParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<OpenAiChatCompletionStreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<OpenAiResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<OpenAiChatTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<OpenAiToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum OpenAiChatMessageParam {
    System {
        content: String,
    },
    User {
        content: OpenAiUserMessageContent,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<OpenAiChatToolCall>>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OpenAiUserMessageContent {
    Text(String),
    Parts(Vec<OpenAiInputContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OpenAiInputContentPart {
    Text { text: String },
    ImageUrl { image_url: OpenAiImageUrl },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiImageUrl {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChatCompletionStreamOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_usage: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OpenAiResponseFormat {
    Text,
    JsonObject,
    JsonSchema { json_schema: OpenAiJsonSchemaConfig },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiJsonSchemaConfig {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub schema: schemars::Schema,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChatTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: OpenAiChatToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChatToolFunction {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: schemars::Schema,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OpenAiToolChoice {
    Mode(String),
    Named {
        #[serde(rename = "type")]
        kind: String,
        function: OpenAiNamedToolChoice,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiNamedToolChoice {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChatToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: OpenAiChatToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChatToolCallFunction {
    pub name: String,
    pub arguments: String,
}

// ── From / TryFrom conversions ──────────────────────────────────────────────

impl From<&LanguageModelToolChoice> for OpenAiToolChoice {
    fn from(choice: &LanguageModelToolChoice) -> Self {
        match choice {
            LanguageModelToolChoice::Auto => OpenAiToolChoice::Mode("auto".to_owned()),
            LanguageModelToolChoice::None => OpenAiToolChoice::Mode("none".to_owned()),
            LanguageModelToolChoice::Required => OpenAiToolChoice::Mode("required".to_owned()),
            LanguageModelToolChoice::Tool { tool_name } => OpenAiToolChoice::Named {
                kind: "function".to_owned(),
                function: OpenAiNamedToolChoice {
                    name: tool_name.clone(),
                },
            },
        }
    }
}

impl From<&LanguageModelResponseFormat> for OpenAiResponseFormat {
    fn from(format: &LanguageModelResponseFormat) -> Self {
        match format {
            LanguageModelResponseFormat::Text => OpenAiResponseFormat::Text,
            LanguageModelResponseFormat::Json {
                schema,
                name,
                description,
            } => match schema {
                Some(schema) => OpenAiResponseFormat::JsonSchema {
                    json_schema: OpenAiJsonSchemaConfig {
                        name: name.clone().unwrap_or_else(|| "output".to_owned()),
                        description: description.clone(),
                        schema: schema.clone(),
                        strict: Some(true),
                    },
                },
                None => OpenAiResponseFormat::JsonObject,
            },
        }
    }
}

impl TryFrom<&LanguageModelTool> for OpenAiChatTool {
    type Error = BitrouterError;

    fn try_from(tool: &LanguageModelTool) -> Result<Self> {
        match tool {
            LanguageModelTool::Function {
                name,
                description,
                input_schema,
                strict,
                ..
            } => Ok(OpenAiChatTool {
                kind: "function".to_owned(),
                function: OpenAiChatToolFunction {
                    name: name.clone(),
                    description: description.clone(),
                    parameters: input_schema.clone(),
                    strict: *strict,
                },
            }),
            LanguageModelTool::Provider { id, .. } => Err(BitrouterError::unsupported(
                OPENAI_PROVIDER_NAME,
                format!("provider tool {}:{}", id.provider_name, id.tool_id),
                Some(
                    "OpenAI chat completions supports function and custom tools, \
                     but bitrouter-core provider tools do not map cleanly here"
                        .to_owned(),
                ),
            )),
        }
    }
}

// ── Helper functions ────────────────────────────────────────────────────────

pub(super) fn map_finish_reason(finish_reason: Option<&str>) -> LanguageModelFinishReason {
    match finish_reason {
        Some("stop") | None => LanguageModelFinishReason::Stop,
        Some("length") => LanguageModelFinishReason::Length,
        Some("tool_calls") | Some("function_call") => LanguageModelFinishReason::FunctionCall,
        Some("content_filter") => LanguageModelFinishReason::ContentFilter,
        Some("error") => LanguageModelFinishReason::Error,
        Some(other) => LanguageModelFinishReason::Other(other.to_owned()),
    }
}

pub(super) fn openai_metadata(
    system_fingerprint: Option<String>,
    refusal: Option<String>,
) -> Option<ProviderMetadata> {
    let mut inner = HashMap::new();
    if let Some(system_fingerprint) = system_fingerprint {
        inner.insert(
            "system_fingerprint".to_owned(),
            JsonValue::String(system_fingerprint),
        );
    }
    if let Some(refusal) = refusal {
        inner.insert("refusal".to_owned(), JsonValue::String(refusal));
    }

    if inner.is_empty() {
        None
    } else {
        Some(HashMap::from([(
            OPENAI_PROVIDER_NAME.to_owned(),
            json!(inner),
        )]))
    }
}

pub(super) fn empty_usage() -> LanguageModelUsage {
    LanguageModelUsage {
        input_tokens: LanguageModelInputTokens {
            total: None,
            no_cache: None,
            cache_read: None,
            cache_write: None,
        },
        output_tokens: LanguageModelOutputTokens {
            total: None,
            text: None,
            reasoning: None,
        },
        raw: None,
    }
}

pub(super) fn json_value_to_string(value: JsonValue) -> Option<String> {
    match value {
        JsonValue::String(value) => Some(value),
        JsonValue::Number(value) => Some(value.to_string()),
        JsonValue::Bool(value) => Some(value.to_string()),
        JsonValue::Null => None,
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_tool_finish_reason() {
        assert_eq!(
            map_finish_reason(Some("tool_calls")),
            LanguageModelFinishReason::FunctionCall
        );
    }
}
