use std::{collections::HashMap, pin::Pin};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use bitrouter_core::{
    errors::{BitrouterError, ProviderErrorContext, Result},
    models::{
        language::{
            call_options::{LanguageModelCallOptions, LanguageModelResponseFormat},
            content::LanguageModelContent,
            data_content::LanguageModelDataContent,
            finish_reason::LanguageModelFinishReason,
            generate_result::{
                LanguageModelGenerateResult, LanguageModelRawRequest, LanguageModelRawResponse,
            },
            prompt::{
                LanguageModelAssistantContent, LanguageModelMessage, LanguageModelToolResult,
                LanguageModelToolResultOutput, LanguageModelToolResultOutputContent,
                LanguageModelToolResultOutputContentFileId, LanguageModelUserContent,
            },
            stream_part::LanguageModelStreamPart,
            tool::LanguageModelTool,
            tool_choice::LanguageModelToolChoice,
            usage::{LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage},
        },
        shared::{provider::ProviderMetadata, types::JsonValue, warnings::Warning},
    },
};
use bytes::Bytes;
use reqwest::header::HeaderMap;
use serde_json::json;
use tokio::{select, sync::mpsc};
use tokio_stream::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

use bitrouter_core::api::openai::chat::types::{
    ChatCompletionChoiceMessage, ChatCompletionChunk, ChatCompletionErrorEnvelope,
    ChatCompletionRequest, ChatCompletionResponse, ChatCompletionStreamOptions,
    ChatCompletionUsage, ChatContentPart, ChatImageUrl, ChatJsonSchemaConfig, ChatMessage,
    ChatMessageContent, ChatMessageToolCall, ChatMessageToolCallFunction, ChatNamedToolChoice,
    ChatResponseFormat, ChatResponseToolCallDelta, ChatTool, ChatToolChoice, ChatToolFunction,
};
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) const OPENAI_PROVIDER_NAME: &str = "openai";
const STREAM_TEXT_ID: &str = "text";

// ── Helper functions (moved from types.rs) ──────────────────────────────────

