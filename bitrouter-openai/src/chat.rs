use std::collections::HashMap;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use bitrouter_core::{
    errors::{BitrouterError, Result},
    models::{
        language::{
            call_options::{LanguageModelCallOptions, LanguageModelResponseFormat},
            data_content::LanguageModelDataContent,
            language_model::LanguageModel,
            prompt::{
                LanguageModelAssistantContent, LanguageModelMessage, LanguageModelToolResult,
                LanguageModelToolResultOutput, LanguageModelToolResultOutputContent,
                LanguageModelToolResultOutputContentFileId, LanguageModelUserContent,
            },
            stream_part::LanguageModelStreamPart,
            stream_result::{
                LanguageModelStreamResult, LanguageModelStreamResultRequest,
                LanguageModelStreamResultResponse,
            },
            tool::{LanguageModelTool, ProviderToolId},
            tool_choice::LanguageModelToolChoice,
            usage::{LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage},
        },
        shared::{types::JsonValue, warnings::Warning},
    },
};
use regex::Regex;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{select, sync::mpsc};
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
use tokio_util::sync::CancellationToken;

use crate::responses::{
    OpenAiChatCompletionChunk, OpenAiChatCompletionResponse, OpenAiChunkDeltaToolCall,
    OpenAiErrorEnvelope, map_finish_reason, parse_openai_error,
};

const OPENAI_PROVIDER_NAME: &str = "openai";
const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const STREAM_TEXT_ID: &str = "text";

#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    pub api_key: String,
    pub base_url: String,
    pub organization: Option<String>,
    pub project: Option<String>,
    pub default_headers: HeaderMap,
}

impl OpenAiConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: OPENAI_DEFAULT_BASE_URL.to_owned(),
            organization: None,
            project: None,
            default_headers: HeaderMap::new(),
        }
    }
}

#[derive(Clone)]
pub struct OpenAiChatCompletionsModel {
    client: reqwest::Client,
    config: OpenAiConfig,
    model_id: String,
    supported_urls: HashMap<String, Regex>,
}

impl OpenAiChatCompletionsModel {
    pub fn new(model_id: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::with_client(reqwest::Client::new(), model_id, OpenAiConfig::new(api_key))
    }

    pub fn with_client(
        client: reqwest::Client,
        model_id: impl Into<String>,
        config: OpenAiConfig,
    ) -> Self {
        Self {
            client,
            config,
            model_id: model_id.into(),
            supported_urls: HashMap::from([
                (
                    "image/png".to_owned(),
                    Regex::new(r"^https?://").expect("static regex must compile"),
                ),
                (
                    "image/jpeg".to_owned(),
                    Regex::new(r"^https?://").expect("static regex must compile"),
                ),
                (
                    "image/webp".to_owned(),
                    Regex::new(r"^https?://").expect("static regex must compile"),
                ),
                (
                    "image/gif".to_owned(),
                    Regex::new(r"^https?://").expect("static regex must compile"),
                ),
            ]),
        }
    }

    async fn generate_impl(
        &self,
        options: LanguageModelCallOptions,
    ) -> Result<bitrouter_core::models::language::generate_result::LanguageModelGenerateResult>
    {
        let request = OpenAiChatCompletionsRequest::from_call_options(
            self.model_id.clone(),
            &options,
            false,
        )?;
        let request_body = serde_json::to_value(&request).map_err(|error| {
            BitrouterError::invalid_request(
                Some(OPENAI_PROVIDER_NAME),
                format!("failed to serialize chat completion request: {error}"),
                None,
            )
        })?;
        let (builder, request_headers) = self.request_builder(&request_body, &options.headers)?;
        let response = self
            .send_request(builder, options.abort_signal.clone(), "chat completion")
            .await?;

        let response_headers = response.headers().clone();
        if !response.status().is_success() {
            return Err(self.decode_error_response(response).await);
        }

        let response_body: JsonValue = self
            .await_with_cancellation(
                options.abort_signal.clone(),
                response.json(),
                |error| {
                    BitrouterError::response_decode(
                        Some(OPENAI_PROVIDER_NAME),
                        format!("failed to decode chat completion response body: {error}"),
                        None,
                    )
                },
                || {
                    BitrouterError::cancelled(
                        Some(OPENAI_PROVIDER_NAME),
                        "chat completion response decoding was cancelled",
                    )
                },
            )
            .await?;
        let completion: OpenAiChatCompletionResponse =
            serde_json::from_value(response_body.clone()).map_err(|error| {
                BitrouterError::response_decode(
                    Some(OPENAI_PROVIDER_NAME),
                    format!("failed to parse chat completion response: {error}"),
                    Some(response_body.clone()),
                )
            })?;

        completion.into_generate_result(
            Some(request_headers),
            request_body,
            Some(response_headers),
            response_body,
        )
    }

