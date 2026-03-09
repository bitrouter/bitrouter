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

pub(super) const GOOGLE_PROVIDER_NAME: &str = "google";
pub(super) const STREAM_TEXT_ID: &str = "text";

// ── Response types ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleGenerateContentResponse {
    #[serde(default)]
    pub candidates: Option<Vec<GoogleCandidate>>,
    #[serde(default)]
    pub usage_metadata: Option<GoogleUsageMetadata>,
    #[serde(default)]
    pub model_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleCandidate {
    #[serde(default)]
    pub content: Option<GoogleContent>,
    #[serde(default)]
    pub finish_reason: Option<String>,
    #[serde(default)]
    pub index: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleContent {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub parts: Option<Vec<GooglePart>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GooglePart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_data: Option<GoogleInlineData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<GoogleFunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_response: Option<GoogleFunctionResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleInlineData {
    pub mime_type: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleFunctionCall {
    pub name: String,
    #[serde(default)]
    pub args: Option<JsonValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleFunctionResponse {
    pub name: String,
    pub response: JsonValue,
}

// ── Usage types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleUsageMetadata {
    #[serde(default)]
    pub prompt_token_count: Option<u32>,
    #[serde(default)]
    pub candidates_token_count: Option<u32>,
    #[serde(default)]
    pub total_token_count: Option<u32>,
    #[serde(default)]
    pub cached_content_token_count: Option<u32>,
}

impl From<GoogleUsageMetadata> for LanguageModelUsage {
    fn from(usage: GoogleUsageMetadata) -> Self {
        let raw = serde_json::to_value(&usage).ok();
        LanguageModelUsage {
            input_tokens: LanguageModelInputTokens {
                total: usage.prompt_token_count,
                no_cache: usage.prompt_token_count.map(|total| {
                    total.saturating_sub(usage.cached_content_token_count.unwrap_or(0))
                }),
                cache_read: usage.cached_content_token_count,
                cache_write: None,
            },
            output_tokens: LanguageModelOutputTokens {
                total: usage.candidates_token_count,
                text: usage.candidates_token_count,
                reasoning: None,
            },
            raw,
        }
    }
}

// ── Error types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleErrorEnvelope {
    pub error: GoogleApiError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleApiError {
    #[serde(default)]
    pub code: Option<u16>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

// ── Request types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleGenerateContentRequest {
    pub contents: Vec<GoogleContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<GoogleContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<GoogleTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_config: Option<GoogleToolConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GoogleGenerationConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<schemars::Schema>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleTool {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_declarations: Option<Vec<GoogleFunctionDeclaration>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleFunctionDeclaration {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<schemars::Schema>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleToolConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_calling_config: Option<GoogleFunctionCallingConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleFunctionCallingConfig {
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_function_names: Option<Vec<String>>,
}

// ── From / TryFrom conversions ──────────────────────────────────────────────

impl From<&LanguageModelToolChoice> for GoogleFunctionCallingConfig {
    fn from(choice: &LanguageModelToolChoice) -> Self {
        match choice {
            LanguageModelToolChoice::Auto => GoogleFunctionCallingConfig {
                mode: "AUTO".to_owned(),
                allowed_function_names: None,
            },
            LanguageModelToolChoice::None => GoogleFunctionCallingConfig {
                mode: "NONE".to_owned(),
                allowed_function_names: None,
            },
            LanguageModelToolChoice::Required => GoogleFunctionCallingConfig {
                mode: "ANY".to_owned(),
                allowed_function_names: None,
            },
            LanguageModelToolChoice::Tool { tool_name } => GoogleFunctionCallingConfig {
                mode: "ANY".to_owned(),
                allowed_function_names: Some(vec![tool_name.clone()]),
            },
        }
    }
}

impl TryFrom<&LanguageModelTool> for GoogleFunctionDeclaration {
    type Error = BitrouterError;

    fn try_from(tool: &LanguageModelTool) -> Result<Self> {
        match tool {
            LanguageModelTool::Function {
                name,
                description,
                input_schema,
                ..
            } => Ok(GoogleFunctionDeclaration {
                name: name.clone(),
                description: description.clone(),
                parameters: Some(input_schema.clone()),
            }),
            LanguageModelTool::Provider { id, .. } => Err(BitrouterError::unsupported(
                GOOGLE_PROVIDER_NAME,
                format!("provider tool {}:{}", id.provider_name, id.tool_id),
                Some(
                    "Google Generative AI API supports function declarations, \
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
        Some("STOP") | None => LanguageModelFinishReason::Stop,
        Some("MAX_TOKENS") => LanguageModelFinishReason::Length,
        Some("SAFETY")
        | Some("RECITATION")
        | Some("BLOCKLIST")
        | Some("PROHIBITED_CONTENT")
        | Some("SPII") => LanguageModelFinishReason::ContentFilter,
        Some("MALFORMED_FUNCTION_CALL") => LanguageModelFinishReason::Error,
        Some("LANGUAGE") => LanguageModelFinishReason::Other("LANGUAGE".to_owned()),
        Some(other) => LanguageModelFinishReason::Other(other.to_owned()),
    }
}

pub(super) fn google_metadata(model_version: Option<String>) -> Option<ProviderMetadata> {
    let mut inner = HashMap::new();
    if let Some(version) = model_version {
        inner.insert("model_version".to_owned(), JsonValue::String(version));
    }

    if inner.is_empty() {
        None
    } else {
        Some(HashMap::from([(
            GOOGLE_PROVIDER_NAME.to_owned(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_core::models::language::usage::LanguageModelUsage;

    #[test]
    fn maps_stop_finish_reason() {
        assert_eq!(
            map_finish_reason(Some("STOP")),
            LanguageModelFinishReason::Stop
        );
    }

    #[test]
    fn maps_all_finish_reasons() {
        assert_eq!(
            map_finish_reason(Some("STOP")),
            LanguageModelFinishReason::Stop
        );
        assert_eq!(map_finish_reason(None), LanguageModelFinishReason::Stop);
        assert_eq!(
            map_finish_reason(Some("MAX_TOKENS")),
            LanguageModelFinishReason::Length
        );
        assert_eq!(
            map_finish_reason(Some("SAFETY")),
            LanguageModelFinishReason::ContentFilter
        );
        assert_eq!(
            map_finish_reason(Some("RECITATION")),
            LanguageModelFinishReason::ContentFilter
        );
        assert_eq!(
            map_finish_reason(Some("BLOCKLIST")),
            LanguageModelFinishReason::ContentFilter
        );
        assert_eq!(
            map_finish_reason(Some("PROHIBITED_CONTENT")),
            LanguageModelFinishReason::ContentFilter
        );
        assert_eq!(
            map_finish_reason(Some("SPII")),
            LanguageModelFinishReason::ContentFilter
        );
        assert_eq!(
            map_finish_reason(Some("MALFORMED_FUNCTION_CALL")),
            LanguageModelFinishReason::Error
        );
        assert_eq!(
            map_finish_reason(Some("LANGUAGE")),
            LanguageModelFinishReason::Other("LANGUAGE".to_owned())
        );
        assert_eq!(
            map_finish_reason(Some("unknown_reason")),
            LanguageModelFinishReason::Other("unknown_reason".to_owned())
        );
    }

    #[test]
    fn google_usage_to_language_model_usage() {
        let usage = GoogleUsageMetadata {
            prompt_token_count: Some(100),
            candidates_token_count: Some(50),
            total_token_count: Some(150),
            cached_content_token_count: Some(20),
        };
        let lm_usage: LanguageModelUsage = usage.into();
        assert_eq!(lm_usage.input_tokens.total, Some(100));
        assert_eq!(lm_usage.input_tokens.no_cache, Some(80));
        assert_eq!(lm_usage.input_tokens.cache_read, Some(20));
        assert_eq!(lm_usage.input_tokens.cache_write, None);
        assert_eq!(lm_usage.output_tokens.total, Some(50));
        assert_eq!(lm_usage.output_tokens.text, Some(50));
        assert_eq!(lm_usage.output_tokens.reasoning, None);
    }

    #[test]
    fn google_usage_without_cache() {
        let usage = GoogleUsageMetadata {
            prompt_token_count: Some(100),
            candidates_token_count: Some(50),
            total_token_count: Some(150),
            cached_content_token_count: None,
        };
        let lm_usage: LanguageModelUsage = usage.into();
        assert_eq!(lm_usage.input_tokens.total, Some(100));
        assert_eq!(lm_usage.input_tokens.no_cache, Some(100));
        assert_eq!(lm_usage.input_tokens.cache_read, None);
    }

    #[test]
    fn deserialize_text_response() {
        let json = r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Hello!"}]
                },
                "finishReason": "STOP",
                "index": 0
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "totalTokenCount": 15
            },
            "modelVersion": "gemini-2.0-flash"
        }"#;
        let response: GoogleGenerateContentResponse = serde_json::from_str(json).unwrap();
        let candidates = response.candidates.unwrap();
        assert_eq!(candidates.len(), 1);
        let parts = candidates[0]
            .content
            .as_ref()
            .unwrap()
            .parts
            .as_ref()
            .unwrap();
        assert_eq!(parts[0].text.as_deref(), Some("Hello!"));
        assert_eq!(candidates[0].finish_reason.as_deref(), Some("STOP"));
        assert_eq!(response.model_version.as_deref(), Some("gemini-2.0-flash"));
    }

    #[test]
    fn deserialize_function_call_response() {
        let json = r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{
                        "functionCall": {
                            "name": "get_weather",
                            "args": {"location": "Paris"}
                        }
                    }]
                },
                "finishReason": "STOP",
                "index": 0
            }],
            "usageMetadata": {
                "promptTokenCount": 20,
                "candidatesTokenCount": 15,
                "totalTokenCount": 35
            }
        }"#;
        let response: GoogleGenerateContentResponse = serde_json::from_str(json).unwrap();
        let candidates = response.candidates.unwrap();
        let parts = candidates[0]
            .content
            .as_ref()
            .unwrap()
            .parts
            .as_ref()
            .unwrap();
        assert!(parts[0].function_call.is_some());
        assert_eq!(parts[0].function_call.as_ref().unwrap().name, "get_weather");
    }

    #[test]
    fn serialize_request() {
        let request = GoogleGenerateContentRequest {
            contents: vec![GoogleContent {
                role: Some("user".to_owned()),
                parts: Some(vec![GooglePart {
                    text: Some("Hello".to_owned()),
                    inline_data: None,
                    function_call: None,
                    function_response: None,
                }]),
            }],
            system_instruction: Some(GoogleContent {
                role: None,
                parts: Some(vec![GooglePart {
                    text: Some("You are a helpful assistant.".to_owned()),
                    inline_data: None,
                    function_call: None,
                    function_response: None,
                }]),
            }),
            tools: None,
            tool_config: None,
            generation_config: Some(GoogleGenerationConfig {
                temperature: Some(0.7),
                top_p: None,
                top_k: None,
                max_output_tokens: Some(1024),
                stop_sequences: None,
                presence_penalty: None,
                frequency_penalty: None,
                seed: None,
                response_mime_type: None,
                response_schema: None,
            }),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["contents"][0]["role"], "user");
        assert_eq!(json["contents"][0]["parts"][0]["text"], "Hello");
        assert_eq!(
            json["systemInstruction"]["parts"][0]["text"],
            "You are a helpful assistant."
        );
        assert!(json["generationConfig"]["temperature"].as_f64().unwrap() - 0.7 < 0.01);
        assert_eq!(json["generationConfig"]["maxOutputTokens"], 1024);
        assert!(json.get("tools").is_none());
    }

    #[test]
    fn tool_choice_auto() {
        let config = GoogleFunctionCallingConfig::from(&LanguageModelToolChoice::Auto);
        assert_eq!(config.mode, "AUTO");
        assert!(config.allowed_function_names.is_none());
    }

    #[test]
    fn tool_choice_none() {
        let config = GoogleFunctionCallingConfig::from(&LanguageModelToolChoice::None);
        assert_eq!(config.mode, "NONE");
    }

    #[test]
    fn tool_choice_required_maps_to_any() {
        let config = GoogleFunctionCallingConfig::from(&LanguageModelToolChoice::Required);
        assert_eq!(config.mode, "ANY");
        assert!(config.allowed_function_names.is_none());
    }

    #[test]
    fn tool_choice_named() {
        let config = GoogleFunctionCallingConfig::from(&LanguageModelToolChoice::Tool {
            tool_name: "get_weather".to_owned(),
        });
        assert_eq!(config.mode, "ANY");
        assert_eq!(
            config.allowed_function_names.as_ref().unwrap(),
            &["get_weather"]
        );
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
        let result = GoogleFunctionDeclaration::try_from(&tool);
        assert!(result.is_ok());
        let decl = result.unwrap();
        assert_eq!(decl.name, "test_tool");
        assert_eq!(decl.description.as_deref(), Some("A test tool"));
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
        let result = GoogleFunctionDeclaration::try_from(&tool);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_error_envelope() {
        let json = r#"{
            "error": {
                "code": 400,
                "message": "Invalid value at 'contents'",
                "status": "INVALID_ARGUMENT"
            }
        }"#;
        let envelope: GoogleErrorEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(envelope.error.code, Some(400));
        assert_eq!(
            envelope.error.message.as_deref(),
            Some("Invalid value at 'contents'")
        );
        assert_eq!(envelope.error.status.as_deref(), Some("INVALID_ARGUMENT"));
    }

    #[test]
    fn serialize_inline_data_part() {
        let part = GooglePart {
            text: None,
            inline_data: Some(GoogleInlineData {
                mime_type: "image/png".to_owned(),
                data: "abc123".to_owned(),
            }),
            function_call: None,
            function_response: None,
        };
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(json["inlineData"]["mimeType"], "image/png");
        assert_eq!(json["inlineData"]["data"], "abc123");
        assert!(json.get("text").is_none());
    }

    #[test]
    fn google_metadata_with_model_version() {
        let meta = google_metadata(Some("gemini-2.0-flash".to_owned()));
        assert!(meta.is_some());
        let meta = meta.unwrap();
        let inner = meta.get(GOOGLE_PROVIDER_NAME).unwrap();
        assert_eq!(inner["model_version"], "gemini-2.0-flash");
    }

    #[test]
    fn google_metadata_empty() {
        let meta = google_metadata(None);
        assert!(meta.is_none());
    }

    #[test]
    fn request_roundtrip_with_tools() {
        let request = GoogleGenerateContentRequest {
            contents: vec![GoogleContent {
                role: Some("user".to_owned()),
                parts: Some(vec![GooglePart {
                    text: Some("Hello".to_owned()),
                    inline_data: None,
                    function_call: None,
                    function_response: None,
                }]),
            }],
            system_instruction: None,
            tools: Some(vec![GoogleTool {
                function_declarations: Some(vec![GoogleFunctionDeclaration {
                    name: "get_weather".to_owned(),
                    description: Some("Get the weather".to_owned()),
                    parameters: Some(schemars::Schema::default()),
                }]),
            }]),
            tool_config: Some(GoogleToolConfig {
                function_calling_config: Some(GoogleFunctionCallingConfig {
                    mode: "AUTO".to_owned(),
                    allowed_function_names: None,
                }),
            }),
            generation_config: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        let parsed: GoogleGenerateContentRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.contents.len(), 1);
        assert_eq!(
            parsed.tools.as_ref().unwrap()[0]
                .function_declarations
                .as_ref()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            parsed
                .tool_config
                .as_ref()
                .unwrap()
                .function_calling_config
                .as_ref()
                .unwrap()
                .mode,
            "AUTO"
        );
    }
}
