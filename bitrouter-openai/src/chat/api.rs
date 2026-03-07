use std::collections::HashMap;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use bitrouter_core::{
    errors::{BitrouterError, Result},
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
                LanguageModelToolResultOutputContentFileId, LanguageModelUserContent,
            },
            stream_part::LanguageModelStreamPart,
            usage::LanguageModelUsage,
        },
        shared::{provider::ProviderMetadata, types::JsonValue, warnings::Warning},
    },
};
use reqwest::header::HeaderMap;
use serde_json::json;
use tokio::sync::mpsc;

use super::types::{
    OPENAI_PROVIDER_NAME, OpenAiChatCompletionChunk, OpenAiChatCompletionResponse,
    OpenAiChatCompletionStreamOptions, OpenAiChatCompletionsRequest, OpenAiChatMessageParam,
    OpenAiChatTool, OpenAiChatToolCall, OpenAiChatToolCallFunction, OpenAiChunkDeltaToolCall,
    OpenAiErrorEnvelope, OpenAiImageUrl, OpenAiInputContentPart, OpenAiToolChoice,
    OpenAiUserMessageContent, STREAM_TEXT_ID, empty_usage, json_value_to_string, map_finish_reason,
    openai_metadata,
};

// ── Response conversion ─────────────────────────────────────────────────────

impl OpenAiChatCompletionResponse {
    pub(super) fn into_generate_result(
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
                .map(LanguageModelUsage::from)
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

// ── Request building ────────────────────────────────────────────────────────

impl OpenAiChatCompletionsRequest {
    pub(super) fn from_call_options(
        model: String,
        options: &LanguageModelCallOptions,
        stream: bool,
    ) -> Result<Self> {
        if options.top_k.is_some() {
            return Err(BitrouterError::unsupported(
                OPENAI_PROVIDER_NAME,
                "top_k",
                Some("OpenAI chat completions does not expose top_k sampling".to_owned()),
            ));
        }

        let tools: Option<Vec<OpenAiChatTool>> = options
            .tools
            .as_ref()
            .map(|tools| {
                tools
                    .iter()
                    .map(OpenAiChatTool::try_from)
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?;
        let has_tools = tools.as_ref().is_some_and(|tools| !tools.is_empty());

        Ok(Self {
            model,
            messages: convert_prompt(&options.prompt)?,
            stream: Some(stream),
            stream_options: stream.then_some(OpenAiChatCompletionStreamOptions {
                include_usage: Some(true),
            }),
            max_completion_tokens: options.max_output_tokens,
            temperature: options.temperature,
            top_p: options.top_p,
            stop: options.stop_sequences.clone(),
            presence_penalty: options.presence_penalty,
            frequency_penalty: options.frequency_penalty,
            response_format: options.response_format.as_ref().map(Into::into),
            seed: options.seed,
            tools,
            tool_choice: options.tool_choice.as_ref().map(OpenAiToolChoice::from),
            parallel_tool_calls: has_tools.then_some(false),
        })
    }
}

// ── Error parsing ───────────────────────────────────────────────────────────

pub(super) fn parse_openai_error(
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

// ── Message / prompt conversion ─────────────────────────────────────────────

fn message_to_language_model_content(
    message: super::types::OpenAiChatCompletionMessage,
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

fn convert_prompt(prompt: &[LanguageModelMessage]) -> Result<Vec<OpenAiChatMessageParam>> {
    let mut messages = Vec::new();

    for message in prompt {
        match message {
            LanguageModelMessage::System { content, .. } => {
                messages.push(OpenAiChatMessageParam::System {
                    content: content.clone(),
                });
            }
            LanguageModelMessage::User { content, .. } => {
                messages.push(OpenAiChatMessageParam::User {
                    content: convert_user_content(content)?,
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
                            tool_calls.push(OpenAiChatToolCall {
                                id: tool_call_id.clone(),
                                kind: "function".to_owned(),
                                function: OpenAiChatToolCallFunction {
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

                messages.push(OpenAiChatMessageParam::Assistant {
                    content: (!text_segments.is_empty()).then(|| text_segments.join("\n")),
                    tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
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
                            messages.push(OpenAiChatMessageParam::Tool {
                                tool_call_id: tool_call_id.clone(),
                                content: stringify_tool_output(output)?,
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

fn convert_user_content(content: &[LanguageModelUserContent]) -> Result<OpenAiUserMessageContent> {
    if content.len() == 1 {
        if let LanguageModelUserContent::Text { text, .. } = &content[0] {
            return Ok(OpenAiUserMessageContent::Text(text.clone()));
        }
    }

    let mut parts = Vec::new();
    for item in content {
        match item {
            LanguageModelUserContent::Text { text, .. } => {
                parts.push(OpenAiInputContentPart::Text { text: text.clone() });
            }
            LanguageModelUserContent::File {
                data, media_type, ..
            } => {
                parts.push(OpenAiInputContentPart::ImageUrl {
                    image_url: OpenAiImageUrl {
                        url: convert_image_input(data, media_type)?,
                    },
                });
            }
        }
    }

    Ok(OpenAiUserMessageContent::Parts(parts))
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
            if let Ok(event) = String::from_utf8(self.buffer.clone()) {
                if let Some(payload) = extract_sse_data(&event) {
                    let mut parts = self.parse_payload(payload);
                    parts.extend(self.state.finish_parts());
                    self.buffer.clear();
                    return parts;
                }
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

        if let Ok(error_envelope) = serde_json::from_value::<OpenAiErrorEnvelope>(raw_value.clone())
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

        let chunk: OpenAiChatCompletionChunk = match serde_json::from_value(raw_value.clone()) {
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
    fn apply_chunk(&mut self, chunk: OpenAiChatCompletionChunk) -> Vec<LanguageModelStreamPart> {
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
            self.usage = Some(usage.into());
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
        tool_call: OpenAiChunkDeltaToolCall,
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
            if let Some(tool_state) = self.tool_inputs.get(&index) {
                if tool_state.started {
                    parts.push(LanguageModelStreamPart::ToolInputEnd {
                        id: tool_state
                            .id
                            .clone()
                            .unwrap_or_else(|| format!("tool-{index}")),
                        provider_metadata: None,
                    });
                }
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

pub(super) async fn send_stream_part(
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

    #[test]
    fn builds_image_prompt_request() {
        let request = OpenAiChatCompletionsRequest::from_call_options(
            "gpt-4o-mini".to_owned(),
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

        assert!(matches!(
            request.messages[0],
            OpenAiChatMessageParam::User { .. }
        ));
    }
}
