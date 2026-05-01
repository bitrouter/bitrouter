use std::{collections::HashMap, pin::Pin};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use bitrouter_core::{
    api::google::generate_content::types::{
        GenerateContentCandidate, GenerateContentResponse, GenerateContentUsageMetadata,
        GoogleContent, GoogleErrorEnvelope, GoogleFunctionCall, GoogleFunctionCallingConfig,
        GoogleFunctionDeclaration, GoogleFunctionResponse, GoogleGenerationConfig,
        GoogleInlineData, GooglePart, GoogleTool, GoogleToolConfig,
    },
    errors::{BitrouterError, ProviderErrorContext, Result},
    models::{
        language::{
            call_options::LanguageModelCallOptions,
            content::LanguageModelContent,
            data_content::LanguageModelDataContent,
            finish_reason::LanguageModelFinishReason,
            generate_result::{
                LanguageModelGenerateResult, LanguageModelRawRequest, LanguageModelRawResponse,
            },
            prompt::{
                LanguageModelAssistantContent, LanguageModelMessage, LanguageModelToolResult,
                LanguageModelToolResultOutput, LanguageModelToolResultOutputContent,
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
use serde_json::json;
use tokio::{select, sync::mpsc};
use tokio_stream::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

// Re-export the request type so provider.rs can use a short path
pub(super) use bitrouter_core::api::google::generate_content::types::GenerateContentRequest;

pub(super) const GOOGLE_PROVIDER_NAME: &str = "google";
pub(super) const STREAM_TEXT_ID: &str = "text";

// ── Default max tokens ──────────────────────────────────────────────────────

const DEFAULT_MAX_TOKENS: u32 = 4096;

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

// ── Type conversions ────────────────────────────────────────────────────────

pub(super) fn usage_to_language_model(usage: GenerateContentUsageMetadata) -> LanguageModelUsage {
    let raw = serde_json::to_value(&usage).ok();
    LanguageModelUsage {
        input_tokens: LanguageModelInputTokens {
            total: usage.prompt_token_count,
            no_cache: usage
                .prompt_token_count
                .map(|total| total.saturating_sub(usage.cached_content_token_count.unwrap_or(0))),
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

pub(super) fn tool_choice_to_config(
    choice: &LanguageModelToolChoice,
) -> GoogleFunctionCallingConfig {
    match choice {
        LanguageModelToolChoice::Auto => GoogleFunctionCallingConfig {
            mode: Some("AUTO".to_owned()),
            allowed_function_names: None,
        },
        LanguageModelToolChoice::None => GoogleFunctionCallingConfig {
            mode: Some("NONE".to_owned()),
            allowed_function_names: None,
        },
        LanguageModelToolChoice::Required => GoogleFunctionCallingConfig {
            mode: Some("ANY".to_owned()),
            allowed_function_names: None,
        },
        LanguageModelToolChoice::Tool { tool_name } => GoogleFunctionCallingConfig {
            mode: Some("ANY".to_owned()),
            allowed_function_names: Some(vec![tool_name.clone()]),
        },
    }
}

pub(super) fn tool_to_declaration(tool: &LanguageModelTool) -> Result<GoogleFunctionDeclaration> {
    match tool {
        LanguageModelTool::Function {
            name,
            description,
            input_schema,
            ..
        } => {
            let parameters = serde_json::to_value(input_schema).ok();
            Ok(GoogleFunctionDeclaration {
                name: name.clone(),
                description: description.clone(),
                parameters,
            })
        }
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

// ── Response conversion ─────────────────────────────────────────────────────

pub(super) fn response_to_generate_result(
    response: GenerateContentResponse,
    request_headers: Option<reqwest::header::HeaderMap>,
    request_body: JsonValue,
    response_headers: Option<reqwest::header::HeaderMap>,
    response_body: JsonValue,
) -> Result<LanguageModelGenerateResult> {
    let provider_metadata = google_metadata(response.model_version.clone());
    let candidate = response
        .candidates
        .as_ref()
        .and_then(|c| c.first())
        .ok_or_else(|| {
            BitrouterError::invalid_response(
                Some(GOOGLE_PROVIDER_NAME),
                "response contained no candidates",
                Some(response_body.clone()),
            )
        })?;

    let finish_reason = map_finish_reason(candidate.finish_reason.as_deref());
    let content = candidate_to_language_model_content(
        candidate,
        provider_metadata.clone(),
        response_body.clone(),
    )?;

    Ok(LanguageModelGenerateResult {
        content,
        finish_reason,
        usage: response
            .usage_metadata
            .map(usage_to_language_model)
            .unwrap_or_else(empty_usage),
        provider_metadata,
        request: Some(LanguageModelRawRequest {
            headers: request_headers,
            body: request_body,
        }),
        response_metadata: Some(LanguageModelRawResponse {
            id: None,
            timestamp: None,
            model_id: response.model_version,
            headers: response_headers,
            body: Some(response_body),
        }),
        warnings: Some(Vec::<Warning>::new()),
    })
}

// ── Request building ────────────────────────────────────────────────────────

pub(super) fn build_generate_content_request(
    model_id: &str,
    options: &LanguageModelCallOptions,
) -> Result<GenerateContentRequest> {
    let _ = model_id; // model_id is in the URL, not the request body for Google

    let tools: Option<Vec<GoogleFunctionDeclaration>> = options
        .tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .map(tool_to_declaration)
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?;

    let (system_instruction, contents) = convert_prompt(&options.prompt)?;

    let has_generation_config = options.max_output_tokens.is_some()
        || options.temperature.is_some()
        || options.top_p.is_some()
        || options.top_k.is_some()
        || options.stop_sequences.is_some()
        || options.presence_penalty.is_some()
        || options.frequency_penalty.is_some()
        || options.seed.is_some()
        || options.response_format.is_some();

    let generation_config = if has_generation_config {
        Some(GoogleGenerationConfig {
            temperature: options.temperature,
            top_p: options.top_p,
            top_k: options.top_k,
            max_output_tokens: Some(options.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
            stop_sequences: options.stop_sequences.clone(),
            presence_penalty: options.presence_penalty,
            frequency_penalty: options.frequency_penalty,
            seed: options.seed.map(|s| s as i64),
            response_mime_type: options
                .response_format
                .as_ref()
                .map(|_| "application/json".to_owned()),
            response_schema: None,
        })
    } else {
        None
    };

    Ok(GenerateContentRequest {
        model: String::new(),
        contents,
        system_instruction,
        tools: tools.map(|decls| {
            vec![GoogleTool {
                function_declarations: Some(decls),
            }]
        }),
        tool_config: options.tool_choice.as_ref().map(|choice| GoogleToolConfig {
            function_calling_config: Some(tool_choice_to_config(choice)),
        }),
        generation_config,
        stream: None,
    })
}

// ── Error parsing ───────────────────────────────────────────────────────────

pub(super) fn parse_google_error(
    status_code: u16,
    request_id: Option<String>,
    body: Option<JsonValue>,
) -> BitrouterError {
    let parsed = body
        .as_ref()
        .and_then(|body| serde_json::from_value::<GoogleErrorEnvelope>(body.clone()).ok());

    match parsed {
        Some(envelope) => BitrouterError::provider_error(
            GOOGLE_PROVIDER_NAME,
            envelope
                .error
                .message
                .unwrap_or_else(|| format!("Google returned HTTP {status_code}")),
            ProviderErrorContext {
                status_code: Some(status_code),
                error_type: envelope.error.status,
                code: envelope.error.code.map(|c| c.to_string()),
                param: None,
                request_id,
                body,
            },
        ),
        None => BitrouterError::provider_error(
            GOOGLE_PROVIDER_NAME,
            format!("Google returned HTTP {status_code}"),
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

// ── Content conversion ──────────────────────────────────────────────────────

fn candidate_to_language_model_content(
    candidate: &GenerateContentCandidate,
    provider_metadata: Option<ProviderMetadata>,
    response_body: JsonValue,
) -> Result<Vec<LanguageModelContent>> {
    let parts = candidate
        .content
        .as_ref()
        .and_then(|c| c.parts.as_ref())
        .ok_or_else(|| {
            BitrouterError::invalid_response(
                Some(GOOGLE_PROVIDER_NAME),
                "candidate contained no content parts",
                Some(response_body.clone()),
            )
        })?;

    if parts.is_empty() {
        return Err(BitrouterError::invalid_response(
            Some(GOOGLE_PROVIDER_NAME),
            "candidate contained empty content parts",
            Some(response_body),
        ));
    }

    let mut out: Vec<LanguageModelContent> = Vec::new();
    let mut text_buf = String::new();

    for part in parts {
        if let Some(text) = part.text.as_deref() {
            text_buf.push_str(text);
            continue;
        }
        if let Some(fc) = &part.function_call {
            if !text_buf.is_empty() {
                out.push(LanguageModelContent::Text {
                    text: std::mem::take(&mut text_buf),
                    provider_metadata: provider_metadata.clone(),
                });
            }
            let input_str = fc
                .args
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|error| {
                    BitrouterError::invalid_response(
                        Some(GOOGLE_PROVIDER_NAME),
                        format!("failed to serialize function call args: {error}"),
                        Some(response_body.clone()),
                    )
                })?
                .unwrap_or_else(|| "{}".to_owned());

            out.push(LanguageModelContent::ToolCall {
                tool_call_id: fc.name.clone(),
                tool_name: fc.name.clone(),
                tool_input: input_str,
                provider_executed: None,
                dynamic: None,
                provider_metadata: provider_metadata.clone(),
            });
        }
    }

    if !text_buf.is_empty() {
        out.push(LanguageModelContent::Text {
            text: text_buf,
            provider_metadata: provider_metadata.clone(),
        });
    }

    if out.is_empty() {
        // Preserve previous behavior: emit empty text rather than erroring.
        out.push(LanguageModelContent::Text {
            text: String::new(),
            provider_metadata,
        });
    }

    Ok(out)
}

// ── Prompt conversion ───────────────────────────────────────────────────────

fn convert_prompt(
    prompt: &[LanguageModelMessage],
) -> Result<(Option<GoogleContent>, Vec<GoogleContent>)> {
    let mut system_instruction: Option<GoogleContent> = None;
    let mut contents = Vec::new();

    for message in prompt {
        match message {
            LanguageModelMessage::System { content, .. } => {
                system_instruction = Some(GoogleContent {
                    role: None,
                    parts: Some(vec![GooglePart {
                        text: Some(content.clone()),
                        inline_data: None,
                        function_call: None,
                        function_response: None,
                    }]),
                });
            }
            LanguageModelMessage::User { content, .. } => {
                let parts = convert_user_content(content)?;
                contents.push(GoogleContent {
                    role: Some("user".to_owned()),
                    parts: Some(parts),
                });
            }
            LanguageModelMessage::Assistant { content, .. } => {
                let parts = convert_assistant_content(content)?;
                contents.push(GoogleContent {
                    role: Some("model".to_owned()),
                    parts: Some(parts),
                });
            }
            LanguageModelMessage::Tool { content, .. } => {
                let parts = convert_tool_results(content)?;
                contents.push(GoogleContent {
                    role: Some("user".to_owned()),
                    parts: Some(parts),
                });
            }
        }
    }

    Ok((system_instruction, contents))
}

fn convert_user_content(
    content: &[bitrouter_core::models::language::prompt::LanguageModelUserContent],
) -> Result<Vec<GooglePart>> {
    use bitrouter_core::models::language::prompt::LanguageModelUserContent;
    let mut parts = Vec::new();
    for item in content {
        match item {
            LanguageModelUserContent::Text { text, .. } => {
                parts.push(GooglePart {
                    text: Some(text.clone()),
                    inline_data: None,
                    function_call: None,
                    function_response: None,
                });
            }
            LanguageModelUserContent::File {
                data, media_type, ..
            } => {
                parts.push(convert_file_input(data, media_type)?);
            }
        }
    }
    Ok(parts)
}

fn convert_file_input(data: &LanguageModelDataContent, media_type: &str) -> Result<GooglePart> {
    let (base64_data, resolved_media_type) = match data {
        LanguageModelDataContent::Bytes(bytes) => {
            (BASE64_STANDARD.encode(bytes), media_type.to_owned())
        }
        LanguageModelDataContent::String(value) => {
            if value.starts_with("http://") || value.starts_with("https://") {
                return Err(BitrouterError::unsupported(
                    GOOGLE_PROVIDER_NAME,
                    "file URLs in inline data",
                    Some(
                        "Google Generative AI API inline data requires base64-encoded data, \
                         not URLs"
                            .to_owned(),
                    ),
                ));
            }
            (value.clone(), media_type.to_owned())
        }
        LanguageModelDataContent::Url(_) => {
            return Err(BitrouterError::unsupported(
                GOOGLE_PROVIDER_NAME,
                "file URLs in inline data",
                Some(
                    "Google Generative AI API inline data requires base64-encoded data, not URLs"
                        .to_owned(),
                ),
            ));
        }
    };

    Ok(GooglePart {
        text: None,
        inline_data: Some(GoogleInlineData {
            mime_type: resolved_media_type,
            data: base64_data,
        }),
        function_call: None,
        function_response: None,
    })
}

fn convert_assistant_content(content: &[LanguageModelAssistantContent]) -> Result<Vec<GooglePart>> {
    let mut parts = Vec::new();

    for item in content {
        match item {
            LanguageModelAssistantContent::Text { text, .. } => {
                parts.push(GooglePart {
                    text: Some(text.clone()),
                    inline_data: None,
                    function_call: None,
                    function_response: None,
                });
            }
            LanguageModelAssistantContent::ToolCall {
                tool_name, input, ..
            } => {
                parts.push(GooglePart {
                    text: None,
                    inline_data: None,
                    function_call: Some(GoogleFunctionCall {
                        name: tool_name.clone(),
                        args: Some(input.clone()),
                    }),
                    function_response: None,
                });
            }
            LanguageModelAssistantContent::Reasoning { .. } => {
                return Err(BitrouterError::unsupported(
                    GOOGLE_PROVIDER_NAME,
                    "assistant reasoning prompt parts",
                    Some(
                        "Google Generative AI API does not expose a dedicated reasoning \
                         message part"
                            .to_owned(),
                    ),
                ));
            }
            LanguageModelAssistantContent::File { .. } => {
                return Err(BitrouterError::unsupported(
                    GOOGLE_PROVIDER_NAME,
                    "assistant file prompt parts",
                    None,
                ));
            }
            LanguageModelAssistantContent::ToolResult { .. } => {
                return Err(BitrouterError::unsupported(
                    GOOGLE_PROVIDER_NAME,
                    "assistant tool-result prompt parts",
                    Some("Use tool role messages for tool outputs".to_owned()),
                ));
            }
        }
    }

    Ok(parts)
}

fn convert_tool_results(content: &[LanguageModelToolResult]) -> Result<Vec<GooglePart>> {
    let mut parts = Vec::new();
    for item in content {
        match item {
            LanguageModelToolResult::ToolResult {
                tool_name, output, ..
            } => {
                let response_value = stringify_tool_output(output)?;
                parts.push(GooglePart {
                    text: None,
                    inline_data: None,
                    function_call: None,
                    function_response: Some(GoogleFunctionResponse {
                        name: tool_name.clone(),
                        response: response_value,
                    }),
                });
            }
            LanguageModelToolResult::ToolApprovalResponse { .. } => {
                return Err(BitrouterError::unsupported(
                    GOOGLE_PROVIDER_NAME,
                    "tool approval responses",
                    None,
                ));
            }
        }
    }
    Ok(parts)
}

fn stringify_tool_output(output: &LanguageModelToolResultOutput) -> Result<JsonValue> {
    match output {
        LanguageModelToolResultOutput::Text { value, .. } => Ok(json!({ "result": value })),
        LanguageModelToolResultOutput::Json { value, .. } => Ok(value.clone()),
        LanguageModelToolResultOutput::ExecutionDenied { reason, .. } => {
            Ok(json!({ "error": reason }))
        }
        LanguageModelToolResultOutput::ErrorText { value, .. } => Ok(json!({ "error": value })),
        LanguageModelToolResultOutput::ErrorJson { value, .. } => Ok(value.clone()),
        LanguageModelToolResultOutput::Content { value, .. } => {
            let items: Vec<JsonValue> = value.iter().map(tool_output_content_to_json).collect();
            Ok(json!({ "content": items }))
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
pub(super) struct GoogleSseParser {
    buffer: Vec<u8>,
    state: GoogleStreamState,
    include_raw_chunks: bool,
}

impl GoogleSseParser {
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
                            "provider": GOOGLE_PROVIDER_NAME,
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
                        "provider": GOOGLE_PROVIDER_NAME,
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

        let response: GenerateContentResponse = match serde_json::from_value(raw_value.clone()) {
            Ok(resp) => resp,
            Err(error) => {
                self.state.finished = true;
                parts.push(LanguageModelStreamPart::Error {
                    error: json!({
                        "provider": GOOGLE_PROVIDER_NAME,
                        "kind": "response_decode",
                        "message": error.to_string(),
                        "raw": raw_value,
                    }),
                });
                return parts;
            }
        };

        parts.extend(self.state.apply_response(response));
        parts
    }
}

#[derive(Default)]
struct GoogleStreamState {
    metadata_emitted: bool,
    text_started: bool,
    tool_started: HashMap<String, bool>,
    usage: Option<LanguageModelUsage>,
    finish_reason:
        Option<bitrouter_core::models::language::finish_reason::LanguageModelFinishReason>,
    finished: bool,
}

impl GoogleStreamState {
    fn apply_response(
        &mut self,
        response: GenerateContentResponse,
    ) -> Vec<LanguageModelStreamPart> {
        let mut parts = Vec::new();

        // Emit metadata on first response
        if !self.metadata_emitted
            && let Some(version) = &response.model_version
        {
            parts.push(LanguageModelStreamPart::ResponseMetadata {
                id: None,
                timestamp: None,
                model_id: Some(version.clone()),
            });
            self.metadata_emitted = true;
        }

        // Process usage
        if let Some(usage) = response.usage_metadata {
            self.merge_usage(usage);
        }

        // Process candidates
        if let Some(candidates) = &response.candidates
            && let Some(candidate) = candidates.first()
        {
            // Track finish reason
            if let Some(reason) = &candidate.finish_reason {
                self.finish_reason = Some(map_finish_reason(Some(reason)));
            }

            // Process parts
            if let Some(content) = &candidate.content
                && let Some(content_parts) = &content.parts
            {
                for part in content_parts {
                    if let Some(text) = &part.text {
                        if !self.text_started {
                            parts.push(LanguageModelStreamPart::TextStart {
                                id: STREAM_TEXT_ID.to_owned(),
                                provider_metadata: None,
                            });
                            self.text_started = true;
                        }
                        parts.push(LanguageModelStreamPart::TextDelta {
                            id: STREAM_TEXT_ID.to_owned(),
                            delta: text.clone(),
                            provider_metadata: None,
                        });
                    }
                    if let Some(fc) = &part.function_call {
                        let tool_id = fc.name.clone();
                        if !self.tool_started.contains_key(&tool_id) {
                            parts.push(LanguageModelStreamPart::ToolInputStart {
                                id: tool_id.clone(),
                                tool_name: fc.name.clone(),
                                provider_executed: None,
                                dynamic: None,
                                title: None,
                                provider_metadata: None,
                            });
                            self.tool_started.insert(tool_id.clone(), true);
                        }
                        if let Some(args) = &fc.args
                            && let Ok(args_str) = serde_json::to_string(args)
                        {
                            parts.push(LanguageModelStreamPart::ToolInputDelta {
                                id: tool_id.clone(),
                                delta: args_str,
                                provider_metadata: None,
                            });
                        }
                        parts.push(LanguageModelStreamPart::ToolInputEnd {
                            id: tool_id,
                            provider_metadata: None,
                        });
                    }
                }
            }
        }

        parts
    }

    fn merge_usage(&mut self, usage: GenerateContentUsageMetadata) {
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
                .unwrap_or_else(|| map_finish_reason(Some("STOP"))),
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
/// [`LanguageModelStreamPart`]s into `sender`. Respects `abort_signal`.
pub(super) async fn drive_sse_stream(
    mut bytes_stream: ByteStream,
    abort_signal: Option<CancellationToken>,
    sender: mpsc::Sender<LanguageModelStreamPart>,
    include_raw_chunks: bool,
) {
    let mut parser = GoogleSseParser::new(include_raw_chunks);
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
                                "provider": GOOGLE_PROVIDER_NAME,
                                "kind": "cancelled",
                                "message": "streaming generation was cancelled",
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
                            "provider": GOOGLE_PROVIDER_NAME,
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

    fn sse_event(data: &str) -> Vec<u8> {
        format!("data: {data}\n\n").into_bytes()
    }

    fn make_byte_stream(chunks: Vec<Vec<u8>>) -> ByteStream {
        Box::pin(tokio_stream::iter(chunks.into_iter().map(|c| {
            Ok(Bytes::from(c))
                as std::result::Result<Bytes, Box<dyn std::error::Error + Send + Sync>>
        })))
    }

    // ── error parsing tests ─────────────────────────────────────────────────

    #[test]
    fn parse_google_error_with_envelope() {
        let body = serde_json::json!({
            "error": {
                "code": 400,
                "message": "Invalid value at 'contents'",
                "status": "INVALID_ARGUMENT"
            }
        });
        let error = parse_google_error(400, None, Some(body));
        match error {
            BitrouterError::Provider { message, .. } => {
                assert_eq!(message, "Invalid value at 'contents'");
            }
            _ => panic!("expected Provider error"),
        }
    }

    #[test]
    fn parse_google_error_without_envelope() {
        let error = parse_google_error(500, None, None);
        match error {
            BitrouterError::Provider { message, .. } => {
                assert!(message.contains("500"));
            }
            _ => panic!("expected Provider error"),
        }
    }

    #[test]
    fn parse_google_error_with_request_id() {
        let body = serde_json::json!({
            "error": {
                "code": 429,
                "message": "Rate limit exceeded",
                "status": "RESOURCE_EXHAUSTED"
            }
        });
        let error = parse_google_error(429, Some("req-abc123".to_owned()), Some(body));
        match error {
            BitrouterError::Provider { context, .. } => {
                assert_eq!(context.request_id.as_deref(), Some("req-abc123"));
                assert_eq!(context.status_code, Some(429));
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
        let (system, contents) = convert_prompt(&prompt).unwrap();
        assert!(system.is_some());
        let sys = system.unwrap();
        assert_eq!(
            sys.parts.as_ref().unwrap()[0].text.as_deref(),
            Some("You are helpful.")
        );
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role.as_deref(), Some("user"));
    }

    #[test]
    fn convert_prompt_with_image() {
        use bitrouter_core::models::language::data_content::LanguageModelDataContent;
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
        let (_, contents) = convert_prompt(&prompt).unwrap();
        assert_eq!(contents.len(), 1);
        let parts = contents[0].parts.as_ref().unwrap();
        assert_eq!(parts.len(), 2);
        assert!(parts[0].text.is_some());
        assert!(parts[1].inline_data.is_some());
    }

    #[test]
    fn convert_prompt_url_image_rejected() {
        use bitrouter_core::models::language::data_content::LanguageModelDataContent;
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
        let (_, contents) = convert_prompt(&prompt).unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role.as_deref(), Some("user"));
        let parts = contents[0].parts.as_ref().unwrap();
        assert_eq!(parts.len(), 1);
        assert!(parts[0].function_response.is_some());
        assert_eq!(
            parts[0].function_response.as_ref().unwrap().name,
            "get_weather"
        );
    }

    // ── SSE parser tests ────────────────────────────────────────────────────

    #[test]
    fn parse_text_stream() {
        let mut parser = GoogleSseParser::new(false);

        let parts = parser.push_bytes(&sse_event(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Hello"}]},"index":0}],"modelVersion":"gemini-2.0-flash","usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":1,"totalTokenCount":11}}"#,
        ));
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
            LanguageModelStreamPart::TextDelta { delta, .. } if delta == "Hello"
        )));

        let parts = parser.push_bytes(&sse_event(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":" world"}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#,
        ));
        assert!(parts.iter().any(|p| matches!(
            p,
            LanguageModelStreamPart::TextDelta { delta, .. } if delta == " world"
        )));

        let parts = parser.finish();
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
    fn parse_function_call_stream() {
        let mut parser = GoogleSseParser::new(false);

        let parts = parser.push_bytes(&sse_event(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"get_weather","args":{"location":"Paris"}}}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":10,"totalTokenCount":30}}"#,
        ));
        assert!(parts.iter().any(|p| matches!(
            p,
            LanguageModelStreamPart::ToolInputStart { tool_name, .. } if tool_name == "get_weather"
        )));
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::ToolInputDelta { .. }))
        );
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::ToolInputEnd { .. }))
        );
    }

    #[test]
    fn parser_with_raw_chunks() {
        let mut parser = GoogleSseParser::new(true);
        let parts = parser.push_bytes(&sse_event(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Hi"}]},"index":0}]}"#,
        ));
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::Raw { .. }))
        );
    }

    #[test]
    fn parser_finish_emits_finish_part() {
        let mut parser = GoogleSseParser::new(false);
        let parts = parser.finish();
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::Finish { .. }))
        );
    }

    #[test]
    fn incremental_byte_delivery() {
        let mut parser = GoogleSseParser::new(false);
        let event = sse_event(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Hi"}]},"index":0}],"modelVersion":"gemini-2.0-flash"}"#,
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
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Hi there!"}]},"index":0}],"modelVersion":"gemini-2.0-flash","usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":0,"totalTokenCount":10}}"#,
            ),
            sse_event(
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":" How can I help?"}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":8,"totalTokenCount":18}}"#,
            ),
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

        let stream: ByteStream = Box::pin(tokio_stream::pending());

        let (tx, mut rx) = mpsc::channel(32);

        let handle = tokio::spawn(async move {
            drive_sse_stream(stream, Some(cancel_token), tx, false).await;
        });

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
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Hi"}]},"index":0}],"modelVersion":"gemini-2.0-flash"}"#,
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
        let mut parser = GoogleSseParser::new(false);
        let event = "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hi\"}]},\"index\":0}]}\r\n\r\n";
        let parts = parser.push_bytes(event.as_bytes());
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, LanguageModelStreamPart::TextDelta { .. }))
        );
    }

    // ── helper / conversion tests (migrated from types.rs) ──────────────────

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
        let usage = GenerateContentUsageMetadata {
            prompt_token_count: Some(100),
            candidates_token_count: Some(50),
            total_token_count: Some(150),
            cached_content_token_count: Some(20),
        };
        let lm_usage = usage_to_language_model(usage);
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
        let usage = GenerateContentUsageMetadata {
            prompt_token_count: Some(100),
            candidates_token_count: Some(50),
            total_token_count: Some(150),
            cached_content_token_count: None,
        };
        let lm_usage = usage_to_language_model(usage);
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
        let response: GenerateContentResponse = serde_json::from_str(json).unwrap();
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
        let response: GenerateContentResponse = serde_json::from_str(json).unwrap();
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
        let request = GenerateContentRequest {
            model: String::new(),
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
            stream: None,
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
        let config = tool_choice_to_config(&LanguageModelToolChoice::Auto);
        assert_eq!(config.mode.as_deref(), Some("AUTO"));
        assert!(config.allowed_function_names.is_none());
    }

    #[test]
    fn tool_choice_none() {
        let config = tool_choice_to_config(&LanguageModelToolChoice::None);
        assert_eq!(config.mode.as_deref(), Some("NONE"));
    }

    #[test]
    fn tool_choice_required_maps_to_any() {
        let config = tool_choice_to_config(&LanguageModelToolChoice::Required);
        assert_eq!(config.mode.as_deref(), Some("ANY"));
        assert!(config.allowed_function_names.is_none());
    }

    #[test]
    fn tool_choice_named() {
        let config = tool_choice_to_config(&LanguageModelToolChoice::Tool {
            tool_name: "get_weather".to_owned(),
        });
        assert_eq!(config.mode.as_deref(), Some("ANY"));
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
        let result = tool_to_declaration(&tool);
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
        let result = tool_to_declaration(&tool);
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
        let request = GenerateContentRequest {
            model: String::new(),
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
                    parameters: Some(serde_json::json!({})),
                }]),
            }]),
            tool_config: Some(GoogleToolConfig {
                function_calling_config: Some(GoogleFunctionCallingConfig {
                    mode: Some("AUTO".to_owned()),
                    allowed_function_names: None,
                }),
            }),
            generation_config: None,
            stream: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        let parsed: GenerateContentRequest = serde_json::from_str(&json).unwrap();
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
                .mode
                .as_deref(),
            Some("AUTO")
        );
    }
}
