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
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{select, sync::mpsc};
use tokio_stream::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

pub(crate) const OPENAI_PROVIDER_NAME: &str = "openai";
const STREAM_TEXT_ID: &str = "text";

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiErrorEnvelope {
    pub error: OpenAiApiError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiResponseUsage {
    #[serde(default)]
    pub input_tokens: Option<u32>,
    #[serde(default)]
    pub output_tokens: Option<u32>,
    #[serde(default)]
    pub total_tokens: Option<u32>,
    #[serde(default)]
    pub input_tokens_details: Option<OpenAiResponseInputTokensDetails>,
    #[serde(default)]
    pub output_tokens_details: Option<OpenAiResponseOutputTokensDetails>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiResponseInputTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiResponseOutputTokensDetails {
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
}

impl From<OpenAiResponseUsage> for LanguageModelUsage {
    fn from(usage: OpenAiResponseUsage) -> Self {
        let raw = serde_json::to_value(&usage).ok();
        let reasoning_tokens = usage
            .output_tokens_details
            .as_ref()
            .and_then(|details| details.reasoning_tokens);

        LanguageModelUsage {
            input_tokens: LanguageModelInputTokens {
                total: usage.input_tokens,
                no_cache: usage
                    .input_tokens_details
                    .as_ref()
                    .and_then(|details| details.cached_tokens)
                    .map(|cached| usage.input_tokens.unwrap_or(cached).saturating_sub(cached)),
                cache_read: usage
                    .input_tokens_details
                    .as_ref()
                    .and_then(|details| details.cached_tokens),
                cache_write: None,
            },
            output_tokens: LanguageModelOutputTokens {
                total: usage.output_tokens,
                text: usage.output_tokens,
                reasoning: reasoning_tokens,
            },
            raw,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiResponsesRequest {
    pub model: String,
    pub input: Vec<OpenAiResponseInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<OpenAiResponsesTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<OpenAiToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<OpenAiResponseTextConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OpenAiResponseInputItem {
    Message {
        role: String,
        content: Vec<OpenAiResponseInputContent>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OpenAiResponseInputContent {
    InputText { text: String },
    InputImage { image_url: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiResponseTextConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<OpenAiResponseTextFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OpenAiResponseTextFormat {
    Text,
    JsonObject,
    JsonSchema {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        schema: schemars::Schema,
        #[serde(skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiResponsesTool {
    #[serde(rename = "type")]
    pub kind: String,
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
        name: String,
    },
}

impl From<&LanguageModelToolChoice> for OpenAiToolChoice {
    fn from(choice: &LanguageModelToolChoice) -> Self {
        match choice {
            LanguageModelToolChoice::Auto => OpenAiToolChoice::Mode("auto".to_owned()),
            LanguageModelToolChoice::None => OpenAiToolChoice::Mode("none".to_owned()),
            LanguageModelToolChoice::Required => OpenAiToolChoice::Mode("required".to_owned()),
            LanguageModelToolChoice::Tool { tool_name } => OpenAiToolChoice::Named {
                kind: "function".to_owned(),
                name: tool_name.clone(),
            },
        }
    }
}

impl From<&LanguageModelResponseFormat> for OpenAiResponseTextConfig {
    fn from(format: &LanguageModelResponseFormat) -> Self {
        match format {
            LanguageModelResponseFormat::Text => Self {
                format: Some(OpenAiResponseTextFormat::Text),
            },
            LanguageModelResponseFormat::Json {
                schema,
                name,
                description,
            } => Self {
                format: Some(match schema {
                    Some(schema) => OpenAiResponseTextFormat::JsonSchema {
                        name: name.clone().unwrap_or_else(|| "output".to_owned()),
                        description: description.clone(),
                        schema: schema.clone(),
                        strict: Some(true),
                    },
                    None => OpenAiResponseTextFormat::JsonObject,
                }),
            },
        }
    }
}

impl TryFrom<&LanguageModelTool> for OpenAiResponsesTool {
    type Error = BitrouterError;

    fn try_from(tool: &LanguageModelTool) -> Result<Self> {
        match tool {
            LanguageModelTool::Function {
                name,
                description,
                input_schema,
                strict,
                ..
            } => Ok(Self {
                kind: "function".to_owned(),
                name: name.clone(),
                description: description.clone(),
                parameters: input_schema.clone(),
                strict: *strict,
            }),
            LanguageModelTool::Provider { id, .. } => Err(BitrouterError::unsupported(
                OPENAI_PROVIDER_NAME,
                format!("provider tool {}:{}", id.provider_name, id.tool_id),
                Some(
                    "OpenAI responses supports function and custom tools, but bitrouter-core provider tools do not map directly".to_owned(),
                ),
            )),
        }
    }
}

impl TryFrom<&LanguageModelUserContent> for OpenAiResponseInputContent {
    type Error = BitrouterError;

    fn try_from(content: &LanguageModelUserContent) -> Result<Self> {
        match content {
            LanguageModelUserContent::Text { text, .. } => {
                Ok(Self::InputText { text: text.clone() })
            }
            LanguageModelUserContent::File {
                data, media_type, ..
            } => Ok(Self::InputImage {
                image_url: convert_image_input(data, media_type)?,
            }),
        }
    }
}

impl OpenAiResponsesRequest {
    pub(crate) fn from_call_options(
        model_id: &str,
        options: &LanguageModelCallOptions,
        stream: bool,
    ) -> Result<Self> {
        let model = model_id.to_owned();
        if options.top_k.is_some() {
            return Err(BitrouterError::unsupported(
                OPENAI_PROVIDER_NAME,
                "top_k",
                Some("OpenAI responses does not expose top_k sampling".to_owned()),
            ));
        }
        if options.stop_sequences.is_some() {
            return Err(BitrouterError::unsupported(
                OPENAI_PROVIDER_NAME,
                "stop_sequences",
                Some("OpenAI responses does not support stop for all models".to_owned()),
            ));
        }
        if options.presence_penalty.is_some() || options.frequency_penalty.is_some() {
            return Err(BitrouterError::unsupported(
                OPENAI_PROVIDER_NAME,
                "presence_penalty/frequency_penalty",
                Some("OpenAI responses does not accept chat-style penalties".to_owned()),
            ));
        }

        let tools = options
            .tools
            .as_ref()
            .map(|tools| {
                tools
                    .iter()
                    .map(OpenAiResponsesTool::try_from)
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?;
        let has_tools = tools.as_ref().is_some_and(|tools| !tools.is_empty());

        Ok(Self {
            model,
            input: convert_prompt(&options.prompt)?,
            stream: Some(stream),
            max_output_tokens: options.max_output_tokens,
            temperature: options.temperature,
            top_p: options.top_p,
            tools,
            tool_choice: options.tool_choice.as_ref().map(OpenAiToolChoice::from),
            parallel_tool_calls: has_tools.then_some(false),
            text: options
                .response_format
                .as_ref()
                .map(OpenAiResponseTextConfig::from),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiResponse {
    pub id: String,
    pub created_at: i64,
    pub model: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub output: Vec<OpenAiResponseOutputItem>,
    #[serde(default)]
    pub usage: Option<OpenAiResponseUsage>,
    #[serde(default)]
    pub incomplete_details: Option<OpenAiResponseIncompleteDetails>,
    #[serde(default)]
    pub error: Option<OpenAiApiError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiResponseIncompleteDetails {
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OpenAiResponseOutputItem {
    Message {
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        content: Vec<OpenAiResponseOutputContent>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OpenAiResponseOutputContent {
    OutputText {
        text: String,
    },
    Refusal {
        refusal: String,
    },
    #[serde(other)]
    Unknown,
}

impl OpenAiResponse {
    pub(crate) fn into_generate_result(
        self,
        request_headers: Option<HeaderMap>,
        request_body: JsonValue,
        response_headers: Option<HeaderMap>,
        response_body: JsonValue,
    ) -> Result<LanguageModelGenerateResult> {
        let mut text_segments = Vec::new();
        let mut refusal: Option<String> = None;
        let mut first_function_call: Option<(String, String, String)> = None;

        for item in &self.output {
            match item {
                OpenAiResponseOutputItem::Message { content, .. } => {
                    for part in content {
                        match part {
                            OpenAiResponseOutputContent::OutputText { text } => {
                                text_segments.push(text.clone());
                            }
                            OpenAiResponseOutputContent::Refusal { refusal: value } => {
                                refusal = Some(value.clone());
                            }
                            OpenAiResponseOutputContent::Unknown => {}
                        }
                    }
                }
                OpenAiResponseOutputItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                } => {
                    if first_function_call.is_none() {
                        first_function_call =
                            Some((call_id.clone(), name.clone(), arguments.clone()));
                    }
                }
                OpenAiResponseOutputItem::Unknown => {}
            }
        }

        let provider_metadata = openai_metadata(refusal);
        let finish_reason = map_response_finish_reason(
            self.status.as_deref(),
            self.incomplete_details
                .as_ref()
                .and_then(|details| details.reason.as_deref()),
            first_function_call.is_some(),
        );

        let content = if let Some((call_id, tool_name, tool_input)) = first_function_call {
            LanguageModelContent::ToolCall {
                tool_call_id: call_id,
                tool_name,
                tool_input,
                provider_executed: None,
                dynamic: None,
                provider_metadata: provider_metadata.clone(),
            }
        } else if !text_segments.is_empty() {
            LanguageModelContent::Text {
                text: text_segments.join("\n"),
                provider_metadata: provider_metadata.clone(),
            }
        } else {
            return Err(BitrouterError::invalid_response(
                Some(OPENAI_PROVIDER_NAME),
                "responses result did not contain text or function_call output",
                Some(response_body),
            ));
        };

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
                timestamp: Some(self.created_at.saturating_mul(1_000)),
                model_id: Some(self.model),
                headers: response_headers,
                body: Some(response_body),
            }),
            warnings: Some(Vec::<Warning>::new()),
        })
    }
}

pub(crate) fn parse_openai_error(
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

fn openai_metadata(refusal: Option<String>) -> Option<ProviderMetadata> {
    refusal.map(|refusal| {
        HashMap::from([(
            OPENAI_PROVIDER_NAME.to_owned(),
            json!({ "refusal": refusal }),
        )])
    })
}

fn map_response_finish_reason(
    status: Option<&str>,
    incomplete_reason: Option<&str>,
    has_function_call: bool,
) -> LanguageModelFinishReason {
    if has_function_call {
        return LanguageModelFinishReason::FunctionCall;
    }

    match (status, incomplete_reason) {
        (_, Some("max_output_tokens")) => LanguageModelFinishReason::Length,
        (_, Some("content_filter")) => LanguageModelFinishReason::ContentFilter,
        (Some("failed"), _) => LanguageModelFinishReason::Error,
        _ => LanguageModelFinishReason::Stop,
    }
}

fn convert_prompt(prompt: &[LanguageModelMessage]) -> Result<Vec<OpenAiResponseInputItem>> {
    let mut input = Vec::new();

    for message in prompt {
        match message {
            LanguageModelMessage::System { content, .. } => {
                input.push(OpenAiResponseInputItem::Message {
                    role: "system".to_owned(),
                    content: vec![OpenAiResponseInputContent::InputText {
                        text: content.clone(),
                    }],
                });
            }
            LanguageModelMessage::User { content, .. } => {
                input.push(OpenAiResponseInputItem::Message {
                    role: "user".to_owned(),
                    content: content
                        .iter()
                        .map(OpenAiResponseInputContent::try_from)
                        .collect::<Result<Vec<_>>>()?,
                });
            }
            LanguageModelMessage::Assistant { content, .. } => {
                let mut text_content = Vec::new();

                for item in content {
                    match item {
                        LanguageModelAssistantContent::Text { text, .. } => {
                            text_content
                                .push(OpenAiResponseInputContent::InputText { text: text.clone() });
                        }
                        LanguageModelAssistantContent::ToolCall {
                            tool_call_id,
                            tool_name,
                            input: tool_input,
                            ..
                        } => {
                            input.push(OpenAiResponseInputItem::FunctionCall {
                                call_id: tool_call_id.clone(),
                                name: tool_name.clone(),
                                arguments: serde_json::to_string(tool_input).map_err(|error| {
                                    BitrouterError::invalid_request(
                                        Some(OPENAI_PROVIDER_NAME),
                                        format!(
                                            "failed to serialize assistant tool call input: {error}"
                                        ),
                                        None,
                                    )
                                })?,
                            });
                        }
                        LanguageModelAssistantContent::Reasoning { .. } => {
                            return Err(BitrouterError::unsupported(
                                OPENAI_PROVIDER_NAME,
                                "assistant reasoning prompt parts",
                                Some(
                                    "Responses API does not expose a dedicated assistant reasoning input type"
                                        .to_owned(),
                                ),
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

                if !text_content.is_empty() {
                    input.push(OpenAiResponseInputItem::Message {
                        role: "assistant".to_owned(),
                        content: text_content,
                    });
                }
            }
            LanguageModelMessage::Tool { content, .. } => {
                for item in content {
                    match item {
                        LanguageModelToolResult::ToolResult {
                            tool_call_id,
                            output,
                            ..
                        } => {
                            input.push(OpenAiResponseInputItem::FunctionCallOutput {
                                call_id: tool_call_id.clone(),
                                output: stringify_tool_output(output)?,
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

    Ok(input)
}

fn convert_image_input(data: &LanguageModelDataContent, media_type: &str) -> Result<String> {
    if !media_type.starts_with("image/") {
        return Err(BitrouterError::unsupported(
            OPENAI_PROVIDER_NAME,
            format!("user file content with media type {media_type}"),
            Some("OpenAI responses input_image requires image media types".to_owned()),
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

#[derive(Default)]
pub(crate) struct OpenAiResponsesSseParser {
    buffer: Vec<u8>,
    state: OpenAiResponsesStreamState,
    include_raw_chunks: bool,
}

impl OpenAiResponsesSseParser {
    pub(crate) fn new(include_raw_chunks: bool) -> Self {
        Self {
            include_raw_chunks,
            ..Self::default()
        }
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.state.finished
    }

    pub(crate) fn push_bytes(&mut self, bytes: &[u8]) -> Vec<LanguageModelStreamPart> {
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

    pub(crate) fn finish(&mut self) -> Vec<LanguageModelStreamPart> {
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

        let event: OpenAiResponsesStreamEvent = match serde_json::from_value(raw_value.clone()) {
            Ok(event) => event,
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

        parts.extend(self.state.apply_event(event));
        parts
    }
}

#[derive(Default)]
struct OpenAiToolInputState {
    tool_name: Option<String>,
    started: bool,
}

#[derive(Default)]
struct OpenAiResponsesStreamState {
    metadata_emitted: bool,
    text_started: bool,
    text_id: Option<String>,
    tool_inputs: HashMap<String, OpenAiToolInputState>,
    usage: Option<LanguageModelUsage>,
    finish_reason: Option<LanguageModelFinishReason>,
    finished: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum OpenAiResponsesStreamEvent {
    #[serde(rename = "response.created")]
    ResponseCreated { response: OpenAiResponse },
    #[serde(rename = "response.output_item.added")]
    ResponseOutputItemAdded { item: OpenAiResponseOutputItem },
    #[serde(rename = "response.output_text.delta")]
    ResponseOutputTextDelta {
        #[serde(default)]
        item_id: Option<String>,
        delta: String,
    },
    #[serde(rename = "response.output_text.done")]
    ResponseOutputTextDone {
        #[serde(default)]
        item_id: Option<String>,
        #[serde(default)]
        text: Option<String>,
    },
    #[serde(rename = "response.function_call_arguments.delta")]
    ResponseFunctionCallArgumentsDelta {
        #[serde(default)]
        item_id: Option<String>,
        delta: String,
    },
    #[serde(rename = "response.function_call_arguments.done")]
    ResponseFunctionCallArgumentsDone {
        #[serde(default)]
        item_id: Option<String>,
        #[serde(default)]
        arguments: Option<String>,
    },
    #[serde(rename = "response.completed")]
    ResponseCompleted { response: OpenAiResponse },
    #[serde(rename = "response.failed")]
    ResponseFailed { response: OpenAiResponse },
    #[serde(rename = "error")]
    Error { error: OpenAiApiError },
}

impl OpenAiResponsesStreamState {
    fn apply_event(&mut self, event: OpenAiResponsesStreamEvent) -> Vec<LanguageModelStreamPart> {
        match event {
            OpenAiResponsesStreamEvent::ResponseCreated { response } => {
                let mut parts = Vec::new();
                if !self.metadata_emitted {
                    parts.push(LanguageModelStreamPart::ResponseMetadata {
                        id: Some(response.id),
                        timestamp: Some(response.created_at.saturating_mul(1_000)),
                        model_id: Some(response.model),
                    });
                    self.metadata_emitted = true;
                }
                if let Some(usage) = response.usage {
                    self.usage = Some(usage.into());
                }
                parts
            }
            OpenAiResponsesStreamEvent::ResponseOutputItemAdded { item } => match item {
                OpenAiResponseOutputItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                } => {
                    let tool_state = self.tool_inputs.entry(call_id.clone()).or_default();
                    tool_state.tool_name = Some(name.clone());
                    let mut parts = Vec::new();
                    if !tool_state.started {
                        parts.push(LanguageModelStreamPart::ToolInputStart {
                            id: call_id.clone(),
                            tool_name: name,
                            provider_executed: None,
                            dynamic: None,
                            title: None,
                            provider_metadata: None,
                        });
                        tool_state.started = true;
                    }
                    if !arguments.is_empty() {
                        parts.push(LanguageModelStreamPart::ToolInputDelta {
                            id: call_id,
                            delta: arguments,
                            provider_metadata: None,
                        });
                    }
                    parts
                }
                _ => Vec::new(),
            },
            OpenAiResponsesStreamEvent::ResponseOutputTextDelta { item_id, delta } => {
                let text_id = item_id.unwrap_or_else(|| STREAM_TEXT_ID.to_owned());
                let mut parts = Vec::new();
                if !self.text_started {
                    parts.push(LanguageModelStreamPart::TextStart {
                        id: text_id.clone(),
                        provider_metadata: None,
                    });
                    self.text_started = true;
                    self.text_id = Some(text_id.clone());
                }
                parts.push(LanguageModelStreamPart::TextDelta {
                    id: text_id,
                    delta,
                    provider_metadata: None,
                });
                parts
            }
            OpenAiResponsesStreamEvent::ResponseOutputTextDone { item_id, text } => {
                let text_id = item_id
                    .or_else(|| self.text_id.clone())
                    .unwrap_or_else(|| STREAM_TEXT_ID.to_owned());
                let mut parts = Vec::new();
                if let Some(text) = text
                    && !text.is_empty()
                    && !self.text_started
                {
                    parts.push(LanguageModelStreamPart::TextStart {
                        id: text_id.clone(),
                        provider_metadata: None,
                    });
                    parts.push(LanguageModelStreamPart::TextDelta {
                        id: text_id.clone(),
                        delta: text,
                        provider_metadata: None,
                    });
                    self.text_started = true;
                    self.text_id = Some(text_id.clone());
                }
                if self.text_started {
                    parts.push(LanguageModelStreamPart::TextEnd {
                        id: text_id,
                        provider_metadata: None,
                    });
                }
                parts
            }
            OpenAiResponsesStreamEvent::ResponseFunctionCallArgumentsDelta { item_id, delta } => {
                let id = item_id.unwrap_or_else(|| "tool".to_owned());
                let tool_state = self.tool_inputs.entry(id.clone()).or_default();
                let mut parts = Vec::new();
                if !tool_state.started {
                    parts.push(LanguageModelStreamPart::ToolInputStart {
                        id: id.clone(),
                        tool_name: tool_state
                            .tool_name
                            .clone()
                            .unwrap_or_else(|| "tool".to_owned()),
                        provider_executed: None,
                        dynamic: None,
                        title: None,
                        provider_metadata: None,
                    });
                    tool_state.started = true;
                }
                if !delta.is_empty() {
                    parts.push(LanguageModelStreamPart::ToolInputDelta {
                        id,
                        delta,
                        provider_metadata: None,
                    });
                }
                parts
            }
            OpenAiResponsesStreamEvent::ResponseFunctionCallArgumentsDone {
                item_id,
                arguments,
            } => {
                let id = item_id.unwrap_or_else(|| "tool".to_owned());
                let mut parts = Vec::new();
                if let Some(arguments) = arguments
                    && !arguments.is_empty()
                {
                    parts.push(LanguageModelStreamPart::ToolInputDelta {
                        id: id.clone(),
                        delta: arguments,
                        provider_metadata: None,
                    });
                }
                parts.push(LanguageModelStreamPart::ToolInputEnd {
                    id,
                    provider_metadata: None,
                });
                parts
            }
            OpenAiResponsesStreamEvent::ResponseCompleted { response } => {
                if let Some(usage) = response.usage {
                    self.usage = Some(usage.into());
                }
                self.finish_reason = Some(map_response_finish_reason(
                    response.status.as_deref(),
                    response
                        .incomplete_details
                        .as_ref()
                        .and_then(|details| details.reason.as_deref()),
                    response
                        .output
                        .iter()
                        .any(|item| matches!(item, OpenAiResponseOutputItem::FunctionCall { .. })),
                ));
                self.finish_parts()
            }
            OpenAiResponsesStreamEvent::ResponseFailed { response } => {
                self.finish_reason = Some(LanguageModelFinishReason::Error);
                let mut parts = Vec::new();
                if let Some(error) = response.error {
                    parts.push(LanguageModelStreamPart::Error {
                        error: json!({
                            "message": error.message,
                            "type": error.error_type,
                            "param": error.param,
                            "code": error.code,
                        }),
                    });
                }
                parts.extend(self.finish_parts());
                parts
            }
            OpenAiResponsesStreamEvent::Error { error } => {
                self.finish_reason = Some(LanguageModelFinishReason::Error);
                let mut parts = vec![LanguageModelStreamPart::Error {
                    error: json!({
                        "message": error.message,
                        "type": error.error_type,
                        "param": error.param,
                        "code": error.code,
                    }),
                }];
                parts.extend(self.finish_parts());
                parts
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
                id: self
                    .text_id
                    .clone()
                    .unwrap_or_else(|| STREAM_TEXT_ID.to_owned()),
                provider_metadata: None,
            });
        }

        let mut tool_ids = self.tool_inputs.keys().cloned().collect::<Vec<_>>();
        tool_ids.sort();
        for id in tool_ids {
            if let Some(state) = self.tool_inputs.get(&id)
                && state.started
            {
                parts.push(LanguageModelStreamPart::ToolInputEnd {
                    id,
                    provider_metadata: None,
                });
            }
        }

        parts.push(LanguageModelStreamPart::Finish {
            usage: self.usage.clone().unwrap_or_else(empty_usage),
            finish_reason: self
                .finish_reason
                .clone()
                .unwrap_or(LanguageModelFinishReason::Stop),
            provider_metadata: None,
        });

        parts
    }
}

pub(super) type ByteStream = Pin<
    Box<
        dyn Stream<Item = std::result::Result<Bytes, Box<dyn std::error::Error + Send + Sync>>>
            + Send,
    >,
>;

pub(super) async fn drive_sse_stream(
    mut bytes_stream: ByteStream,
    abort_signal: Option<CancellationToken>,
    sender: mpsc::Sender<LanguageModelStreamPart>,
    include_raw_chunks: bool,
) {
    let mut parser = OpenAiResponsesSseParser::new(include_raw_chunks);
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
                                "message": "streaming responses request was cancelled",
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
    fn builds_image_prompt_request() {
        let request = OpenAiResponsesRequest::from_call_options(
            "gpt-4.1-mini",
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
            request.input[0],
            OpenAiResponseInputItem::Message { .. }
        ));
    }

    #[test]
    fn parses_non_stream_response_to_generate_result() {
        let response = OpenAiResponse {
            id: "resp_123".to_owned(),
            created_at: 100,
            model: "gpt-4.1-mini".to_owned(),
            status: Some("completed".to_owned()),
            output: vec![OpenAiResponseOutputItem::Message {
                role: Some("assistant".to_owned()),
                content: vec![OpenAiResponseOutputContent::OutputText {
                    text: "hello".to_owned(),
                }],
            }],
            usage: Some(OpenAiResponseUsage {
                input_tokens: Some(2),
                output_tokens: Some(1),
                total_tokens: Some(3),
                input_tokens_details: None,
                output_tokens_details: None,
            }),
            incomplete_details: None,
            error: None,
        };

        let result = response
            .into_generate_result(None, json!({}), None, json!({}))
            .expect("conversion should succeed");

        assert!(matches!(
            result.content,
            LanguageModelContent::Text { ref text, .. } if text == "hello"
        ));
        assert!(matches!(
            result.finish_reason,
            LanguageModelFinishReason::Stop
        ));
    }

    fn sse_event(data: &str) -> Vec<u8> {
        format!("data: {data}\n\n").into_bytes()
    }

    #[test]
    fn sse_parser_text_stream() {
        let mut parser = OpenAiResponsesSseParser::new(false);

        let created = json!({
            "type": "response.created",
            "response": {
                "id": "resp_1",
                "created_at": 1,
                "model": "gpt-4.1-mini",
                "output": []
            }
        });
        let delta = json!({
            "type": "response.output_text.delta",
            "item_id": "msg_1",
            "delta": "Hello"
        });
        let completed = json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "created_at": 1,
                "model": "gpt-4.1-mini",
                "status": "completed",
                "output": [],
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }
        });

        let parts = parser.push_bytes(&sse_event(&created.to_string()));
        assert!(
            parts
                .iter()
                .any(|part| matches!(part, LanguageModelStreamPart::ResponseMetadata { id, .. } if id.as_deref() == Some("resp_1")))
        );

        let parts = parser.push_bytes(&sse_event(&delta.to_string()));
        assert!(
            parts
                .iter()
                .any(|part| matches!(part, LanguageModelStreamPart::TextStart { .. }))
        );
        assert!(
            parts
                .iter()
                .any(|part| matches!(part, LanguageModelStreamPart::TextDelta { delta, .. } if delta == "Hello"))
        );

        let done_parts = parser.push_bytes(&sse_event(&completed.to_string()));
        assert!(
            done_parts
                .iter()
                .any(|part| matches!(part, LanguageModelStreamPart::TextEnd { .. }))
        );
        assert!(
            done_parts
                .iter()
                .any(|part| matches!(part, LanguageModelStreamPart::Finish { .. }))
        );
    }
}