fn map_finish_reason(finish_reason: Option<&str>) -> LanguageModelFinishReason {
    match finish_reason {
        Some("stop") | None => LanguageModelFinishReason::Stop,
        Some("length") => LanguageModelFinishReason::Length,
        Some("tool_calls") | Some("function_call") => LanguageModelFinishReason::FunctionCall,
        Some("content_filter") => LanguageModelFinishReason::ContentFilter,
        Some("error") => LanguageModelFinishReason::Error,
        Some(other) => LanguageModelFinishReason::Other(other.to_owned()),
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

fn json_value_to_string(value: JsonValue) -> Option<String> {
    match value {
        JsonValue::String(value) => Some(value),
        JsonValue::Number(value) => Some(value.to_string()),
        JsonValue::Bool(value) => Some(value.to_string()),
        JsonValue::Null => None,
        other => Some(other.to_string()),
    }
}

fn usage_to_language_model(usage: ChatCompletionUsage) -> LanguageModelUsage {
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

fn tool_choice_from_language_model(choice: &LanguageModelToolChoice) -> ChatToolChoice {
    match choice {
        LanguageModelToolChoice::Auto => ChatToolChoice::Mode("auto".to_owned()),
        LanguageModelToolChoice::None => ChatToolChoice::Mode("none".to_owned()),
        LanguageModelToolChoice::Required => ChatToolChoice::Mode("required".to_owned()),
        LanguageModelToolChoice::Tool { tool_name } => ChatToolChoice::Named {
            r#type: "function".to_owned(),
            function: ChatNamedToolChoice {
                name: tool_name.clone(),
            },
        },
    }
}

fn response_format_from_language_model(format: &LanguageModelResponseFormat) -> ChatResponseFormat {
    match format {
        LanguageModelResponseFormat::Text => ChatResponseFormat::Text,
        LanguageModelResponseFormat::Json {
            schema,
            name,
            description,
        } => match schema {
            Some(schema) => ChatResponseFormat::JsonSchema {
                json_schema: ChatJsonSchemaConfig {
                    name: name.clone().unwrap_or_else(|| "output".to_owned()),
                    description: description.clone(),
                    schema: schema.clone(),
                    strict: Some(true),
                },
            },
            None => ChatResponseFormat::JsonObject,
        },
    }
}

fn tool_from_language_model(tool: &LanguageModelTool) -> Result<ChatTool> {
    match tool {
        LanguageModelTool::Function {
            name,
            description,
            input_schema,
            strict,
            ..
        } => {
            let parameters = serde_json::to_value(input_schema).map_err(|error| {
                BitrouterError::invalid_request(
                    Some(OPENAI_PROVIDER_NAME),
                    format!("failed to serialize tool parameters: {error}"),
                    None,
                )
            })?;
            Ok(ChatTool {
                r#type: "function".to_owned(),
                function: ChatToolFunction {
                    name: name.clone(),
                    description: description.clone(),
                    parameters: Some(parameters),
                    strict: *strict,
                },
            })
        }
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

// ── Response conversion ─────────────────────────────────────────────────────

pub(crate) fn response_to_generate_result(
    response: ChatCompletionResponse,
    request_headers: Option<HeaderMap>,
    request_body: JsonValue,
    response_headers: Option<HeaderMap>,
    response_body: JsonValue,
) -> Result<LanguageModelGenerateResult> {
    let Some(choice) = response
        .choices
        .into_iter()
        .find(|choice| choice.index == 0)
    else {
        return Err(BitrouterError::invalid_response(
            Some(OPENAI_PROVIDER_NAME),
            "chat completion response did not contain choice 0",
            Some(response_body),
        ));
    };

    let provider_metadata = openai_metadata(
        response.system_fingerprint.clone(),
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
        usage: response
            .usage
            .map(usage_to_language_model)
            .unwrap_or_else(empty_usage),
        provider_metadata,
        request: Some(LanguageModelRawRequest {
            headers: request_headers,
            body: request_body,
        }),
        response_metadata: Some(LanguageModelRawResponse {
            id: Some(response.id),
            timestamp: Some(response.created.saturating_mul(1_000)),
            model_id: Some(response.model),
            headers: response_headers,
            body: Some(response_body),
        }),
        warnings: Some(Vec::<Warning>::new()),
    })
}

// ── Request building ────────────────────────────────────────────────────────

pub(crate) fn build_chat_request(
    model_id: &str,
    options: &LanguageModelCallOptions,
    stream: bool,
) -> Result<ChatCompletionRequest> {
    let model = model_id.to_owned();
    if options.top_k.is_some() {
        return Err(BitrouterError::unsupported(
            OPENAI_PROVIDER_NAME,
            "top_k",
            Some("OpenAI chat completions does not expose top_k sampling".to_owned()),
        ));
    }

    let tools: Option<Vec<ChatTool>> = options
        .tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .map(tool_from_language_model)
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?;
    let has_tools = tools.as_ref().is_some_and(|tools| !tools.is_empty());

    Ok(ChatCompletionRequest {
        model,
        messages: convert_prompt(&options.prompt)?,
        stream: Some(stream),
        stream_options: stream.then_some(ChatCompletionStreamOptions {
            include_usage: Some(true),
        }),
        max_completion_tokens: options.max_output_tokens,
        max_tokens: None,
        temperature: options.temperature,
        top_p: options.top_p,
        stop: options.stop_sequences.clone(),
        presence_penalty: options.presence_penalty,
        frequency_penalty: options.frequency_penalty,
        response_format: options
            .response_format
            .as_ref()
            .map(response_format_from_language_model),
        seed: options.seed,
        tools,
        tool_choice: options
            .tool_choice
            .as_ref()
            .map(tool_choice_from_language_model),
        parallel_tool_calls: has_tools.then_some(false),
    })
}

// ── Error parsing ───────────────────────────────────────────────────────────

pub(super) fn parse_openai_error(
    status_code: u16,
    request_id: Option<String>,
    body: Option<JsonValue>,
) -> BitrouterError {
    let parsed = body
        .as_ref()
        .and_then(|body| serde_json::from_value::<ChatCompletionErrorEnvelope>(body.clone()).ok());

    match parsed {
        Some(envelope) => BitrouterError::provider_error(
            OPENAI_PROVIDER_NAME,
            envelope.error.message,
            ProviderErrorContext {
                status_code: Some(status_code),
                error_type: envelope.error.error_type,
                code: envelope.error.code.and_then(json_value_to_string),
                param: envelope.error.param,
                request_id,
                body,
            },
        ),
        None => BitrouterError::provider_error(
            OPENAI_PROVIDER_NAME,
            format!("OpenAI returned HTTP {status_code}"),
            ProviderErrorContext {
                status_code: Some(status_code),
                error_type: None,
                code: None,
                param: None,
                request_id,
                body,
            },
        ),
    }
}

// ── Message / prompt conversion ─────────────────────────────────────────────

fn message_to_language_model_content(
    message: ChatCompletionChoiceMessage,
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
            // len() == 1 is guaranteed here because len() != 1 returns early above.
            // The else branch is a defensive fallback that cannot be reached.
            let Some(tool_call) = tool_calls.into_iter().next() else {
                return Err(BitrouterError::invalid_response(
                    Some(OPENAI_PROVIDER_NAME),
                    "expected single tool call but iterator was empty",
                    Some(response_body),
                ));
            };
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

fn convert_prompt(prompt: &[LanguageModelMessage]) -> Result<Vec<ChatMessage>> {
    let mut messages = Vec::new();

    for message in prompt {
        match message {
            LanguageModelMessage::System { content, .. } => {
                messages.push(ChatMessage {
                    role: "system".to_owned(),
                    content: Some(ChatMessageContent::Text(content.clone())),
                    tool_call_id: None,
                    tool_calls: None,
                    name: None,
                });
            }
            LanguageModelMessage::User { content, .. } => {
                messages.push(ChatMessage {
                    role: "user".to_owned(),
                    content: Some(convert_user_content(content)?),
                    tool_call_id: None,
                    tool_calls: None,
                    name: None,
                });
            }
            LanguageModelMessage::Assistant { content, .. } => {
                let mut text_segments = Vec::new();
                let mut tool_calls = Vec::new();

                for item in content {
                    match item {
                        LanguageModelAssistantContent::Text { text, .. } => {
                            text_segments.push(text.clone());
                        }
                        LanguageModelAssistantContent::ToolCall {
                            tool_call_id,
                            tool_name,
                            input,
                            ..
                        } => {
                            tool_calls.push(ChatMessageToolCall {
                                id: tool_call_id.clone(),
                                r#type: "function".to_owned(),
                                function: ChatMessageToolCallFunction {
                                    name: tool_name.clone(),
                                    arguments: serde_json::to_string(input).map_err(|error| {
                                        BitrouterError::invalid_request(
                                            Some(OPENAI_PROVIDER_NAME),
                                            format!("failed to serialize assistant tool call input: {error}"),
                                            None,
                                        )
                                    })?,
                                },
                            });
                        }
                        LanguageModelAssistantContent::Reasoning { .. } => {
                            return Err(BitrouterError::unsupported(
                                OPENAI_PROVIDER_NAME,
                                "assistant reasoning prompt parts",
                                Some("Chat completions does not expose a dedicated reasoning message part".to_owned()),
                            ));
                        }
                        LanguageModelAssistantContent::File { .. } => {
                            return Err(BitrouterError::unsupported(
                                OPENAI_PROVIDER_NAME,
                                "assistant file prompt parts",
                                None,
                            ));
                        }
                        LanguageModelAssistantContent::ToolResult { .. } => {
                            return Err(BitrouterError::unsupported(
                                OPENAI_PROVIDER_NAME,
                                "assistant tool-result prompt parts",
                                Some("Use tool role messages for tool outputs".to_owned()),
                            ));
                        }
                    }
                }

                let content_text = (!text_segments.is_empty())
                    .then(|| ChatMessageContent::Text(text_segments.join("\n")));
                messages.push(ChatMessage {
                    role: "assistant".to_owned(),
                    content: content_text,
                    tool_call_id: None,
                    tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
                    name: None,
                });
            }
            LanguageModelMessage::Tool { content, .. } => {
                for item in content {
                    match item {
                        LanguageModelToolResult::ToolResult {
                            tool_call_id,
                            output,
                            ..
                        } => {
                            messages.push(ChatMessage {
                                role: "tool".to_owned(),
                                content: Some(ChatMessageContent::Text(stringify_tool_output(
                                    output,
                                )?)),
                                tool_call_id: Some(tool_call_id.clone()),
                                tool_calls: None,
                                name: None,
                            });
                        }
                        LanguageModelToolResult::ToolApprovalResponse { .. } => {
                            return Err(BitrouterError::unsupported(
                                OPENAI_PROVIDER_NAME,
                                "tool approval responses",
                                None,
                            ));
                        }
                    }
                }
            }
        }
    }

    Ok(messages)
}

fn convert_user_content(content: &[LanguageModelUserContent]) -> Result<ChatMessageContent> {
    if content.len() == 1
        && let LanguageModelUserContent::Text { text, .. } = &content[0]
    {
        return Ok(ChatMessageContent::Text(text.clone()));
    }

    let mut parts = Vec::new();
    for item in content {
        match item {
            LanguageModelUserContent::Text { text, .. } => {
                parts.push(ChatContentPart::Text { text: text.clone() });
            }
            LanguageModelUserContent::File {
                data, media_type, ..
            } => {
                parts.push(ChatContentPart::ImageUrl {
                    image_url: ChatImageUrl {
                        url: convert_image_input(data, media_type)?,
                    },
                });
            }
        }
    }

    Ok(ChatMessageContent::Parts(parts))
}

fn convert_image_input(data: &LanguageModelDataContent, media_type: &str) -> Result<String> {
    if !media_type.starts_with("image/") {
        return Err(BitrouterError::unsupported(
            OPENAI_PROVIDER_NAME,
            format!("user file content with media type {media_type}"),
            Some("OpenAI chat completions only accepts image multimodal parts here".to_owned()),
        ));
    }

    match data {
        LanguageModelDataContent::Url(url) => Ok(url.clone()),
        LanguageModelDataContent::Bytes(bytes) => Ok(format!(
            "data:{media_type};base64,{}",
            BASE64_STANDARD.encode(bytes)
        )),
        LanguageModelDataContent::String(value) => {
            if value.starts_with("http://")
                || value.starts_with("https://")
                || value.starts_with("data:")
            {
                Ok(value.clone())
            } else {
                Ok(format!("data:{media_type};base64,{value}"))
            }
        }
    }
}

fn stringify_tool_output(output: &LanguageModelToolResultOutput) -> Result<String> {
    match output {
        LanguageModelToolResultOutput::Text { value, .. } => Ok(value.clone()),
        LanguageModelToolResultOutput::Json { value, .. }
        | LanguageModelToolResultOutput::ErrorJson { value, .. } => serde_json::to_string(value)
            .map_err(|error| {
                BitrouterError::invalid_request(
                    Some(OPENAI_PROVIDER_NAME),
                    format!("failed to serialize tool output JSON: {error}"),
                    None,
                )
            }),
        LanguageModelToolResultOutput::ExecutionDenied { reason, .. }
        | LanguageModelToolResultOutput::ErrorText { value: reason, .. } => Ok(reason.clone()),
        LanguageModelToolResultOutput::Content { value, .. } => serde_json::to_string(
            &JsonValue::Array(value.iter().map(tool_output_content_to_json).collect()),
        )
        .map_err(|error| {
            BitrouterError::invalid_request(
                Some(OPENAI_PROVIDER_NAME),
                format!("failed to serialize content-style tool output: {error}"),
                None,
            )
        }),
    }
}

fn tool_output_content_to_json(content: &LanguageModelToolResultOutputContent) -> JsonValue {
    match content {
        LanguageModelToolResultOutputContent::Text { text, .. } => {
            json!({ "type": "text", "text": text })
        }
        LanguageModelToolResultOutputContent::FileData {
            filename,
            data,
            media_type,
            ..
        } => json!({
            "type": "file-data",
            "filename": filename,
            "data": data,
            "media_type": media_type,
        }),
        LanguageModelToolResultOutputContent::FileUrl { url, .. } => {
            json!({ "type": "file-url", "url": url })
        }
        LanguageModelToolResultOutputContent::FileId { id, .. } => json!({
            "type": "file-id",
            "id": file_id_to_json(id),
        }),
        LanguageModelToolResultOutputContent::ImageData {
            data, media_type, ..
        } => json!({
            "type": "image-data",
            "data": data,
            "media_type": media_type,
        }),
        LanguageModelToolResultOutputContent::ImageUrl { url, .. } => {
            json!({ "type": "image-url", "url": url })
        }
        LanguageModelToolResultOutputContent::ImageFileId { id, .. } => json!({
            "type": "image-file-id",
            "id": file_id_to_json(id),
        }),
        LanguageModelToolResultOutputContent::ProviderSpecific { .. } => {
            json!({ "type": "provider-specific" })
        }
    }
}

fn file_id_to_json(id: &LanguageModelToolResultOutputContentFileId) -> JsonValue {
    match id {
        LanguageModelToolResultOutputContentFileId::Record(record) => json!(record),
        LanguageModelToolResultOutputContentFileId::String(value) => {
            JsonValue::String(value.clone())
        }
    }
}

// ── SSE parser ──────────────────────────────────────────────────────────────

#[derive(Default)]
pub(super) struct OpenAiSseParser {
    buffer: Vec<u8>,
    state: OpenAiStreamState,
    include_raw_chunks: bool,
}

impl OpenAiSseParser {
    pub(super) fn new(include_raw_chunks: bool) -> Self {
        Self {
            include_raw_chunks,
            ..Self::default()
        }
    }

    pub(super) fn is_finished(&self) -> bool {
        self.state.finished
    }

    pub(super) fn push_bytes(&mut self, bytes: &[u8]) -> Vec<LanguageModelStreamPart> {
        self.buffer.extend_from_slice(bytes);
        let mut parts = Vec::new();

        while let Some((event_len, separator_len)) = next_sse_event_boundary(&self.buffer) {
            let event_bytes = self.buffer[..event_len].to_vec();
            self.buffer.drain(..event_len + separator_len);

            if event_bytes.is_empty() {
                continue;
            }

            match String::from_utf8(event_bytes) {
                Ok(event) => {
                    if let Some(payload) = extract_sse_data(&event) {
                        parts.extend(self.parse_payload(payload));
                        if self.state.finished {
                            break;
                        }
                    }
                }
                Err(error) => {
                    parts.push(LanguageModelStreamPart::Error {
                        error: json!({
                            "provider": OPENAI_PROVIDER_NAME,
                            "kind": "stream_protocol",
                            "message": error.to_string(),
                        }),
                    });
                    self.state.finished = true;
                    break;
                }
            }
        }

        parts
    }

    pub(super) fn finish(&mut self) -> Vec<LanguageModelStreamPart> {
        if self.state.finished {
            return Vec::new();
        }

        if !self.buffer.is_empty() {
            if let Ok(event) = String::from_utf8(self.buffer.clone())
                && let Some(payload) = extract_sse_data(&event)
            {
                let mut parts = self.parse_payload(payload);
                parts.extend(self.state.finish_parts());
                self.buffer.clear();
                return parts;
            }
            self.buffer.clear();
        }

        self.state.finish_parts()
    }

    fn parse_payload(&mut self, payload: String) -> Vec<LanguageModelStreamPart> {
        if payload == "[DONE]" {
            return self.state.finish_parts();
        }

        let raw_value = match serde_json::from_str::<JsonValue>(&payload) {
            Ok(value) => value,
            Err(error) => {
                self.state.finished = true;
                return vec![LanguageModelStreamPart::Error {
                    error: json!({
                        "provider": OPENAI_PROVIDER_NAME,
                        "kind": "stream_protocol",
                        "message": error.to_string(),
                        "raw": payload,
                    }),
                }];
            }
        };

        let mut parts = Vec::new();
        if self.include_raw_chunks {
            parts.push(LanguageModelStreamPart::Raw {
                raw_value: raw_value.clone(),
            });
        }

        if let Ok(error_envelope) =
            serde_json::from_value::<ChatCompletionErrorEnvelope>(raw_value.clone())
        {
            self.state.finished = true;
            parts.push(LanguageModelStreamPart::Error {
                error: json!({
                    "message": error_envelope.error.message,
                    "type": error_envelope.error.error_type,
                    "param": error_envelope.error.param,
                    "code": error_envelope.error.code,
                }),
            });
            return parts;
        }

        // Some providers (e.g. GitHub Copilot) omit the `created` field.
        // Fill it in with the current timestamp before typed deserialization.
        let mut raw_value = raw_value;
        if let Some(obj) = raw_value.as_object_mut() {
            obj.entry("created").or_insert_with(|| {
                json!(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64
                )
            });
        }

        let chunk: ChatCompletionChunk = match serde_json::from_value(raw_value.clone()) {
            Ok(chunk) => chunk,
            Err(error) => {
                self.state.finished = true;
                parts.push(LanguageModelStreamPart::Error {
                    error: json!({
                        "provider": OPENAI_PROVIDER_NAME,
                        "kind": "response_decode",
                        "message": error.to_string(),
                        "raw": raw_value,
                    }),
                });
                return parts;
            }
        };

        parts.extend(self.state.apply_chunk(chunk));
        parts
    }
}

#[derive(Default)]
struct OpenAiStreamState {
    metadata_emitted: bool,
    text_started: bool,
    tool_inputs: HashMap<u32, OpenAiToolInputState>,
    usage: Option<LanguageModelUsage>,
    finish_reason:
        Option<bitrouter_core::models::language::finish_reason::LanguageModelFinishReason>,
    finished: bool,
}

#[derive(Default)]
struct OpenAiToolInputState {
    id: Option<String>,
    name: Option<String>,
    started: bool,
    buffered_delta: String,
}

impl OpenAiStreamState {
    fn apply_chunk(&mut self, chunk: ChatCompletionChunk) -> Vec<LanguageModelStreamPart> {
        let mut parts = Vec::new();

        if !self.metadata_emitted {
            parts.push(LanguageModelStreamPart::ResponseMetadata {
                id: Some(chunk.id.clone()),
                timestamp: Some(chunk.created.saturating_mul(1_000)),
                model_id: Some(chunk.model.clone()),
            });
            self.metadata_emitted = true;
        }

        if let Some(usage) = chunk.usage {
            self.usage = Some(usage_to_language_model(usage));
        }

        for choice in chunk.choices {
            if choice.index != 0 {
                continue;
            }

            if let Some(content) = choice.delta.content {
                if !self.text_started {
                    parts.push(LanguageModelStreamPart::TextStart {
                        id: STREAM_TEXT_ID.to_owned(),
                        provider_metadata: None,
                    });
                    self.text_started = true;
                }
                parts.push(LanguageModelStreamPart::TextDelta {
                    id: STREAM_TEXT_ID.to_owned(),
                    delta: content,
                    provider_metadata: None,
                });
            }

            if let Some(tool_calls) = choice.delta.tool_calls {
                for tool_call in tool_calls {
                    parts.extend(self.apply_tool_delta(tool_call));
                }
            }

            if let Some(finish_reason) = choice.finish_reason.as_deref() {
                self.finish_reason = Some(map_finish_reason(Some(finish_reason)));
            }
        }

        parts
    }

    fn apply_tool_delta(
        &mut self,
        tool_call: ChatResponseToolCallDelta,
    ) -> Vec<LanguageModelStreamPart> {
        let entry = self.tool_inputs.entry(tool_call.index).or_default();
        if let Some(id) = tool_call.id {
            entry.id = Some(id);
        }

        if let Some(function) = tool_call.function {
            if let Some(name) = function.name {
                entry.name = Some(name);
            }
            if let Some(arguments) = function.arguments {
                entry.buffered_delta.push_str(&arguments);
            }
        }

        let mut parts = Vec::new();
        if !entry.started {
            if let (Some(id), Some(name)) = (entry.id.clone(), entry.name.clone()) {
                parts.push(LanguageModelStreamPart::ToolInputStart {
                    id: id.clone(),
                    tool_name: name,
                    provider_executed: None,
                    dynamic: None,
                    title: None,
                    provider_metadata: None,
                });
                entry.started = true;
                if !entry.buffered_delta.is_empty() {
                    parts.push(LanguageModelStreamPart::ToolInputDelta {
                        id,
                        delta: std::mem::take(&mut entry.buffered_delta),
                        provider_metadata: None,
                    });
                }
            }
        } else if !entry.buffered_delta.is_empty() {
            parts.push(LanguageModelStreamPart::ToolInputDelta {
                id: entry
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("tool-{}", tool_call.index)),
                delta: std::mem::take(&mut entry.buffered_delta),
                provider_metadata: None,
            });
        }

        parts
    }

    fn finish_parts(&mut self) -> Vec<LanguageModelStreamPart> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;

        let mut parts = Vec::new();
        if self.text_started {
            parts.push(LanguageModelStreamPart::TextEnd {
                id: STREAM_TEXT_ID.to_owned(),
                provider_metadata: None,
            });
        }

        let mut tool_indices = self.tool_inputs.keys().copied().collect::<Vec<_>>();
        tool_indices.sort_unstable();
        for index in tool_indices {
            if let Some(tool_state) = self.tool_inputs.get(&index)
                && tool_state.started
            {
                parts.push(LanguageModelStreamPart::ToolInputEnd {
                    id: tool_state
                        .id
                        .clone()
                        .unwrap_or_else(|| format!("tool-{index}")),
                    provider_metadata: None,
                });
            }
        }

        parts.push(LanguageModelStreamPart::Finish {
            usage: self.usage.clone().unwrap_or_else(empty_usage),
            finish_reason: self
                .finish_reason
                .clone()
                .unwrap_or_else(|| map_finish_reason(Some("stop"))),
            provider_metadata: None,
        });
        parts
    }
}

/// A boxed byte stream used by the SSE driver, abstracting over the transport.
pub(crate) type ByteStream = Pin<
    Box<
        dyn Stream<Item = std::result::Result<Bytes, Box<dyn std::error::Error + Send + Sync>>>
            + Send,
    >,
>;

/// Reads chunks from `bytes_stream`, parses SSE events, and forwards
/// [`LanguageModelStreamPart`]s into `sender`.  Respects `abort_signal`.
pub(crate) async fn drive_sse_stream(
    mut bytes_stream: ByteStream,
    abort_signal: Option<CancellationToken>,
    sender: mpsc::Sender<LanguageModelStreamPart>,
    include_raw_chunks: bool,
) {
    let mut parser = OpenAiSseParser::new(include_raw_chunks);
    if send_stream_part(
        &sender,
        LanguageModelStreamPart::StreamStart {
            warnings: Vec::<Warning>::new(),
        },
    )
    .await
    .is_err()
    {
        return;
    }

    loop {
        let next_chunk = if let Some(token) = abort_signal.as_ref() {
            select! {
                _ = token.cancelled() => {
                    let _ = send_stream_part(
                        &sender,
                        LanguageModelStreamPart::Error {
                            error: json!({
                                "provider": OPENAI_PROVIDER_NAME,
                                "kind": "cancelled",
                                "message": "streaming chat completion was cancelled",
                            }),
                        },
                    ).await;
                    return;
                }
                chunk = bytes_stream.next() => chunk,
            }
        } else {
            bytes_stream.next().await
        };

        match next_chunk {
            Some(Ok(chunk)) => {
                for part in parser.push_bytes(&chunk) {
                    if send_stream_part(&sender, part).await.is_err() {
                        return;
                    }
                }
                if parser.is_finished() {
                    return;
                }
            }
            Some(Err(error)) => {
                let _ = send_stream_part(
                    &sender,
                    LanguageModelStreamPart::Error {
                        error: json!({
                            "provider": OPENAI_PROVIDER_NAME,
                            "kind": "transport",
                            "message": error.to_string(),
                        }),
                    },
                )
                .await;
                return;
            }
            None => {
                for part in parser.finish() {
                    if send_stream_part(&sender, part).await.is_err() {
                        return;
                    }
                }
                return;
            }
        }
    }
}

async fn send_stream_part(
    sender: &mpsc::Sender<LanguageModelStreamPart>,
    part: LanguageModelStreamPart,
) -> std::result::Result<(), ()> {
    sender.send(part).await.map_err(|_| ())
}

fn extract_sse_data(event: &str) -> Option<String> {
    let data = event
        .lines()
        .filter_map(|line| {
            let line = line.trim_end_matches('\r');
            line.strip_prefix("data:")
                .map(|rest| rest.strip_prefix(' ').unwrap_or(rest).to_owned())
        })
        .collect::<Vec<_>>();

    (!data.is_empty()).then(|| data.join("\n"))
}

fn next_sse_event_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    for index in 0..buffer.len().saturating_sub(1) {
        if buffer[index] == b'\n' && buffer[index + 1] == b'\n' {
            return Some((index, 2));
        }
        if index + 3 < buffer.len()
            && buffer[index] == b'\r'
            && buffer[index + 1] == b'\n'
            && buffer[index + 2] == b'\r'
            && buffer[index + 3] == b'\n'
        {
            return Some((index, 4));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_core::models::language::{
        call_options::LanguageModelCallOptions,
        data_content::LanguageModelDataContent,
        prompt::{LanguageModelMessage, LanguageModelUserContent},
    };

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
            BitrouterError::Provider { context, .. } => {
                assert_eq!(context.status_code, Some(429));
                assert_eq!(context.code.as_deref(), Some("rate_limit_exceeded"));
                assert_eq!(context.request_id.as_deref(), Some("req_123"));
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn builds_image_prompt_request() {
        let request = build_chat_request(
            "gpt-4o-mini",
            &LanguageModelCallOptions {
                prompt: vec![LanguageModelMessage::User {
                    content: vec![
                        LanguageModelUserContent::Text {
                            text: "describe this".to_owned(),
                            provider_options: None,
                        },
                        LanguageModelUserContent::File {
                            filename: None,
                            data: LanguageModelDataContent::Url(
                                "https://example.com/image.png".to_owned(),
                            ),
                            media_type: "image/png".to_owned(),
                            provider_options: None,
                        },
                    ],
                    provider_options: None,
                }],
                stream: None,
                max_output_tokens: None,
                temperature: None,
                top_p: None,
                top_k: None,
                stop_sequences: None,
                presence_penalty: None,
                frequency_penalty: None,
                response_format: None,
                seed: None,
                tools: None,
                tool_choice: None,
                include_raw_chunks: None,
                abort_signal: None,
                headers: None,
                provider_options: None,
            },
            false,
        )
        .expect("request should build");

        assert_eq!(request.messages[0].role, "user");
    }

    // ── SSE parser unit tests ──────────────────────────────────────────

    fn sse_event(data: &str) -> Vec<u8> {
        format!("data: {data}\n\n").into_bytes()
    }

    #[test]
    fn sse_parser_text_stream() {
        let mut parser = OpenAiSseParser::new(false);

        let chunk1 = json!({
            "id": "c1", "created": 1, "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {"content": "Hello"}, "finish_reason": null}]
        });
        let chunk2 = json!({
            "id": "c1", "created": 1, "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {"content": " world"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
        });

        let parts = parser.push_bytes(&sse_event(&chunk1.to_string()));
        assert!(
            matches!(&parts[0], LanguageModelStreamPart::ResponseMetadata { id, .. } if id.as_deref() == Some("c1"))
        );
        assert!(matches!(
            &parts[1],
            LanguageModelStreamPart::TextStart { .. }
        ));
        assert!(
            matches!(&parts[2], LanguageModelStreamPart::TextDelta { delta, .. } if delta == "Hello")
        );

        let parts = parser.push_bytes(&sse_event(&chunk2.to_string()));
        assert!(
            matches!(&parts[0], LanguageModelStreamPart::TextDelta { delta, .. } if delta == " world")
        );

        let done_parts = parser.push_bytes(&sse_event("[DONE]"));
        // [DONE] triggers finish_parts
        assert!(
            done_parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::TextEnd { .. }))
        );
        assert!(
            done_parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::Finish { .. }))
        );
        assert!(parser.is_finished());
    }

    #[test]
    fn sse_parser_tool_call_stream() {
        let mut parser = OpenAiSseParser::new(false);

        let chunk1 = json!({
            "id": "c2", "created": 1, "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {
                "tool_calls": [{"index": 0, "id": "call_a", "type": "function",
                    "function": {"name": "search", "arguments": ""}}]
            }, "finish_reason": null}]
        });
        let chunk2 = json!({
            "id": "c2", "created": 1, "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {
                "tool_calls": [{"index": 0, "function": {"arguments": "{\"q\":"}}]
            }, "finish_reason": null}]
        });
        let chunk3 = json!({
            "id": "c2", "created": 1, "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {
                "tool_calls": [{"index": 0, "function": {"arguments": "\"hi\"}"}}]
            }, "finish_reason": "tool_calls"}]
        });

        let parts = parser.push_bytes(&sse_event(&chunk1.to_string()));
        assert!(parts.iter().any(|p| matches!(p, LanguageModelStreamPart::ToolInputStart { tool_name, .. } if tool_name == "search")));

        let parts = parser.push_bytes(&sse_event(&chunk2.to_string()));
        assert!(parts.iter().any(|p| matches!(p, LanguageModelStreamPart::ToolInputDelta { delta, .. } if delta == "{\"q\":")));

        let parts = parser.push_bytes(&sse_event(&chunk3.to_string()));
        assert!(parts.iter().any(|p| matches!(p, LanguageModelStreamPart::ToolInputDelta { delta, .. } if delta == "\"hi\"}")));

        let done_parts = parser.push_bytes(&sse_event("[DONE]"));
        assert!(
            done_parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::ToolInputEnd { .. }))
        );
        assert!(done_parts.iter().any(|p| matches!(p, LanguageModelStreamPart::Finish { finish_reason, .. }
            if matches!(finish_reason, bitrouter_core::models::language::finish_reason::LanguageModelFinishReason::FunctionCall)
        )));
    }

    #[test]
    fn sse_parser_handles_error_envelope() {
        let mut parser = OpenAiSseParser::new(false);

        let error = json!({
            "error": {
                "message": "Server overloaded",
                "type": "server_error",
                "param": null,
                "code": null
            }
        });
        let parts = parser.push_bytes(&sse_event(&error.to_string()));
        assert!(parser.is_finished());
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::Error { error }
                    if error["message"] == "Server overloaded"
                ))
        );
    }

    #[test]
    fn sse_parser_incremental_byte_delivery() {
        let mut parser = OpenAiSseParser::new(false);

        let full_event = sse_event(
            &json!({
                "id": "c3", "created": 1, "model": "gpt-4o",
                "choices": [{"index": 0, "delta": {"content": "Hi"}, "finish_reason": null}]
            })
            .to_string(),
        );

        // Feed bytes one at a time — parser should buffer until a full event arrives
        let mut accumulated = Vec::new();
        for &byte in &full_event[..full_event.len() - 1] {
            let parts = parser.push_bytes(&[byte]);
            accumulated.extend(parts);
        }
        // No parts should have been emitted yet (event boundary not reached)
        assert!(accumulated.is_empty());

        // Feed the last byte to complete the event
        let parts = parser.push_bytes(&[*full_event.last().unwrap()]);
        assert!(parts.iter().any(
            |p| matches!(p, LanguageModelStreamPart::TextDelta { delta, .. } if delta == "Hi")
        ));
    }

    #[test]
    fn sse_parser_raw_chunks_when_enabled() {
        let mut parser = OpenAiSseParser::new(true);
        let chunk = json!({
            "id": "c4", "created": 1, "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {"content": "X"}, "finish_reason": null}]
        });
        let parts = parser.push_bytes(&sse_event(&chunk.to_string()));
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::Raw { .. }))
        );
    }

    #[test]
    fn sse_parser_crlf_events() {
        let mut parser = OpenAiSseParser::new(false);
        let chunk = json!({
            "id": "c5", "created": 1, "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {"content": "ok"}, "finish_reason": null}]
        });
        let event = format!("data: {}\r\n\r\n", chunk);
        let parts = parser.push_bytes(event.as_bytes());
        assert!(parts.iter().any(
            |p| matches!(p, LanguageModelStreamPart::TextDelta { delta, .. } if delta == "ok")
        ));
    }

    #[test]
    fn sse_parser_finish_flushes_remaining_buffer() {
        let mut parser = OpenAiSseParser::new(false);
        let chunk = json!({
            "id": "c6", "created": 1, "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {"content": "last"}, "finish_reason": "stop"}]
        });
        // Push event without the trailing \n\n (simulate connection drop mid-event)
        let partial = format!("data: {}", chunk);
        let parts = parser.push_bytes(partial.as_bytes());
        assert!(parts.is_empty(), "no event boundary yet");

        let final_parts = parser.finish();
        assert!(final_parts.iter().any(
            |p| matches!(p, LanguageModelStreamPart::TextDelta { delta, .. } if delta == "last")
        ));
        assert!(
            final_parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::Finish { .. }))
        );
    }

    // ── drive_sse_stream integration tests ─────────────────────────────

    fn make_byte_stream(chunks: Vec<Vec<u8>>) -> ByteStream {
        Box::pin(tokio_stream::iter(chunks.into_iter().map(|c| {
            Ok(Bytes::from(c))
                as std::result::Result<Bytes, Box<dyn std::error::Error + Send + Sync>>
        })))
    }

    async fn collect_parts(
        bytes_stream: ByteStream,
        abort_signal: Option<CancellationToken>,
        include_raw: bool,
    ) -> Vec<LanguageModelStreamPart> {
        let (sender, mut receiver) = mpsc::channel(64);
        tokio::spawn(drive_sse_stream(
            bytes_stream,
            abort_signal,
            sender,
            include_raw,
        ));
        let mut parts = Vec::new();
        while let Some(part) = receiver.recv().await {
            parts.push(part);
        }
        parts
    }

    #[tokio::test]
    async fn drive_stream_text_completion() {
        let chunk1 = json!({
            "id": "s1", "created": 1, "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {"content": "Hello"}, "finish_reason": null}]
        });
        let chunk2 = json!({
            "id": "s1", "created": 1, "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {"content": " world"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        });

        let events = vec![
            sse_event(&chunk1.to_string()),
            sse_event(&chunk2.to_string()),
            sse_event("[DONE]"),
        ];

        let parts = collect_parts(make_byte_stream(events), None, false).await;

        assert!(matches!(
            &parts[0],
            LanguageModelStreamPart::StreamStart { .. }
        ));
        assert!(matches!(
            &parts[1],
            LanguageModelStreamPart::ResponseMetadata { .. }
        ));
        assert!(matches!(
            &parts[2],
            LanguageModelStreamPart::TextStart { .. }
        ));
        assert!(
            matches!(&parts[3], LanguageModelStreamPart::TextDelta { delta, .. } if delta == "Hello")
        );
        assert!(
            matches!(&parts[4], LanguageModelStreamPart::TextDelta { delta, .. } if delta == " world")
        );
        assert!(matches!(&parts[5], LanguageModelStreamPart::TextEnd { .. }));
        assert!(matches!(&parts[6], LanguageModelStreamPart::Finish { .. }));
    }

    #[tokio::test]
    async fn drive_stream_transport_error() {
        let chunk = sse_event(
            &json!({
                "id": "e1", "created": 1, "model": "gpt-4o",
                "choices": [{"index": 0, "delta": {"content": "ok"}, "finish_reason": null}]
            })
            .to_string(),
        );

        let items: Vec<std::result::Result<Bytes, Box<dyn std::error::Error + Send + Sync>>> = vec![
            Ok(Bytes::from(chunk)),
            Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "connection reset",
            ))),
        ];
        let stream: ByteStream = Box::pin(tokio_stream::iter(items));

        let parts = collect_parts(stream, None, false).await;
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::Error { error }
                    if error["kind"] == "transport"
                ))
        );
    }

    #[tokio::test]
    async fn drive_stream_parallel_handling() {
        let make_events = |id: &str, text: &str| {
            let chunk = json!({
                "id": id, "created": 1, "model": "gpt-4o",
                "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}]
            });
            vec![sse_event(&chunk.to_string()), sse_event("[DONE]")]
        };

        let (parts_a, parts_b) = tokio::join!(
            collect_parts(make_byte_stream(make_events("a", "alpha")), None, false),
            collect_parts(make_byte_stream(make_events("b", "beta")), None, false),
        );

        // Both streams should complete independently
        assert!(parts_a.iter().any(
            |p| matches!(p, LanguageModelStreamPart::TextDelta { delta, .. } if delta == "alpha")
        ));
        assert!(parts_b.iter().any(
            |p| matches!(p, LanguageModelStreamPart::TextDelta { delta, .. } if delta == "beta")
        ));
        assert!(
            parts_a
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::Finish { .. }))
        );
        assert!(
            parts_b
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::Finish { .. }))
        );
    }

    #[tokio::test]
    async fn drive_stream_cancellation() {
        use tokio_stream::wrappers::ReceiverStream;

        let cancel_token = CancellationToken::new();
        let (byte_tx, byte_rx) = tokio::sync::mpsc::channel::<
            std::result::Result<Bytes, Box<dyn std::error::Error + Send + Sync>>,
        >(16);

        let stream: ByteStream = Box::pin(ReceiverStream::new(byte_rx));
        let (part_tx, mut part_rx) = mpsc::channel(64);

        let token = cancel_token.clone();
        tokio::spawn(drive_sse_stream(stream, Some(token), part_tx, false));

        // Send one valid chunk
        let chunk = json!({
            "id": "cancel", "created": 1, "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {"content": "start"}, "finish_reason": null}]
        });
        byte_tx
            .send(Ok(Bytes::from(sse_event(&chunk.to_string()))))
            .await
            .unwrap();

        // Receive StreamStart + metadata + text parts
        let mut received = Vec::new();
        for _ in 0..4 {
            if let Some(part) = part_rx.recv().await {
                received.push(part);
            }
        }
        assert!(received.iter().any(
            |p| matches!(p, LanguageModelStreamPart::TextDelta { delta, .. } if delta == "start")
        ));

        // Cancel the stream
        cancel_token.cancel();

        // Should receive a cancellation error
        let mut saw_cancel = false;
        while let Some(part) = part_rx.recv().await {
            if matches!(&part, LanguageModelStreamPart::Error { error } if error["kind"] == "cancelled")
            {
                saw_cancel = true;
                break;
            }
        }
        assert!(saw_cancel, "should have received cancellation error");

        // Channel should close after cancellation
        assert!(part_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn drive_stream_with_raw_chunks() {
        let chunk = json!({
            "id": "r1", "created": 1, "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {"content": "hey"}, "finish_reason": "stop"}]
        });
        let events = vec![sse_event(&chunk.to_string()), sse_event("[DONE]")];

        let parts = collect_parts(make_byte_stream(events), None, true).await;
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::Raw { .. }))
        );
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::Finish { .. }))
        );
    }

    #[tokio::test]
    async fn drive_stream_connection_drop() {
        // Stream ends without sending [DONE] — finish() should still produce final parts
        let chunk = json!({
            "id": "d1", "created": 1, "model": "gpt-4o",
            "choices": [{"index": 0, "delta": {"content": "abrupt"}, "finish_reason": "stop"}]
        });
        let events = vec![sse_event(&chunk.to_string())];

        let parts = collect_parts(make_byte_stream(events), None, false).await;
        assert!(parts.iter().any(
            |p| matches!(p, LanguageModelStreamPart::TextDelta { delta, .. } if delta == "abrupt")
        ));
        // Should still get Finish from the parser's finish() call
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::Finish { .. }))
        );
    }
}
