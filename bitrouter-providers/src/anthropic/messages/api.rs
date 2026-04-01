use std::{collections::HashMap, pin::Pin};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use bitrouter_core::{
    errors::{BitrouterError, ProviderErrorContext, Result},
    models::{
        language::{
            call_options::LanguageModelCallOptions,
            content::LanguageModelContent,
            data_content::LanguageModelDataContent,
            generate_result::{
                LanguageModelGenerateResult, LanguageModelRawRequest, LanguageModelRawResponse,
            },
            prompt::{
                LanguageModelAssistantContent, LanguageModelMessage, LanguageModelToolResult,
                LanguageModelToolResultOutput, LanguageModelToolResultOutputContent,
                LanguageModelUserContent,
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

use bitrouter_core::api::anthropic::messages::types::{
    AnthropicContentBlock, AnthropicImageSource, AnthropicMessage, AnthropicMessageContent,
    AnthropicTool, AnthropicToolChoice, MessagesErrorEnvelope, MessagesRequest, MessagesResponse,
    MessagesStreamDelta, MessagesStreamEvent, MessagesUsage,
};

// ── Default max tokens ──────────────────────────────────────────────────────

const DEFAULT_MAX_TOKENS: u32 = 4096;

// ── Constants & helpers (moved from types.rs) ───────────────────────────────

pub(super) const ANTHROPIC_PROVIDER_NAME: &str = "anthropic";
pub(super) const STREAM_TEXT_ID: &str = "text";

pub(super) fn map_finish_reason(
    stop_reason: Option<&str>,
) -> bitrouter_core::models::language::finish_reason::LanguageModelFinishReason {
    use bitrouter_core::models::language::finish_reason::LanguageModelFinishReason;
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

pub(super) fn anthropic_metadata(stop_sequence: Option<String>) -> Option<ProviderMetadata> {
    let mut inner = HashMap::new();
    if let Some(stop_sequence) = stop_sequence {
        inner.insert("stop_sequence".to_owned(), JsonValue::String(stop_sequence));
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

pub(super) fn usage_to_language_model(usage: MessagesUsage) -> LanguageModelUsage {
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

fn tool_choice_from_language_model(choice: &LanguageModelToolChoice) -> AnthropicToolChoice {
    match choice {
        LanguageModelToolChoice::Auto => AnthropicToolChoice::Auto,
        LanguageModelToolChoice::None => AnthropicToolChoice::Auto,
        LanguageModelToolChoice::Required => AnthropicToolChoice::Any,
        LanguageModelToolChoice::Tool { tool_name } => AnthropicToolChoice::Tool {
            name: tool_name.clone(),
        },
    }
}

fn tool_from_language_model(tool: &LanguageModelTool) -> Result<AnthropicTool> {
    match tool {
        LanguageModelTool::Function {
            name,
            description,
            input_schema,
            ..
        } => {
            let schema_value = serde_json::to_value(input_schema).map_err(|error| {
                BitrouterError::invalid_request(
                    Some(ANTHROPIC_PROVIDER_NAME),
                    format!("failed to serialize tool input schema: {error}"),
                    None,
                )
            })?;
            Ok(AnthropicTool {
                name: name.clone(),
                description: description.clone(),
                input_schema: schema_value,
            })
        }
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

// ── Response conversion ─────────────────────────────────────────────────────

pub(super) fn response_to_generate_result(
    response: MessagesResponse,
    request_headers: Option<HeaderMap>,
    request_body: JsonValue,
    response_headers: Option<HeaderMap>,
    response_body: JsonValue,
) -> Result<LanguageModelGenerateResult> {
    let provider_metadata = anthropic_metadata(response.stop_sequence.clone());
    let finish_reason = map_finish_reason(response.stop_reason.as_deref());
    let content = content_blocks_to_language_model_content(
        response.content,
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
            timestamp: None,
            model_id: Some(response.model),
            headers: response_headers,
            body: Some(response_body),
        }),
        warnings: Some(Vec::<Warning>::new()),
    })
}

// ── Request building ────────────────────────────────────────────────────────

pub(super) fn build_messages_request(
    model_id: &str,
    options: &LanguageModelCallOptions,
    stream: bool,
) -> Result<MessagesRequest> {
    let model = model_id.to_owned();
    let mut warnings = Vec::new();

    if options.presence_penalty.is_some() {
        warnings.push(Warning::Unsupported {
            feature: "presence_penalty".to_owned(),
            details: Some("Anthropic messages API does not support presence_penalty".to_owned()),
        });
    }
    if options.frequency_penalty.is_some() {
        warnings.push(Warning::Unsupported {
            feature: "frequency_penalty".to_owned(),
            details: Some("Anthropic messages API does not support frequency_penalty".to_owned()),
        });
    }
    if options.seed.is_some() {
        warnings.push(Warning::Unsupported {
            feature: "seed".to_owned(),
            details: Some("Anthropic messages API does not support seed".to_owned()),
        });
    }
    if options.response_format.is_some() {
        warnings.push(Warning::Unsupported {
            feature: "response_format".to_owned(),
            details: Some(
                "Anthropic messages API does not support response_format directly".to_owned(),
            ),
        });
    }

    let tools: Option<Vec<AnthropicTool>> = options
        .tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .map(tool_from_language_model)
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?;

    let (system, messages) = convert_prompt(&options.prompt)?;

    Ok(MessagesRequest {
        model,
        messages,
        max_tokens: options.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        system: system.map(bitrouter_core::api::anthropic::messages::types::SystemPrompt::Text),
        stream: Some(stream),
        temperature: options.temperature,
        top_p: options.top_p,
        top_k: options.top_k,
        stop_sequences: options.stop_sequences.clone(),
        tools,
        tool_choice: options
            .tool_choice
            .as_ref()
            .map(tool_choice_from_language_model),
        metadata: None,
    })
}

// ── Error parsing ───────────────────────────────────────────────────────────

pub(super) fn parse_anthropic_error(
    status_code: u16,
    request_id: Option<String>,
    body: Option<JsonValue>,
) -> BitrouterError {
    let parsed = body
        .as_ref()
        .and_then(|body| serde_json::from_value::<MessagesErrorEnvelope>(body.clone()).ok());

    match parsed {
        Some(envelope) => BitrouterError::provider_error(
            ANTHROPIC_PROVIDER_NAME,
            envelope.error.message,
            ProviderErrorContext {
                status_code: Some(status_code),
                error_type: Some(envelope.error.error_type),
                code: None,
                param: None,
                request_id,
                body,
            },
        ),
        None => BitrouterError::provider_error(
            ANTHROPIC_PROVIDER_NAME,
            format!("Anthropic returned HTTP {status_code}"),
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

// ── Content block conversion ────────────────────────────────────────────────

fn content_blocks_to_language_model_content(
    blocks: Vec<AnthropicContentBlock>,
    provider_metadata: Option<ProviderMetadata>,
    response_body: JsonValue,
) -> Result<LanguageModelContent> {
    if blocks.is_empty() {
        return Err(BitrouterError::invalid_response(
            Some(ANTHROPIC_PROVIDER_NAME),
            "message response contained no content blocks",
            Some(response_body),
        ));
    }

    if blocks.len() == 1 {
        // len() == 1 guarantees next() returns Some.
        // The else branch is a defensive fallback that cannot be reached.
        let Some(block) = blocks.into_iter().next() else {
            return Err(BitrouterError::invalid_response(
                Some(ANTHROPIC_PROVIDER_NAME),
                "expected single content block but iterator was empty",
                Some(response_body),
            ));
        };
        return match block {
            AnthropicContentBlock::Text { text } => Ok(LanguageModelContent::Text {
                text,
                provider_metadata,
            }),
            AnthropicContentBlock::ToolUse { id, name, input } => {
                Ok(LanguageModelContent::ToolCall {
                    tool_call_id: id,
                    tool_name: name,
                    tool_input: serde_json::to_string(&input).map_err(|error| {
                        BitrouterError::invalid_response(
                            Some(ANTHROPIC_PROVIDER_NAME),
                            format!("failed to serialize tool call input: {error}"),
                            Some(response_body.clone()),
                        )
                    })?,
                    provider_executed: None,
                    dynamic: None,
                    provider_metadata,
                })
            }
            AnthropicContentBlock::Image { .. } | AnthropicContentBlock::ToolResult { .. } => {
                Err(BitrouterError::invalid_response(
                    Some(ANTHROPIC_PROVIDER_NAME),
                    "unexpected content block type in response",
                    Some(response_body),
                ))
            }
        };
    }

    // Multiple blocks: find the first tool_use or concatenate texts
    let mut texts = Vec::new();
    let mut tool_use = None;
    for block in blocks {
        match block {
            AnthropicContentBlock::Text { text } => texts.push(text),
            AnthropicContentBlock::ToolUse { id, name, input } if tool_use.is_none() => {
                tool_use = Some((id, name, input));
            }
            AnthropicContentBlock::ToolUse { .. }
            | AnthropicContentBlock::Image { .. }
            | AnthropicContentBlock::ToolResult { .. } => {}
        }
    }

    if let Some((id, name, input)) = tool_use {
        return Ok(LanguageModelContent::ToolCall {
            tool_call_id: id,
            tool_name: name,
            tool_input: serde_json::to_string(&input).map_err(|error| {
                BitrouterError::invalid_response(
                    Some(ANTHROPIC_PROVIDER_NAME),
                    format!("failed to serialize tool call input: {error}"),
                    Some(response_body.clone()),
                )
            })?,
            provider_executed: None,
            dynamic: None,
            provider_metadata,
        });
    }

    Ok(LanguageModelContent::Text {
        text: texts.join(""),
        provider_metadata,
    })
}

// ── Prompt conversion ───────────────────────────────────────────────────────

fn convert_prompt(
    prompt: &[LanguageModelMessage],
) -> Result<(Option<String>, Vec<AnthropicMessage>)> {
    let mut system_text: Option<String> = None;
    let mut messages = Vec::new();

    for message in prompt {
        match message {
            LanguageModelMessage::System { content, .. } => {
                system_text = Some(content.clone());
            }
            LanguageModelMessage::User { content, .. } => {
                let blocks = convert_user_content(content)?;
                messages.push(AnthropicMessage {
                    role: "user".to_owned(),
                    content: Some(blocks),
                });
            }
            LanguageModelMessage::Assistant { content, .. } => {
                let blocks = convert_assistant_content(content)?;
                messages.push(AnthropicMessage {
                    role: "assistant".to_owned(),
                    content: Some(blocks),
                });
            }
            LanguageModelMessage::Tool { content, .. } => {
                let blocks = convert_tool_results(content)?;
                messages.push(AnthropicMessage {
                    role: "user".to_owned(),
                    content: Some(AnthropicMessageContent::Blocks(blocks)),
                });
            }
        }
    }

    Ok((system_text, messages))
}

fn convert_user_content(content: &[LanguageModelUserContent]) -> Result<AnthropicMessageContent> {
    if content.len() == 1
        && let LanguageModelUserContent::Text { text, .. } = &content[0]
    {
        return Ok(AnthropicMessageContent::Text(text.clone()));
    }

    let mut blocks = Vec::new();
    for item in content {
        match item {
            LanguageModelUserContent::Text { text, .. } => {
                blocks.push(AnthropicContentBlock::Text { text: text.clone() });
            }
            LanguageModelUserContent::File {
                data, media_type, ..
            } => {
                blocks.push(convert_file_input(data, media_type)?);
            }
        }
    }

    Ok(AnthropicMessageContent::Blocks(blocks))
}

fn convert_file_input(
    data: &LanguageModelDataContent,
    media_type: &str,
) -> Result<AnthropicContentBlock> {
    if !media_type.starts_with("image/") {
        return Err(BitrouterError::unsupported(
            ANTHROPIC_PROVIDER_NAME,
            format!("user file content with media type {media_type}"),
            Some("Anthropic messages API only accepts image multimodal parts here".to_owned()),
        ));
    }

    let (base64_data, resolved_media_type) = match data {
        LanguageModelDataContent::Bytes(bytes) => {
            (BASE64_STANDARD.encode(bytes), media_type.to_owned())
        }
        LanguageModelDataContent::String(value) => {
            if value.starts_with("http://") || value.starts_with("https://") {
                return Err(BitrouterError::unsupported(
                    ANTHROPIC_PROVIDER_NAME,
                    "image URLs",
                    Some(
                        "Anthropic messages API requires base64-encoded image data, not URLs"
                            .to_owned(),
                    ),
                ));
            }
            (value.clone(), media_type.to_owned())
        }
        LanguageModelDataContent::Url(_) => {
            return Err(BitrouterError::unsupported(
                ANTHROPIC_PROVIDER_NAME,
                "image URLs",
                Some(
                    "Anthropic messages API requires base64-encoded image data, not URLs"
                        .to_owned(),
                ),
            ));
        }
    };

    Ok(AnthropicContentBlock::Image {
        source: AnthropicImageSource {
            source_type: "base64".to_owned(),
            media_type: resolved_media_type,
            data: base64_data,
        },
    })
}

fn convert_assistant_content(
    content: &[LanguageModelAssistantContent],
) -> Result<AnthropicMessageContent> {
    let mut blocks = Vec::new();

    for item in content {
        match item {
            LanguageModelAssistantContent::Text { text, .. } => {
                blocks.push(AnthropicContentBlock::Text { text: text.clone() });
            }
            LanguageModelAssistantContent::ToolCall {
                tool_call_id,
                tool_name,
                input,
                ..
            } => {
                blocks.push(AnthropicContentBlock::ToolUse {
                    id: tool_call_id.clone(),
                    name: tool_name.clone(),
                    input: input.clone(),
                });
            }
            LanguageModelAssistantContent::Reasoning { .. } => {
                return Err(BitrouterError::unsupported(
                    ANTHROPIC_PROVIDER_NAME,
                    "assistant reasoning prompt parts",
                    Some(
                        "Anthropic messages API does not expose a dedicated reasoning message part"
                            .to_owned(),
                    ),
                ));
            }
            LanguageModelAssistantContent::File { .. } => {
                return Err(BitrouterError::unsupported(
                    ANTHROPIC_PROVIDER_NAME,
                    "assistant file prompt parts",
                    None,
                ));
            }
            LanguageModelAssistantContent::ToolResult { .. } => {
                return Err(BitrouterError::unsupported(
                    ANTHROPIC_PROVIDER_NAME,
                    "assistant tool-result prompt parts",
                    Some("Use tool role messages for tool outputs".to_owned()),
                ));
            }
        }
    }

    if blocks.len() == 1
        && let AnthropicContentBlock::Text { text } = &blocks[0]
    {
        return Ok(AnthropicMessageContent::Text(text.clone()));
    }

    Ok(AnthropicMessageContent::Blocks(blocks))
}

fn convert_tool_results(content: &[LanguageModelToolResult]) -> Result<Vec<AnthropicContentBlock>> {
    let mut blocks = Vec::new();
    for item in content {
        match item {
            LanguageModelToolResult::ToolResult {
                tool_call_id,
                output,
                ..
            } => {
                let (content_str, is_error) = stringify_tool_output(output)?;
                blocks.push(AnthropicContentBlock::ToolResult {
                    tool_use_id: tool_call_id.clone(),
                    content: Some(content_str),
                    is_error,
                });
            }
            LanguageModelToolResult::ToolApprovalResponse { .. } => {
                return Err(BitrouterError::unsupported(
                    ANTHROPIC_PROVIDER_NAME,
                    "tool approval responses",
                    None,
                ));
            }
        }
    }
    Ok(blocks)
}

fn stringify_tool_output(output: &LanguageModelToolResultOutput) -> Result<(String, Option<bool>)> {
    match output {
        LanguageModelToolResultOutput::Text { value, .. } => Ok((value.clone(), None)),
        LanguageModelToolResultOutput::Json { value, .. } => serde_json::to_string(value)
            .map(|s| (s, None))
            .map_err(|error| {
                BitrouterError::invalid_request(
                    Some(ANTHROPIC_PROVIDER_NAME),
                    format!("failed to serialize tool output JSON: {error}"),
                    None,
                )
            }),
        LanguageModelToolResultOutput::ExecutionDenied { reason, .. } => {
            Ok((reason.clone(), Some(true)))
        }
        LanguageModelToolResultOutput::ErrorText { value, .. } => Ok((value.clone(), Some(true))),
        LanguageModelToolResultOutput::ErrorJson { value, .. } => serde_json::to_string(value)
            .map(|s| (s, Some(true)))
            .map_err(|error| {
                BitrouterError::invalid_request(
                    Some(ANTHROPIC_PROVIDER_NAME),
                    format!("failed to serialize error tool output JSON: {error}"),
                    None,
                )
            }),
        LanguageModelToolResultOutput::Content { value, .. } => {
            let items: Vec<JsonValue> = value.iter().map(tool_output_content_to_json).collect();
            serde_json::to_string(&JsonValue::Array(items))
                .map(|s| (s, None))
                .map_err(|error| {
                    BitrouterError::invalid_request(
                        Some(ANTHROPIC_PROVIDER_NAME),
                        format!("failed to serialize content-style tool output: {error}"),
                        None,
                    )
                })
        }
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

fn file_id_to_json(
    id: &bitrouter_core::models::language::prompt::LanguageModelToolResultOutputContentFileId,
) -> JsonValue {
    match id {
        bitrouter_core::models::language::prompt::LanguageModelToolResultOutputContentFileId::Record(record) => json!(record),
        bitrouter_core::models::language::prompt::LanguageModelToolResultOutputContentFileId::String(value) => {
            JsonValue::String(value.clone())
        }
    }
}

// ── SSE parser ──────────────────────────────────────────────────────────────

#[derive(Default)]
pub(super) struct AnthropicSseParser {
    buffer: Vec<u8>,
    state: AnthropicStreamState,
    include_raw_chunks: bool,
}

impl AnthropicSseParser {
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
                            "provider": ANTHROPIC_PROVIDER_NAME,
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
        let raw_value = match serde_json::from_str::<JsonValue>(&payload) {
            Ok(value) => value,
            Err(error) => {
                self.state.finished = true;
                return vec![LanguageModelStreamPart::Error {
                    error: json!({
                        "provider": ANTHROPIC_PROVIDER_NAME,
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

        let event: MessagesStreamEvent = match serde_json::from_value(raw_value.clone()) {
            Ok(event) => event,
            Err(error) => {
                self.state.finished = true;
                parts.push(LanguageModelStreamPart::Error {
                    error: json!({
                        "provider": ANTHROPIC_PROVIDER_NAME,
                        "kind": "response_decode",
                        "message": error.to_string(),
                        "raw": raw_value,
                    }),
                });
                return parts;
            }
        };

        parts.extend(self.state.apply_event(event));
        parts
    }
}

#[derive(Default)]
struct AnthropicStreamState {
    metadata_emitted: bool,
    text_started: bool,
    tool_inputs: HashMap<u32, AnthropicToolInputState>,
    usage: Option<LanguageModelUsage>,
    finish_reason:
        Option<bitrouter_core::models::language::finish_reason::LanguageModelFinishReason>,
    finished: bool,
}

#[derive(Default)]
struct AnthropicToolInputState {
    id: Option<String>,
    name: Option<String>,
    started: bool,
    buffered_delta: String,
}

impl AnthropicStreamState {
    fn apply_event(&mut self, event: MessagesStreamEvent) -> Vec<LanguageModelStreamPart> {
        match event {
            MessagesStreamEvent::MessageStart { message } => {
                let mut parts = Vec::new();
                if !self.metadata_emitted {
                    parts.push(LanguageModelStreamPart::ResponseMetadata {
                        id: Some(message.id),
                        timestamp: None,
                        model_id: Some(message.model),
                    });
                    self.metadata_emitted = true;
                }
                if let Some(usage) = message.usage {
                    self.merge_usage(usage);
                }
                parts
            }
            MessagesStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                let mut parts = Vec::new();
                match content_block {
                    AnthropicContentBlock::Text { .. } => {
                        if !self.text_started {
                            parts.push(LanguageModelStreamPart::TextStart {
                                id: STREAM_TEXT_ID.to_owned(),
                                provider_metadata: None,
                            });
                            self.text_started = true;
                        }
                    }
                    AnthropicContentBlock::ToolUse { id, name, .. } => {
                        let entry = self.tool_inputs.entry(index).or_default();
                        entry.id = Some(id.clone());
                        entry.name = Some(name.clone());
                        parts.push(LanguageModelStreamPart::ToolInputStart {
                            id,
                            tool_name: name,
                            provider_executed: None,
                            dynamic: None,
                            title: None,
                            provider_metadata: None,
                        });
                        entry.started = true;
                    }
                    AnthropicContentBlock::Image { .. }
                    | AnthropicContentBlock::ToolResult { .. } => {}
                }
                parts
            }
            MessagesStreamEvent::ContentBlockDelta { index, delta } => match delta {
                MessagesStreamDelta::TextDelta { text } => {
                    let mut parts = Vec::new();
                    if !self.text_started {
                        parts.push(LanguageModelStreamPart::TextStart {
                            id: STREAM_TEXT_ID.to_owned(),
                            provider_metadata: None,
                        });
                        self.text_started = true;
                    }
                    parts.push(LanguageModelStreamPart::TextDelta {
                        id: STREAM_TEXT_ID.to_owned(),
                        delta: text,
                        provider_metadata: None,
                    });
                    parts
                }
                MessagesStreamDelta::InputJsonDelta { partial_json } => {
                    let entry = self.tool_inputs.entry(index).or_default();
                    entry.buffered_delta.push_str(&partial_json);
                    let mut parts = Vec::new();
                    if entry.started && !entry.buffered_delta.is_empty() {
                        parts.push(LanguageModelStreamPart::ToolInputDelta {
                            id: entry.id.clone().unwrap_or_else(|| format!("tool-{index}")),
                            delta: std::mem::take(&mut entry.buffered_delta),
                            provider_metadata: None,
                        });
                    }
                    parts
                }
            },
            MessagesStreamEvent::ContentBlockStop { index } => {
                let mut parts = Vec::new();
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
                parts
            }
            MessagesStreamEvent::MessageDelta { delta, usage, .. } => {
                if let Some(stop_reason) = delta.stop_reason.as_deref() {
                    self.finish_reason = Some(map_finish_reason(Some(stop_reason)));
                }
                if let Some(usage) = usage {
                    self.merge_usage(usage);
                }
                Vec::new()
            }
            MessagesStreamEvent::MessageStop => self.finish_parts(),
            MessagesStreamEvent::Ping => Vec::new(),
            MessagesStreamEvent::Error { error } => {
                self.finished = true;
                vec![LanguageModelStreamPart::Error {
                    error: json!({
                        "provider": ANTHROPIC_PROVIDER_NAME,
                        "type": error.error_type,
                        "message": error.message,
                    }),
                }]
            }
        }
    }

    fn merge_usage(&mut self, usage: MessagesUsage) {
        let new_usage: LanguageModelUsage = usage_to_language_model(usage);
        match &mut self.usage {
            Some(existing) => {
                if new_usage.input_tokens.total.is_some() {
                    existing.input_tokens = new_usage.input_tokens;
                }
                if new_usage.output_tokens.total.is_some() {
                    existing.output_tokens = new_usage.output_tokens;
                }
            }
            None => {
                self.usage = Some(new_usage);
            }
        }
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

        parts.push(LanguageModelStreamPart::Finish {
            usage: self.usage.clone().unwrap_or_else(empty_usage),
            finish_reason: self
                .finish_reason
                .clone()
                .unwrap_or_else(|| map_finish_reason(Some("end_turn"))),
            provider_metadata: None,
        });
        parts
    }
}

/// A boxed byte stream used by the SSE driver, abstracting over the transport.
pub(super) type ByteStream = Pin<
    Box<
        dyn Stream<Item = std::result::Result<Bytes, Box<dyn std::error::Error + Send + Sync>>>
            + Send,
    >,
>;

/// Reads chunks from `bytes_stream`, parses SSE events, and forwards
/// [`LanguageModelStreamPart`]s into `sender`.  Respects `abort_signal`.
pub(super) async fn drive_sse_stream(
    mut bytes_stream: ByteStream,
    abort_signal: Option<CancellationToken>,
    sender: mpsc::Sender<LanguageModelStreamPart>,
    include_raw_chunks: bool,
) {
    let mut parser = AnthropicSseParser::new(include_raw_chunks);
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
                                "provider": ANTHROPIC_PROVIDER_NAME,
                                "kind": "cancelled",
                                "message": "streaming message was cancelled",
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
                            "provider": ANTHROPIC_PROVIDER_NAME,
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
        finish_reason::LanguageModelFinishReason,
        prompt::{
            LanguageModelMessage, LanguageModelToolResult, LanguageModelToolResultOutput,
            LanguageModelUserContent,
        },
        stream_part::LanguageModelStreamPart,
    };

    // ── helpers ─────────────────────────────────────────────────────────────

    fn sse_event(event_type: &str, data: &str) -> Vec<u8> {
        format!("event: {event_type}\ndata: {data}\n\n").into_bytes()
    }

    fn make_byte_stream(chunks: Vec<Vec<u8>>) -> ByteStream {
        Box::pin(tokio_stream::iter(chunks.into_iter().map(|c| {
            Ok(Bytes::from(c))
                as std::result::Result<Bytes, Box<dyn std::error::Error + Send + Sync>>
        })))
    }

    // ── error parsing tests ─────────────────────────────────────────────────

    #[test]
    fn parse_anthropic_error_with_envelope() {
        let body = serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "max_tokens must be less than 4096"
            }
        });
        let error = parse_anthropic_error(400, None, Some(body));
        match error {
            BitrouterError::Provider { message, .. } => {
                assert_eq!(message, "max_tokens must be less than 4096");
            }
            _ => panic!("expected Provider error"),
        }
    }

    #[test]
    fn parse_anthropic_error_without_envelope() {
        let error = parse_anthropic_error(500, None, None);
        match error {
            BitrouterError::Provider { message, .. } => {
                assert!(message.contains("500"));
            }
            _ => panic!("expected Provider error"),
        }
    }

    #[test]
    fn parse_anthropic_error_with_request_id() {
        let body = serde_json::json!({
            "type": "error",
            "error": {
                "type": "overloaded_error",
                "message": "Overloaded"
            }
        });
        let error = parse_anthropic_error(529, Some("req-abc123".to_owned()), Some(body));
        match error {
            BitrouterError::Provider { context, .. } => {
                assert_eq!(context.request_id.as_deref(), Some("req-abc123"));
                assert_eq!(context.status_code, Some(529));
            }
            _ => panic!("expected Provider error"),
        }
    }

    // ── prompt conversion tests ─────────────────────────────────────────────

    #[test]
    fn convert_prompt_system_message() {
        let prompt = vec![
            LanguageModelMessage::System {
                content: "You are helpful.".to_owned(),
                provider_options: None,
            },
            LanguageModelMessage::User {
                content: vec![LanguageModelUserContent::Text {
                    text: "Hello".to_owned(),
                    provider_options: None,
                }],
                provider_options: None,
            },
        ];
        let (system, messages) = convert_prompt(&prompt).unwrap();
        assert_eq!(system.as_deref(), Some("You are helpful."));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
    }

    #[test]
    fn convert_prompt_with_image() {
        let prompt = vec![LanguageModelMessage::User {
            content: vec![
                LanguageModelUserContent::Text {
                    text: "Describe this".to_owned(),
                    provider_options: None,
                },
                LanguageModelUserContent::File {
                    filename: None,
                    data: LanguageModelDataContent::Bytes(vec![1, 2, 3]),
                    media_type: "image/png".to_owned(),
                    provider_options: None,
                },
            ],
            provider_options: None,
        }];
        let (_, messages) = convert_prompt(&prompt).unwrap();
        assert_eq!(messages.len(), 1);
        match &messages[0].content {
            Some(AnthropicMessageContent::Blocks(blocks)) => {
                assert_eq!(blocks.len(), 2);
                assert!(matches!(blocks[0], AnthropicContentBlock::Text { .. }));
                assert!(matches!(blocks[1], AnthropicContentBlock::Image { .. }));
            }
            _ => panic!("expected blocks content"),
        }
    }

    #[test]
    fn convert_prompt_url_image_rejected() {
        let prompt = vec![LanguageModelMessage::User {
            content: vec![LanguageModelUserContent::File {
                filename: None,
                data: LanguageModelDataContent::Url("https://example.com/img.png".to_owned()),
                media_type: "image/png".to_owned(),
                provider_options: None,
            }],
            provider_options: None,
        }];
        let result = convert_prompt(&prompt);
        assert!(result.is_err());
    }

    #[test]
    fn convert_prompt_tool_results() {
        let prompt = vec![LanguageModelMessage::Tool {
            content: vec![LanguageModelToolResult::ToolResult {
                tool_call_id: "toolu_123".to_owned(),
                tool_name: "get_weather".to_owned(),
                output: LanguageModelToolResultOutput::Text {
                    value: "Sunny, 72F".to_owned(),
                    provider_options: None,
                },
                provider_options: None,
            }],
            provider_options: None,
        }];
        let (_, messages) = convert_prompt(&prompt).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
        match &messages[0].content {
            Some(AnthropicMessageContent::Blocks(blocks)) => {
                assert_eq!(blocks.len(), 1);
                assert!(matches!(
                    &blocks[0],
                    AnthropicContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "toolu_123"
                ));
            }
            _ => panic!("expected blocks content"),
        }
    }

    // ── SSE parser tests ────────────────────────────────────────────────────

    #[test]
    fn parse_text_stream() {
        let mut parser = AnthropicSseParser::new(false);

        let parts = parser.push_bytes(&sse_event(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_123","type":"message","role":"assistant","content":[],"model":"claude-3-5-sonnet-20241022","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#,
        ));
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::ResponseMetadata { .. }))
        );

        let parts = parser.push_bytes(&sse_event(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ));
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::TextStart { .. }))
        );

        let parts = parser.push_bytes(&sse_event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
        ));
        assert!(parts.iter().any(|p| matches!(
            p,
            LanguageModelStreamPart::TextDelta { delta, .. } if delta == "Hello"
        )));

        let parts = parser.push_bytes(&sse_event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}"#,
        ));
        assert!(parts.iter().any(|p| matches!(
            p,
            LanguageModelStreamPart::TextDelta { delta, .. } if delta == " world"
        )));

        let parts = parser.push_bytes(&sse_event(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ));
        assert!(parts.is_empty());

        let parts = parser.push_bytes(&sse_event(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
        ));
        assert!(parts.is_empty());

        let parts = parser.push_bytes(&sse_event("message_stop", r#"{"type":"message_stop"}"#));
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::TextEnd { .. }))
        );
        assert!(parts.iter().any(|p| matches!(
            p,
            LanguageModelStreamPart::Finish {
                finish_reason: LanguageModelFinishReason::Stop,
                ..
            }
        )));
        assert!(parser.is_finished());
    }

    #[test]
    fn parse_tool_use_stream() {
        let mut parser = AnthropicSseParser::new(false);

        parser.push_bytes(&sse_event(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_456","type":"message","role":"assistant","content":[],"model":"claude-3-5-sonnet-20241022","stop_reason":null,"usage":{"input_tokens":20,"output_tokens":0}}}"#,
        ));

        let parts = parser.push_bytes(&sse_event(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_789","name":"get_weather","input":{}}}"#,
        ));
        assert!(parts.iter().any(|p| matches!(
            p,
            LanguageModelStreamPart::ToolInputStart { tool_name, .. } if tool_name == "get_weather"
        )));

        let parts = parser.push_bytes(&sse_event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"location\": \"Pa"}}"#,
        ));
        assert!(parts.iter().any(|p| matches!(
            p,
            LanguageModelStreamPart::ToolInputDelta { delta, .. } if delta.contains("Pa")
        )));

        let parts = parser.push_bytes(&sse_event(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ));
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::ToolInputEnd { .. }))
        );
    }

    #[test]
    fn parse_ping_event_ignored() {
        let mut parser = AnthropicSseParser::new(false);
        let parts = parser.push_bytes(&sse_event("ping", r#"{"type":"ping"}"#));
        assert!(parts.is_empty());
    }

    #[test]
    fn parse_error_event() {
        let mut parser = AnthropicSseParser::new(false);
        let parts = parser.push_bytes(&sse_event(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        ));
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::Error { .. }))
        );
        assert!(parser.is_finished());
    }

    #[test]
    fn parser_with_raw_chunks() {
        let mut parser = AnthropicSseParser::new(true);
        let parts = parser.push_bytes(&sse_event("ping", r#"{"type":"ping"}"#));
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::Raw { .. }))
        );
    }

    #[test]
    fn parser_finish_emits_finish_part() {
        let mut parser = AnthropicSseParser::new(false);
        let parts = parser.finish();
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::Finish { .. }))
        );
    }

    #[test]
    fn incremental_byte_delivery() {
        let mut parser = AnthropicSseParser::new(false);
        let event = sse_event(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_inc","type":"message","role":"assistant","content":[],"model":"claude-3-5-sonnet-20241022","stop_reason":null,"usage":{"input_tokens":5,"output_tokens":0}}}"#,
        );

        // Deliver one byte at a time
        let mut all_parts = Vec::new();
        for byte in &event {
            all_parts.extend(parser.push_bytes(&[*byte]));
        }
        assert!(
            all_parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::ResponseMetadata { .. }))
        );
    }

    // ── drive_sse_stream integration tests ──────────────────────────────────

    #[tokio::test]
    async fn drive_text_stream() {
        let chunks = vec![
            sse_event(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_drv","type":"message","role":"assistant","content":[],"model":"claude-3-5-sonnet-20241022","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#,
            ),
            sse_event(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            sse_event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi there!"}}"#,
            ),
            sse_event(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            sse_event(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
            ),
            sse_event("message_stop", r#"{"type":"message_stop"}"#),
        ];

        let stream = make_byte_stream(chunks);
        let (tx, mut rx) = mpsc::channel(32);
        drive_sse_stream(stream, None, tx, false).await;

        let mut parts = Vec::new();
        while let Some(part) = rx.recv().await {
            parts.push(part);
        }

        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::StreamStart { .. }))
        );
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::ResponseMetadata { .. }))
        );
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::TextStart { .. }))
        );
        assert!(parts.iter().any(|p| matches!(
            p,
            LanguageModelStreamPart::TextDelta { delta, .. } if delta == "Hi there!"
        )));
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::TextEnd { .. }))
        );
        assert!(parts.iter().any(|p| matches!(
            p,
            LanguageModelStreamPart::Finish {
                finish_reason: LanguageModelFinishReason::Stop,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn drive_stream_cancellation() {
        let token = CancellationToken::new();
        let cancel_token = token.clone();

        // Create a stream that will never end
        let stream: ByteStream = Box::pin(tokio_stream::pending());

        let (tx, mut rx) = mpsc::channel(32);

        let handle = tokio::spawn(async move {
            drive_sse_stream(stream, Some(cancel_token), tx, false).await;
        });

        // Give time for stream start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        token.cancel();
        handle.await.unwrap();

        let mut parts = Vec::new();
        while let Some(part) = rx.recv().await {
            parts.push(part);
        }

        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::StreamStart { .. }))
        );
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::Error { .. }))
        );
    }

    #[tokio::test]
    async fn drive_stream_transport_error() {
        let error_stream: ByteStream = Box::pin(tokio_stream::iter(vec![
            Ok(Bytes::from(sse_event(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_err","type":"message","role":"assistant","content":[],"model":"claude-3-5-sonnet-20241022","stop_reason":null,"usage":{"input_tokens":5,"output_tokens":0}}}"#,
            ))),
            Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "connection dropped",
            )) as Box<dyn std::error::Error + Send + Sync>),
        ]));

        let (tx, mut rx) = mpsc::channel(32);
        drive_sse_stream(error_stream, None, tx, false).await;

        let mut parts = Vec::new();
        while let Some(part) = rx.recv().await {
            parts.push(part);
        }

        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::Error { .. }))
        );
    }

    #[test]
    fn crlf_event_handling() {
        let mut parser = AnthropicSseParser::new(false);
        let event = "event: ping\r\ndata: {\"type\":\"ping\"}\r\n\r\n";
        let parts = parser.push_bytes(event.as_bytes());
        assert!(parts.is_empty()); // ping is ignored
    }

    // ── content block conversion tests ──────────────────────────────────────

    #[test]
    fn single_text_block_to_content() {
        let blocks = vec![AnthropicContentBlock::Text {
            text: "Hello".to_owned(),
        }];
        let result = content_blocks_to_language_model_content(blocks, None, json!({}));
        assert!(result.is_ok());
        assert!(matches!(
            result.unwrap(),
            LanguageModelContent::Text { text, .. } if text == "Hello"
        ));
    }

    #[test]
    fn single_tool_use_block_to_content() {
        let blocks = vec![AnthropicContentBlock::ToolUse {
            id: "toolu_123".to_owned(),
            name: "get_weather".to_owned(),
            input: json!({"location": "Paris"}),
        }];
        let result = content_blocks_to_language_model_content(blocks, None, json!({}));
        assert!(result.is_ok());
        assert!(matches!(
            result.unwrap(),
            LanguageModelContent::ToolCall { tool_name, .. } if tool_name == "get_weather"
        ));
    }

    #[test]
    fn multiple_text_blocks_concatenated() {
        let blocks = vec![
            AnthropicContentBlock::Text {
                text: "Hello ".to_owned(),
            },
            AnthropicContentBlock::Text {
                text: "world!".to_owned(),
            },
        ];
        let result = content_blocks_to_language_model_content(blocks, None, json!({}));
        assert!(result.is_ok());
        assert!(matches!(
            result.unwrap(),
            LanguageModelContent::Text { text, .. } if text == "Hello world!"
        ));
    }

    #[test]
    fn text_and_tool_use_blocks_tool_wins() {
        let blocks = vec![
            AnthropicContentBlock::Text {
                text: "Let me look that up.".to_owned(),
            },
            AnthropicContentBlock::ToolUse {
                id: "toolu_456".to_owned(),
                name: "search".to_owned(),
                input: json!({"query": "weather"}),
            },
        ];
        let result = content_blocks_to_language_model_content(blocks, None, json!({}));
        assert!(result.is_ok());
        assert!(matches!(
            result.unwrap(),
            LanguageModelContent::ToolCall { tool_name, .. } if tool_name == "search"
        ));
    }

    #[test]
    fn empty_blocks_returns_error() {
        let result = content_blocks_to_language_model_content(vec![], None, json!({}));
        assert!(result.is_err());
    }
}