    async fn stream_impl(
        &self,
        options: LanguageModelCallOptions,
    ) -> Result<LanguageModelStreamResult> {
        let request =
            OpenAiChatCompletionsRequest::from_call_options(self.model_id.clone(), &options, true)?;
        let request_body = serde_json::to_value(&request).map_err(|error| {
            BitrouterError::invalid_request(
                Some(OPENAI_PROVIDER_NAME),
                format!("failed to serialize streaming chat completion request: {error}"),
                None,
            )
        })?;
        let (builder, request_headers) = self.request_builder(&request_body, &options.headers)?;
        let response = self
            .send_request(
                builder,
                options.abort_signal.clone(),
                "streaming chat completion",
            )
            .await?;
        let response_headers = response.headers().clone();
        if !response.status().is_success() {
            return Err(self.decode_error_response(response).await);
        }

        let include_raw_chunks = options.include_raw_chunks.unwrap_or(false);
        let mut bytes_stream = response.bytes_stream();
        let abort_signal = options.abort_signal.clone();
        let (sender, receiver) = mpsc::channel(32);
        tokio::spawn(async move {
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
        });
        let stream = Box::pin(ReceiverStream::new(receiver));

        Ok(LanguageModelStreamResult {
            stream,
            request: Some(LanguageModelStreamResultRequest {
                headers: Some(request_headers),
                body: Some(request_body),
            }),
            response: Some(LanguageModelStreamResultResponse {
                headers: Some(response_headers),
            }),
        })
    }

    fn request_builder(
        &self,
        request_body: &JsonValue,
        extra_headers: &Option<HeaderMap>,
    ) -> Result<(reqwest::RequestBuilder, HeaderMap)> {
        let endpoint = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let headers = self.build_headers(extra_headers)?;
        let request_headers = headers.clone();
        let builder = self
            .client
            .post(endpoint)
            .headers(headers)
            .json(request_body);

        Ok((builder, request_headers))
    }

    fn build_headers(&self, extra_headers: &Option<HeaderMap>) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.config.api_key)).map_err(|error| {
                BitrouterError::invalid_request(
                    Some(OPENAI_PROVIDER_NAME),
                    format!("invalid Authorization header: {error}"),
                    None,
                )
            })?,
        );

        if let Some(organization) = &self.config.organization {
            insert_header(&mut headers, "OpenAI-Organization", organization)?;
        }
        if let Some(project) = &self.config.project {
            insert_header(&mut headers, "OpenAI-Project", project)?;
        }

        for (name, value) in &self.config.default_headers {
            headers.insert(name, value.clone());
        }

        if let Some(extra_headers) = extra_headers {
            for (name, value) in extra_headers {
                headers.insert(name, value.clone());
            }
        }

        Ok(headers)
    }

    async fn decode_error_response(&self, response: reqwest::Response) -> BitrouterError {
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = match response.text().await {
            Ok(text) if text.trim().is_empty() => None,
            Ok(text) => serde_json::from_str::<JsonValue>(&text)
                .ok()
                .or_else(|| Some(JsonValue::String(text))),
            Err(_) => None,
        };

        parse_openai_error(status.as_u16(), request_id, body)
    }

    async fn send_request(
        &self,
        builder: reqwest::RequestBuilder,
        abort_signal: Option<CancellationToken>,
        operation: &str,
    ) -> Result<reqwest::Response> {
        let response = self
            .await_with_cancellation(
                abort_signal,
                builder.send(),
                |error| {
                    BitrouterError::transport(
                        Some(OPENAI_PROVIDER_NAME),
                        format!("failed to send {operation} request: {error}"),
                    )
                },
                || {
                    BitrouterError::cancelled(
                        Some(OPENAI_PROVIDER_NAME),
                        format!("{operation} request was cancelled"),
                    )
                },
            )
            .await?;
        Ok(response)
    }

    async fn await_with_cancellation<F, T, E, M, C>(
        &self,
        abort_signal: Option<CancellationToken>,
        future: F,
        map_error: M,
        cancelled: C,
    ) -> Result<T>
    where
        F: std::future::Future<Output = std::result::Result<T, E>>,
        M: FnOnce(E) -> BitrouterError,
        C: FnOnce() -> BitrouterError,
    {
        if let Some(token) = abort_signal {
            select! {
                _ = token.cancelled() => Err(cancelled()),
                result = future => result.map_err(map_error),
            }
        } else {
            future.await.map_err(map_error)
        }
    }
}

