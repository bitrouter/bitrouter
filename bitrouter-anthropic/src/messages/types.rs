use std::collections::HashMap;

use bitrouter_core::{
    errors::{BitrouterError, Result},
    models::{
        language::{
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

pub(super) const ANTHROPIC_PROVIDER_NAME: &str = "anthropic";
pub(super) const STREAM_TEXT_ID: &str = "text";

// ── Response types ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMessageResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub role: String,
    pub content: Vec<AnthropicContentBlock>,
    pub model: String,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
    #[serde(default)]
    pub usage: Option<AnthropicUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: JsonValue,
    },
}

// ── Streaming event types ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicStreamEvent {
    MessageStart {
        message: AnthropicMessageResponse,
    },
    ContentBlockStart {
        index: u32,
        content_block: AnthropicContentBlock,
    },
    ContentBlockDelta {
        index: u32,
        delta: AnthropicDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: AnthropicMessageDelta,
        #[serde(default)]
        usage: Option<AnthropicUsage>,
    },
    MessageStop,
    Ping,
    Error {
        error: AnthropicStreamError,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicDelta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMessageDelta {
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicStreamError {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

// ── Usage types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicUsage {
    #[serde(default)]
    pub input_tokens: Option<u32>,
    #[serde(default)]
    pub output_tokens: Option<u32>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u32>,
}

impl From<AnthropicUsage> for LanguageModelUsage {
    fn from(usage: AnthropicUsage) -> Self {
        let raw = serde_json::to_value(&usage).ok();
        LanguageModelUsage {
            input_tokens: LanguageModelInputTokens {
                total: usage.input_tokens,
                no_cache: usage.input_tokens.map(|total| {
                    total
                        .saturating_sub(usage.cache_read_input_tokens.unwrap_or(0))
                        .saturating_sub(usage.cache_creation_input_tokens.unwrap_or(0))
                }),
                cache_read: usage.cache_read_input_tokens,
                cache_write: usage.cache_creation_input_tokens,
            },
            output_tokens: LanguageModelOutputTokens {
                total: usage.output_tokens,
                text: usage.output_tokens,
                reasoning: None,
            },
            raw,
        }
    }
}

// ── Error types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicErrorEnvelope {
    #[serde(rename = "type")]
    pub kind: String,
    pub error: AnthropicApiError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicApiError {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

// ── Request types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMessagesRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessageParam>,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<AnthropicToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<AnthropicMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMessageParam {
    pub role: String,
    pub content: AnthropicMessageContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnthropicMessageContent {
    Text(String),
    Blocks(Vec<AnthropicInputContentBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicInputContentBlock {
    Text {
        text: String,
    },
    Image {
        source: AnthropicImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: JsonValue,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicImageSource {
    #[serde(rename = "type")]
    pub kind: String,
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: schemars::Schema,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicToolChoice {
    Auto,
    Any,
    Tool { name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

// ── From / TryFrom conversions ──────────────────────────────────────────────

impl From<&LanguageModelToolChoice> for AnthropicToolChoice {
    fn from(choice: &LanguageModelToolChoice) -> Self {
        match choice {
            LanguageModelToolChoice::Auto => AnthropicToolChoice::Auto,
            LanguageModelToolChoice::None => AnthropicToolChoice::Auto,
            LanguageModelToolChoice::Required => AnthropicToolChoice::Any,
            LanguageModelToolChoice::Tool { tool_name } => AnthropicToolChoice::Tool {
                name: tool_name.clone(),
            },
        }
    }
}

impl TryFrom<&LanguageModelTool> for AnthropicTool {
    type Error = BitrouterError;

    fn try_from(tool: &LanguageModelTool) -> Result<Self> {
        match tool {
            LanguageModelTool::Function {
                name,
                description,
                input_schema,
                ..
            } => Ok(AnthropicTool {
                name: name.clone(),
                description: description.clone(),
                input_schema: input_schema.clone(),
            }),
            LanguageModelTool::Provider { id, .. } => Err(BitrouterError::unsupported(
                ANTHROPIC_PROVIDER_NAME,
                format!("provider tool {}:{}", id.provider_name, id.tool_id),
                Some(
                    "Anthropic messages API supports function tools, \
                     but bitrouter-core provider tools do not map cleanly here"
                        .to_owned(),
                ),
            )),
        }
    }
}

// ── Helper functions ────────────────────────────────────────────────────────

pub(super) fn map_finish_reason(stop_reason: Option<&str>) -> LanguageModelFinishReason {
    match stop_reason {
        Some("end_turn") | None => LanguageModelFinishReason::Stop,
        Some("stop_sequence") => LanguageModelFinishReason::Stop,
        Some("max_tokens") => LanguageModelFinishReason::Length,
        Some("tool_use") => LanguageModelFinishReason::FunctionCall,
        Some("content_filter") => LanguageModelFinishReason::ContentFilter,
        Some("error") => LanguageModelFinishReason::Error,
        Some(other) => LanguageModelFinishReason::Other(other.to_owned()),
    }
}

pub(super) fn anthropic_metadata(
    stop_sequence: Option<String>,
) -> Option<ProviderMetadata> {
    let mut inner = HashMap::new();
    if let Some(stop_sequence) = stop_sequence {
        inner.insert(
            "stop_sequence".to_owned(),
            JsonValue::String(stop_sequence),
        );
    }

    if inner.is_empty() {
        None
    } else {
        Some(HashMap::from([(
            ANTHROPIC_PROVIDER_NAME.to_owned(),
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

#[allow(dead_code)]
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
    fn maps_end_turn_finish_reason() {
        assert_eq!(
            map_finish_reason(Some("end_turn")),
            LanguageModelFinishReason::Stop
        );
    }

    #[test]
    fn maps_all_finish_reasons() {
        assert_eq!(
            map_finish_reason(Some("end_turn")),
            LanguageModelFinishReason::Stop
        );
        assert_eq!(
            map_finish_reason(Some("stop_sequence")),
            LanguageModelFinishReason::Stop
        );
        assert_eq!(map_finish_reason(None), LanguageModelFinishReason::Stop);
        assert_eq!(
            map_finish_reason(Some("max_tokens")),
            LanguageModelFinishReason::Length
        );
        assert_eq!(
            map_finish_reason(Some("tool_use")),
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
            map_finish_reason(Some("unknown_reason")),
            LanguageModelFinishReason::Other("unknown_reason".to_owned())
        );
    }

    #[test]
    fn anthropic_usage_to_language_model_usage() {
        let usage = AnthropicUsage {
            input_tokens: Some(100),
            output_tokens: Some(50),
            cache_creation_input_tokens: Some(10),
            cache_read_input_tokens: Some(20),
        };
        let lm_usage: LanguageModelUsage = usage.into();
        assert_eq!(lm_usage.input_tokens.total, Some(100));
        assert_eq!(lm_usage.input_tokens.no_cache, Some(70));
        assert_eq!(lm_usage.input_tokens.cache_read, Some(20));
        assert_eq!(lm_usage.input_tokens.cache_write, Some(10));
        assert_eq!(lm_usage.output_tokens.total, Some(50));
        assert_eq!(lm_usage.output_tokens.text, Some(50));
        assert_eq!(lm_usage.output_tokens.reasoning, None);
    }

    #[test]
    fn anthropic_usage_without_cache() {
        let usage = AnthropicUsage {
            input_tokens: Some(100),
            output_tokens: Some(50),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        };
        let lm_usage: LanguageModelUsage = usage.into();
        assert_eq!(lm_usage.input_tokens.total, Some(100));
        assert_eq!(lm_usage.input_tokens.no_cache, Some(100));
        assert_eq!(lm_usage.input_tokens.cache_read, None);
        assert_eq!(lm_usage.input_tokens.cache_write, None);
    }

    #[test]
    fn deserialize_text_response() {
        let json = r#"{
            "id": "msg_123",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "Hello!"}],
            "model": "claude-3-5-sonnet-20241022",
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 10, "output_tokens": 5}
        }"#;
        let response: AnthropicMessageResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.id, "msg_123");
        assert_eq!(response.content.len(), 1);
        assert!(matches!(
            &response.content[0],
            AnthropicContentBlock::Text { text } if text == "Hello!"
        ));
        assert_eq!(response.stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn deserialize_tool_use_response() {
        let json = r#"{
            "id": "msg_456",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "tool_use", "id": "toolu_123", "name": "get_weather", "input": {"location": "Paris"}}
            ],
            "model": "claude-3-5-sonnet-20241022",
            "stop_reason": "tool_use",
            "stop_sequence": null,
            "usage": {"input_tokens": 20, "output_tokens": 15}
        }"#;
        let response: AnthropicMessageResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.content.len(), 1);
        assert!(matches!(
            &response.content[0],
            AnthropicContentBlock::ToolUse { name, .. } if name == "get_weather"
        ));
    }

    #[test]
    fn serialize_request() {
        let request = AnthropicMessagesRequest {
            model: "claude-3-5-sonnet-20241022".to_owned(),
            messages: vec![AnthropicMessageParam {
                role: "user".to_owned(),
                content: AnthropicMessageContent::Text("Hello".to_owned()),
            }],
            max_tokens: 1024,
            system: Some("You are a helpful assistant.".to_owned()),
            stream: None,
            temperature: Some(0.7),
            top_p: None,
            top_k: None,
            stop_sequences: None,
            tools: None,
            tool_choice: None,
            metadata: None,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["model"], "claude-3-5-sonnet-20241022");
        assert_eq!(json["max_tokens"], 1024);
        assert_eq!(json["system"], "You are a helpful assistant.");
        assert!(json["temperature"].as_f64().unwrap() - 0.7 < 0.01);
        assert!(json.get("top_p").is_none());
        assert!(json.get("stream").is_none());
    }

    #[test]
    fn tool_choice_auto() {
        let choice = AnthropicToolChoice::from(&LanguageModelToolChoice::Auto);
        let json = serde_json::to_value(&choice).unwrap();
        assert_eq!(json["type"], "auto");
    }

    #[test]
    fn tool_choice_required_maps_to_any() {
        let choice = AnthropicToolChoice::from(&LanguageModelToolChoice::Required);
        let json = serde_json::to_value(&choice).unwrap();
        assert_eq!(json["type"], "any");
    }

    #[test]
    fn tool_choice_named() {
        let choice = AnthropicToolChoice::from(&LanguageModelToolChoice::Tool {
            tool_name: "get_weather".to_owned(),
        });
        let json = serde_json::to_value(&choice).unwrap();
        assert_eq!(json["type"], "tool");
        assert_eq!(json["name"], "get_weather");
    }

    #[test]
    fn tool_conversion_function() {
        let tool = LanguageModelTool::Function {
            name: "test_tool".to_owned(),
            description: Some("A test tool".to_owned()),
            input_schema: schemars::Schema::default(),
            input_examples: vec![],
            strict: None,
            provider_options: None,
        };
        let result = AnthropicTool::try_from(&tool);
        assert!(result.is_ok());
        let anthropic_tool = result.unwrap();
        assert_eq!(anthropic_tool.name, "test_tool");
        assert_eq!(anthropic_tool.description.as_deref(), Some("A test tool"));
    }

    #[test]
    fn tool_conversion_provider_fails() {
        let tool = LanguageModelTool::Provider {
            id: bitrouter_core::models::language::tool::ProviderToolId {
                provider_name: "test".to_owned(),
                tool_id: "123".to_owned(),
            },
            name: "test_tool".to_owned(),
            args: HashMap::new(),
            provider_options: None,
        };
        let result = AnthropicTool::try_from(&tool);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_streaming_events() {
        let message_start = r#"{
            "type": "message_start",
            "message": {
                "id": "msg_123",
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": "claude-3-5-sonnet-20241022",
                "stop_reason": null,
                "usage": {"input_tokens": 10, "output_tokens": 0}
            }
        }"#;
        let event: AnthropicStreamEvent = serde_json::from_str(message_start).unwrap();
        assert!(matches!(event, AnthropicStreamEvent::MessageStart { .. }));

        let content_block_start = r#"{
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""}
        }"#;
        let event: AnthropicStreamEvent = serde_json::from_str(content_block_start).unwrap();
        assert!(matches!(
            event,
            AnthropicStreamEvent::ContentBlockStart { index: 0, .. }
        ));

        let content_block_delta = r#"{
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "Hello"}
        }"#;
        let event: AnthropicStreamEvent = serde_json::from_str(content_block_delta).unwrap();
        assert!(matches!(
            event,
            AnthropicStreamEvent::ContentBlockDelta { index: 0, .. }
        ));

        let message_stop = r#"{"type": "message_stop"}"#;
        let event: AnthropicStreamEvent = serde_json::from_str(message_stop).unwrap();
        assert!(matches!(event, AnthropicStreamEvent::MessageStop));

        let ping = r#"{"type": "ping"}"#;
        let event: AnthropicStreamEvent = serde_json::from_str(ping).unwrap();
        assert!(matches!(event, AnthropicStreamEvent::Ping));
    }

    #[test]
    fn deserialize_tool_use_stream_events() {
        let tool_block_start = r#"{
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_123", "name": "get_weather", "input": {}}
        }"#;
        let event: AnthropicStreamEvent = serde_json::from_str(tool_block_start).unwrap();
        assert!(matches!(
            event,
            AnthropicStreamEvent::ContentBlockStart {
                index: 0,
                content_block: AnthropicContentBlock::ToolUse { .. },
            }
        ));

        let input_delta = r#"{
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "{\"location\": \"Pa"}
        }"#;
        let event: AnthropicStreamEvent = serde_json::from_str(input_delta).unwrap();
        assert!(matches!(
            event,
            AnthropicStreamEvent::ContentBlockDelta {
                index: 0,
                delta: AnthropicDelta::InputJsonDelta { .. },
            }
        ));
    }

    #[test]
    fn deserialize_message_delta() {
        let json = r#"{
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 15}
        }"#;
        let event: AnthropicStreamEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(
            event,
            AnthropicStreamEvent::MessageDelta { .. }
        ));
    }

    #[test]
    fn deserialize_error_envelope() {
        let json = r#"{
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "max_tokens must be less than 4096"
            }
        }"#;
        let envelope: AnthropicErrorEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(envelope.error.error_type, "invalid_request_error");
        assert_eq!(
            envelope.error.message,
            "max_tokens must be less than 4096"
        );
    }

    #[test]
    fn serialize_message_content_text() {
        let content = AnthropicMessageContent::Text("Hello".to_owned());
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json, "Hello");
    }

    #[test]
    fn serialize_message_content_blocks() {
        let content = AnthropicMessageContent::Blocks(vec![
            AnthropicInputContentBlock::Text {
                text: "Analyze this".to_owned(),
            },
            AnthropicInputContentBlock::Image {
                source: AnthropicImageSource {
                    kind: "base64".to_owned(),
                    media_type: "image/png".to_owned(),
                    data: "abc123".to_owned(),
                },
            },
        ]);
        let json = serde_json::to_value(&content).unwrap();
        assert!(json.is_array());
        assert_eq!(json[0]["type"], "text");
        assert_eq!(json[1]["type"], "image");
        assert_eq!(json[1]["source"]["type"], "base64");
    }

    #[test]
    fn serialize_tool_result_block() {
        let block = AnthropicInputContentBlock::ToolResult {
            tool_use_id: "toolu_123".to_owned(),
            content: Some("The weather is sunny".to_owned()),
            is_error: None,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "tool_result");
        assert_eq!(json["tool_use_id"], "toolu_123");
        assert_eq!(json["content"], "The weather is sunny");
    }

    #[test]
    fn anthropic_metadata_with_stop_sequence() {
        let meta = anthropic_metadata(Some("</result>".to_owned()));
        assert!(meta.is_some());
        let meta = meta.unwrap();
        let inner = meta.get(ANTHROPIC_PROVIDER_NAME).unwrap();
        assert_eq!(inner["stop_sequence"], "</result>");
    }

    #[test]
    fn anthropic_metadata_empty() {
        let meta = anthropic_metadata(None);
        assert!(meta.is_none());
    }

    #[test]
    fn json_value_to_string_conversions() {
        assert_eq!(
            json_value_to_string(JsonValue::String("hello".to_owned())),
            Some("hello".to_owned())
        );
        assert_eq!(
            json_value_to_string(json!(42)),
            Some("42".to_owned())
        );
        assert_eq!(
            json_value_to_string(json!(true)),
            Some("true".to_owned())
        );
        assert_eq!(json_value_to_string(JsonValue::Null), None);
    }

    #[test]
    fn request_roundtrip_with_tools() {
        let request = AnthropicMessagesRequest {
            model: "claude-3-5-sonnet-20241022".to_owned(),
            messages: vec![AnthropicMessageParam {
                role: "user".to_owned(),
                content: AnthropicMessageContent::Text("Hello".to_owned()),
            }],
            max_tokens: 1024,
            system: None,
            stream: Some(true),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            tools: Some(vec![AnthropicTool {
                name: "get_weather".to_owned(),
                description: Some("Get the weather".to_owned()),
                input_schema: schemars::Schema::default(),
            }]),
            tool_choice: Some(AnthropicToolChoice::Auto),
            metadata: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        let parsed: AnthropicMessagesRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.model, "claude-3-5-sonnet-20241022");
        assert_eq!(parsed.tools.as_ref().unwrap().len(), 1);
        assert!(matches!(
            parsed.tool_choice.as_ref().unwrap(),
            AnthropicToolChoice::Auto
        ));
    }
}
