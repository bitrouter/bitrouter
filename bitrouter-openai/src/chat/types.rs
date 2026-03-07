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
    use bitrouter_core::models::language::usage::LanguageModelUsage;

    #[test]
    fn maps_tool_finish_reason() {
        assert_eq!(
            map_finish_reason(Some("tool_calls")),
            LanguageModelFinishReason::FunctionCall
        );
    }

    #[test]
    fn maps_all_finish_reasons() {
        assert_eq!(
            map_finish_reason(Some("stop")),
            LanguageModelFinishReason::Stop
        );
        assert_eq!(map_finish_reason(None), LanguageModelFinishReason::Stop);
        assert_eq!(
            map_finish_reason(Some("length")),
            LanguageModelFinishReason::Length
        );
        assert_eq!(
            map_finish_reason(Some("function_call")),
            LanguageModelFinishReason::FunctionCall
        );
        assert_eq!(
            map_finish_reason(Some("content_filter")),
            LanguageModelFinishReason::ContentFilter
        );
        assert_eq!(
            map_finish_reason(Some("error")),
            LanguageModelFinishReason::Error
        );
        assert_eq!(
            map_finish_reason(Some("unknown")),
            LanguageModelFinishReason::Other("unknown".to_owned())
        );
    }

    #[test]
    fn deserializes_chat_completion_response() {
        let raw = json!({
            "id": "chatcmpl-abc123",
            "object": "chat.completion",
            "created": 1710000000,
            "model": "gpt-4o-2024-05-13",
            "system_fingerprint": "fp_abc123",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello! How can I help you today?"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 8,
                "total_tokens": 18
            }
        });

        let response: OpenAiChatCompletionResponse =
            serde_json::from_value(raw).expect("should deserialize");
        assert_eq!(response.id, "chatcmpl-abc123");
        assert_eq!(response.model, "gpt-4o-2024-05-13");
        assert_eq!(response.system_fingerprint.as_deref(), Some("fp_abc123"));
        assert_eq!(response.choices.len(), 1);
        assert_eq!(
            response.choices[0].message.content.as_deref(),
            Some("Hello! How can I help you today?")
        );
        assert_eq!(response.choices[0].finish_reason.as_deref(), Some("stop"));
        let usage = response.usage.as_ref().unwrap();
        assert_eq!(usage.prompt_tokens, Some(10));
        assert_eq!(usage.completion_tokens, Some(8));
    }

    #[test]
    fn deserializes_tool_call_response() {
        let raw = json!({
            "id": "chatcmpl-tool456",
            "created": 1710000001,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"location\":\"Hong Kong\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 50,
                "completion_tokens": 20,
                "total_tokens": 70
            }
        });

        let response: OpenAiChatCompletionResponse =
            serde_json::from_value(raw).expect("should deserialize");
        assert!(response.choices[0].message.content.is_none());
        let tool_calls = response.choices[0]
            .message
            .tool_calls
            .as_ref()
            .expect("should have tool_calls");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_abc");
        assert_eq!(tool_calls[0].kind, "function");
        assert_eq!(tool_calls[0].function.name, "get_weather");
        assert_eq!(
            tool_calls[0].function.arguments,
            "{\"location\":\"Hong Kong\"}"
        );
    }

    #[test]
    fn deserializes_streaming_chunk_with_text_delta() {
        let raw = json!({
            "id": "chatcmpl-stream",
            "object": "chat.completion.chunk",
            "created": 1710000002,
            "model": "gpt-4o-mini",
            "choices": [{
                "index": 0,
                "delta": {
                    "content": "Hello"
                },
                "finish_reason": null
            }]
        });

        let chunk: OpenAiChatCompletionChunk =
            serde_json::from_value(raw).expect("should deserialize");
        assert_eq!(chunk.id, "chatcmpl-stream");
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("Hello"));
        assert!(chunk.choices[0].finish_reason.is_none());
    }

    #[test]
    fn deserializes_streaming_chunk_with_tool_call_delta() {
        let raw = json!({
            "id": "chatcmpl-stream2",
            "created": 1710000003,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_xyz",
                        "type": "function",
                        "function": {
                            "name": "search",
                            "arguments": "{\"q\":"
                        }
                    }]
                },
                "finish_reason": null
            }]
        });

        let chunk: OpenAiChatCompletionChunk =
            serde_json::from_value(raw).expect("should deserialize");
        let tool_calls = chunk.choices[0]
            .delta
            .tool_calls
            .as_ref()
            .expect("should have tool_calls");
        assert_eq!(tool_calls[0].index, 0);
        assert_eq!(tool_calls[0].id.as_deref(), Some("call_xyz"));
        let func = tool_calls[0].function.as_ref().unwrap();
        assert_eq!(func.name.as_deref(), Some("search"));
        assert_eq!(func.arguments.as_deref(), Some("{\"q\":"));
    }

    #[test]
    fn deserializes_error_envelope() {
        let raw = json!({
            "error": {
                "message": "Rate limit exceeded",
                "type": "rate_limit_error",
                "param": null,
                "code": "rate_limit_exceeded"
            }
        });

        let envelope: OpenAiErrorEnvelope =
            serde_json::from_value(raw).expect("should deserialize");
        assert_eq!(envelope.error.message, "Rate limit exceeded");
        assert_eq!(
            envelope.error.error_type.as_deref(),
            Some("rate_limit_error")
        );
        assert!(envelope.error.param.is_none());
        assert_eq!(
            envelope.error.code,
            Some(JsonValue::String("rate_limit_exceeded".to_owned()))
        );
    }

    #[test]
    fn deserializes_usage_with_cache_and_reasoning_details() {
        let raw = json!({
            "prompt_tokens": 200,
            "completion_tokens": 150,
            "total_tokens": 350,
            "prompt_tokens_details": {
                "cached_tokens": 50
            },
            "completion_tokens_details": {
                "reasoning_tokens": 30
            }
        });

        let usage: OpenAiUsage = serde_json::from_value(raw).expect("should deserialize");
        assert_eq!(usage.prompt_tokens, Some(200));
        assert_eq!(usage.completion_tokens, Some(150));
        assert_eq!(usage.total_tokens, Some(350));
        assert_eq!(
            usage.prompt_tokens_details.as_ref().unwrap().cached_tokens,
            Some(50)
        );
        assert_eq!(
            usage
                .completion_tokens_details
                .as_ref()
                .unwrap()
                .reasoning_tokens,
            Some(30)
        );
    }

    #[test]
    fn from_usage_computes_cache_and_reasoning() {
        let usage = OpenAiUsage {
            prompt_tokens: Some(200),
            completion_tokens: Some(150),
            total_tokens: Some(350),
            prompt_tokens_details: Some(OpenAiPromptTokensDetails {
                cached_tokens: Some(50),
            }),
            completion_tokens_details: Some(OpenAiCompletionTokensDetails {
                reasoning_tokens: Some(30),
            }),
        };

        let converted: LanguageModelUsage = usage.into();
        assert_eq!(converted.input_tokens.total, Some(200));
        assert_eq!(converted.input_tokens.no_cache, Some(150)); // 200 - 50
        assert_eq!(converted.input_tokens.cache_read, Some(50));
        assert_eq!(converted.output_tokens.total, Some(150));
        assert_eq!(converted.output_tokens.reasoning, Some(30));
        assert!(converted.raw.is_some());
    }

    #[test]
    fn serializes_request_round_trip() {
        let request = OpenAiChatCompletionsRequest {
            model: "gpt-4o".to_owned(),
            messages: vec![
                OpenAiChatMessageParam::System {
                    content: "You are helpful.".to_owned(),
                },
                OpenAiChatMessageParam::User {
                    content: OpenAiUserMessageContent::Text("Hi".to_owned()),
                },
            ],
            stream: Some(false),
            stream_options: None,
            max_completion_tokens: Some(1024),
            temperature: Some(0.7),
            top_p: None,
            stop: None,
            presence_penalty: None,
            frequency_penalty: None,
            response_format: None,
            seed: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
        };

        let json_val = serde_json::to_value(&request).expect("should serialize");
        assert_eq!(json_val["model"], "gpt-4o");
        assert_eq!(json_val["max_completion_tokens"], 1024);
        assert!(json_val["temperature"].as_f64().unwrap() - 0.7 < 0.001);
        // Optional None fields should be absent (skip_serializing_if)
        assert!(json_val.get("top_p").is_none());
        assert!(json_val.get("tools").is_none());

        // Round-trip
        let deserialized: OpenAiChatCompletionsRequest =
            serde_json::from_value(json_val).expect("should deserialize");
        assert_eq!(deserialized.model, "gpt-4o");
        assert_eq!(deserialized.max_completion_tokens, Some(1024));
    }

    #[test]
    fn serializes_message_param_roles() {
        let system = OpenAiChatMessageParam::System {
            content: "system prompt".to_owned(),
        };
        let val = serde_json::to_value(&system).unwrap();
        assert_eq!(val["role"], "system");
        assert_eq!(val["content"], "system prompt");

        let tool = OpenAiChatMessageParam::Tool {
            tool_call_id: "call_123".to_owned(),
            content: "result".to_owned(),
        };
        let val = serde_json::to_value(&tool).unwrap();
        assert_eq!(val["role"], "tool");
        assert_eq!(val["tool_call_id"], "call_123");
    }

    #[test]
    fn from_tool_choice_variants() {
        use bitrouter_core::models::language::tool_choice::LanguageModelToolChoice;

        let auto = OpenAiToolChoice::from(&LanguageModelToolChoice::Auto);
        assert!(matches!(auto, OpenAiToolChoice::Mode(ref m) if m == "auto"));

        let none = OpenAiToolChoice::from(&LanguageModelToolChoice::None);
        assert!(matches!(none, OpenAiToolChoice::Mode(ref m) if m == "none"));

        let required = OpenAiToolChoice::from(&LanguageModelToolChoice::Required);
        assert!(matches!(required, OpenAiToolChoice::Mode(ref m) if m == "required"));

        let named = OpenAiToolChoice::from(&LanguageModelToolChoice::Tool {
            tool_name: "my_func".to_owned(),
        });
        match named {
            OpenAiToolChoice::Named { kind, function } => {
                assert_eq!(kind, "function");
                assert_eq!(function.name, "my_func");
            }
            other => panic!("expected Named, got {other:?}"),
        }
    }

    #[test]
    fn from_response_format_variants() {
        use bitrouter_core::models::language::call_options::LanguageModelResponseFormat;

        let text = OpenAiResponseFormat::from(&LanguageModelResponseFormat::Text);
        assert!(matches!(text, OpenAiResponseFormat::Text));

        let json_obj = OpenAiResponseFormat::from(&LanguageModelResponseFormat::Json {
            schema: None,
            name: None,
            description: None,
        });
        assert!(matches!(json_obj, OpenAiResponseFormat::JsonObject));

        let json_schema = OpenAiResponseFormat::from(&LanguageModelResponseFormat::Json {
            schema: Some(schemars::Schema::default()),
            name: Some("output".to_owned()),
            description: Some("test".to_owned()),
        });
        match json_schema {
            OpenAiResponseFormat::JsonSchema { json_schema } => {
                assert_eq!(json_schema.name, "output");
                assert_eq!(json_schema.description.as_deref(), Some("test"));
                assert_eq!(json_schema.strict, Some(true));
            }
            other => panic!("expected JsonSchema, got {other:?}"),
        }
    }

    #[test]
    fn try_from_function_tool_succeeds() {
        use bitrouter_core::models::language::tool::LanguageModelTool;

        let tool = LanguageModelTool::Function {
            name: "search".to_owned(),
            description: Some("Search the web".to_owned()),
            input_schema: schemars::Schema::default(),
            input_examples: vec![],
            strict: Some(true),
            provider_options: None,
        };

        let converted = OpenAiChatTool::try_from(&tool).expect("should convert");
        assert_eq!(converted.kind, "function");
        assert_eq!(converted.function.name, "search");
        assert_eq!(
            converted.function.description.as_deref(),
            Some("Search the web")
        );
        assert_eq!(converted.function.strict, Some(true));
    }

    #[test]
    fn try_from_provider_tool_fails() {
        use bitrouter_core::models::language::tool::{LanguageModelTool, ProviderToolId};

        let tool = LanguageModelTool::Provider {
            id: ProviderToolId {
                provider_name: "openai".to_owned(),
                tool_id: "code_interpreter".to_owned(),
            },
            name: "code_interpreter".to_owned(),
            args: std::collections::HashMap::new(),
            provider_options: None,
        };

        let result = OpenAiChatTool::try_from(&tool);
        assert!(result.is_err());
    }
}