impl LanguageModel for OpenAiChatCompletionsModel {
    fn provider_name(&self) -> &str {
        OPENAI_PROVIDER_NAME
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn supported_urls(&self) -> impl Future<Output = HashMap<String, Regex>> {
        let supported_urls = self.supported_urls.clone();
        async move { supported_urls }
    }

    fn generate(
        &self,
        options: LanguageModelCallOptions,
    ) -> impl Future<
        Output = Result<
            bitrouter_core::models::language::generate_result::LanguageModelGenerateResult,
        >,
    > {
        async move { self.generate_impl(options).await }
    }

    fn stream(
        &self,
        options: LanguageModelCallOptions,
    ) -> impl Future<Output = Result<LanguageModelStreamResult>> {
        async move { self.stream_impl(options).await }
    }
}

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

impl OpenAiChatCompletionsRequest {
    pub fn from_call_options(
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

        let tools = options
            .tools
            .as_ref()
            .map(|tools| convert_tools(tools.as_slice()))
            .transpose()?;
        let has_tools = tools
            .as_ref()
            .is_some_and(|tools: &Vec<OpenAiChatTool>| !tools.is_empty());

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
            response_format: options
                .response_format
                .as_ref()
                .map(convert_response_format)
                .transpose()?,
            seed: options.seed,
            tools,
            tool_choice: options.tool_choice.as_ref().map(convert_tool_choice),
            parallel_tool_calls: has_tools.then_some(false),
        })
    }
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

#[derive(Default)]
struct OpenAiSseParser {
    buffer: Vec<u8>,
    state: OpenAiStreamState,
    include_raw_chunks: bool,
}

impl OpenAiSseParser {
    fn new(include_raw_chunks: bool) -> Self {
        Self {
            include_raw_chunks,
            ..Self::default()
        }
    }

    fn is_finished(&self) -> bool {
        self.state.finished
    }

    fn push_bytes(&mut self, bytes: &[u8]) -> Vec<LanguageModelStreamPart> {
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

    fn finish(&mut self) -> Vec<LanguageModelStreamPart> {
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
            self.usage = Some(usage.into_language_model_usage());
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

fn convert_tools(tools: &[LanguageModelTool]) -> Result<Vec<OpenAiChatTool>> {
    let mut converted = Vec::with_capacity(tools.len());
    for tool in tools {
        match tool {
            LanguageModelTool::Function {
                name,
                description,
                input_schema,
                strict,
                ..
            } => converted.push(OpenAiChatTool {
                kind: "function".to_owned(),
                function: OpenAiChatToolFunction {
                    name: name.clone(),
                    description: description.clone(),
                    parameters: input_schema.clone(),
                    strict: *strict,
                },
            }),
            LanguageModelTool::Provider { id, .. } => {
                return Err(provider_tool_not_supported(id));
            }
        }
    }

    Ok(converted)
}

fn provider_tool_not_supported(id: &ProviderToolId) -> BitrouterError {
    BitrouterError::unsupported(
		OPENAI_PROVIDER_NAME,
		format!("provider tool {}:{}", id.provider_name, id.tool_id),
		Some("OpenAI chat completions supports function and custom tools, but bitrouter-core provider tools do not map cleanly here".to_owned()),
	)
}

fn convert_tool_choice(tool_choice: &LanguageModelToolChoice) -> OpenAiToolChoice {
    match tool_choice {
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

fn convert_response_format(
    response_format: &LanguageModelResponseFormat,
) -> Result<OpenAiResponseFormat> {
    match response_format {
        LanguageModelResponseFormat::Text => Ok(OpenAiResponseFormat::Text),
        LanguageModelResponseFormat::Json {
            schema,
            name,
            description,
        } => Ok(match schema {
            Some(schema) => OpenAiResponseFormat::JsonSchema {
                json_schema: OpenAiJsonSchemaConfig {
                    name: name.clone().unwrap_or_else(|| "output".to_owned()),
                    description: description.clone(),
                    schema: schema.clone(),
                    strict: Some(true),
                },
            },
            None => OpenAiResponseFormat::JsonObject,
        }),
    }
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<()> {
    let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
        BitrouterError::invalid_request(
            Some(OPENAI_PROVIDER_NAME),
            format!("invalid header name {name}: {error}"),
            None,
        )
    })?;
    let header_value = HeaderValue::from_str(value).map_err(|error| {
        BitrouterError::invalid_request(
            Some(OPENAI_PROVIDER_NAME),
            format!("invalid header value for {name}: {error}"),
            None,
        )
    })?;
    headers.insert(header_name, header_value);
    Ok(())
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
    use bitrouter_core::models::language::{
        call_options::LanguageModelCallOptions,
        prompt::{LanguageModelMessage, LanguageModelUserContent},
    };

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
