use std::collections::HashMap;

use bitrouter_core::{
    errors::{BitrouterError, Result},
    models::{
        language::{
            content::LanguageModelContent,
            finish_reason::LanguageModelFinishReason,
            generate_result::{
                LanguageModelGenerateResult, LanguageModelRawRequest, LanguageModelRawResponse,
            },
            usage::{LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage},
        },
        shared::{provider::ProviderMetadata, types::JsonValue, warnings::Warning},
    },
};
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::json;

const OPENAI_PROVIDER_NAME: &str = "openai";

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

impl OpenAiChatCompletionResponse {
    pub fn into_generate_result(
        self,
        request_headers: Option<HeaderMap>,
        request_body: JsonValue,
        response_headers: Option<HeaderMap>,
        response_body: JsonValue,
    ) -> Result<LanguageModelGenerateResult> {
        let Some(choice) = self.choices.into_iter().find(|choice| choice.index == 0) else {
            return Err(BitrouterError::invalid_response(
                Some(OPENAI_PROVIDER_NAME),
                "chat completion response did not contain choice 0",
                Some(response_body),
            ));
        };

        let provider_metadata = openai_metadata(
            self.system_fingerprint.clone(),
            choice.message.refusal.clone(),
        );
        let finish_reason = map_finish_reason(choice.finish_reason.as_deref());
        let content = message_to_language_model_content(
            choice.message,
            provider_metadata.clone(),
            response_body.clone(),
        )?;

        Ok(LanguageModelGenerateResult {
            content,
            finish_reason,
            usage: self
                .usage
                .map(OpenAiUsage::into_language_model_usage)
                .unwrap_or_else(empty_usage),
            provider_metadata,
            request: Some(LanguageModelRawRequest {
                headers: request_headers,
                body: request_body,
            }),
            response_metadata: Some(LanguageModelRawResponse {
                id: Some(self.id),
                timestamp: Some(self.created.saturating_mul(1_000)),
                model_id: Some(self.model),
                headers: response_headers,
                body: Some(response_body),
            }),
            warnings: Some(Vec::<Warning>::new()),
        })
    }
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

impl OpenAiUsage {
    pub fn into_language_model_usage(self) -> LanguageModelUsage {
        let reasoning_tokens = self
            .completion_tokens_details
            .as_ref()
            .and_then(|details| details.reasoning_tokens);
        LanguageModelUsage {
            input_tokens: LanguageModelInputTokens {
                total: self.prompt_tokens,
                no_cache: self
                    .prompt_tokens_details
                    .as_ref()
                    .and_then(|details| details.cached_tokens)
                    .map(|cached_tokens| {
                        self.prompt_tokens
                            .unwrap_or(cached_tokens)
                            .saturating_sub(cached_tokens)
                    }),
                cache_read: self
                    .prompt_tokens_details
                    .as_ref()
                    .and_then(|details| details.cached_tokens),
                cache_write: None,
            },
            output_tokens: LanguageModelOutputTokens {
                total: self.completion_tokens,
                text: self.completion_tokens,
                reasoning: reasoning_tokens,
            },
            raw: serde_json::to_value(self).ok(),
        }
    }
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

pub fn map_finish_reason(finish_reason: Option<&str>) -> LanguageModelFinishReason {
    match finish_reason {
        Some("stop") | None => LanguageModelFinishReason::Stop,
        Some("length") => LanguageModelFinishReason::Length,
        Some("tool_calls") | Some("function_call") => LanguageModelFinishReason::FunctionCall,
        Some("content_filter") => LanguageModelFinishReason::ContentFilter,
        Some("error") => LanguageModelFinishReason::Error,
        Some(other) => LanguageModelFinishReason::Other(other.to_owned()),
    }
}

pub fn parse_openai_error(
    status_code: u16,
    request_id: Option<String>,
    body: Option<JsonValue>,
) -> BitrouterError {
    let parsed = body
        .as_ref()
        .and_then(|body| serde_json::from_value::<OpenAiErrorEnvelope>(body.clone()).ok());

    match parsed {
        Some(envelope) => BitrouterError::provider_error(
            OPENAI_PROVIDER_NAME,
            Some(status_code),
            envelope.error.error_type,
            envelope.error.code.and_then(json_value_to_string),
            envelope.error.param,
            envelope.error.message,
            request_id,
            body,
        ),
        None => BitrouterError::provider_error(
            OPENAI_PROVIDER_NAME,
            Some(status_code),
            None,
            None,
            None,
            format!("OpenAI returned HTTP {status_code}"),
            request_id,
            body,
        ),
    }
}

fn message_to_language_model_content(
    message: OpenAiChatCompletionMessage,
    provider_metadata: Option<ProviderMetadata>,
    response_body: JsonValue,
) -> Result<LanguageModelContent> {
    match (message.content, message.tool_calls) {
        (Some(content), None) => Ok(LanguageModelContent::Text {
            text: content,
            provider_metadata,
        }),
        (None, Some(tool_calls)) => {
            if tool_calls.len() != 1 {
                return Err(BitrouterError::invalid_response(
                    Some(OPENAI_PROVIDER_NAME),
                    "chat completion returned multiple tool calls, but bitrouter-core generate_result can only represent one top-level content item",
                    Some(response_body),
                ));
            }
            let tool_call = tool_calls.into_iter().next().expect("length checked");
            let tool_input = serde_json::from_str::<JsonValue>(&tool_call.function.arguments)
                .map_err(|error| {
                    BitrouterError::invalid_response(
                        Some(OPENAI_PROVIDER_NAME),
                        format!("tool call arguments were not valid JSON: {error}"),
                        Some(response_body.clone()),
                    )
                })?;
            Ok(LanguageModelContent::ToolCall {
                tool_call_id: tool_call.id,
                tool_name: tool_call.function.name,
                tool_input: serde_json::to_string(&tool_input).map_err(|error| {
                    BitrouterError::invalid_response(
                        Some(OPENAI_PROVIDER_NAME),
                        format!("failed to re-serialize tool call arguments: {error}"),
                        Some(response_body.clone()),
                    )
                })?,
                provider_executed: None,
                dynamic: None,
                provider_metadata,
            })
        }
        (Some(_), Some(_)) => Err(BitrouterError::invalid_response(
            Some(OPENAI_PROVIDER_NAME),
            "chat completion returned both assistant text and tool calls in one choice, which bitrouter-core generate_result cannot represent as a single content value",
            Some(response_body),
        )),
        (None, None) => Err(BitrouterError::invalid_response(
            Some(OPENAI_PROVIDER_NAME),
            "chat completion returned neither content nor tool calls",
            Some(response_body),
        )),
    }
}

fn openai_metadata(
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

fn json_value_to_string(value: JsonValue) -> Option<String> {
    match value {
        JsonValue::String(value) => Some(value),
        JsonValue::Number(value) => Some(value.to_string()),
        JsonValue::Bool(value) => Some(value.to_string()),
        JsonValue::Null => None,
        other => Some(other.to_string()),
    }
}

fn empty_usage() -> LanguageModelUsage {
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

    #[test]
    fn maps_tool_finish_reason() {
        assert_eq!(
            map_finish_reason(Some("tool_calls")),
            LanguageModelFinishReason::FunctionCall
        );
    }

    #[test]
    fn parses_openai_error_body() {
        let error = parse_openai_error(
            429,
            Some("req_123".to_owned()),
            Some(json!({
                "error": {
                    "message": "too many requests",
                    "type": "rate_limit_error",
                    "param": null,
                    "code": "rate_limit_exceeded"
                }
            })),
        );

        match error {
            BitrouterError::Provider {
                status_code,
                code,
                request_id,
                ..
            } => {
                assert_eq!(status_code, Some(429));
                assert_eq!(code.as_deref(), Some("rate_limit_exceeded"));
                assert_eq!(request_id.as_deref(), Some("req_123"));
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }
}
