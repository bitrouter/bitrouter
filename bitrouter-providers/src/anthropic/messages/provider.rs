use std::collections::HashMap;

use bitrouter_core::{
    errors::{BitrouterError, Result},
    models::{
        language::{
            call_options::LanguageModelCallOptions,
            generate_result::LanguageModelGenerateResult,
            language_model::LanguageModel,
            stream_result::{
                LanguageModelStreamResult, LanguageModelStreamResultRequest,
                LanguageModelStreamResultResponse,
            },
        },
        shared::types::JsonValue,
    },
};
use regex::Regex;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use tokio::{select, sync::mpsc};
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
use tokio_util::sync::CancellationToken;

use super::api::{
    ANTHROPIC_PROVIDER_NAME, ByteStream, build_messages_request, drive_sse_stream,
    parse_anthropic_error, response_to_generate_result,
};

const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub api_key: String,
    pub base_url: String,
    pub api_version: String,
    pub default_headers: HeaderMap,
}

impl AnthropicConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: ANTHROPIC_DEFAULT_BASE_URL.to_owned(),
            api_version: ANTHROPIC_API_VERSION.to_owned(),
            default_headers: HeaderMap::new(),
        }
    }
}

#[derive(Clone)]
pub struct AnthropicMessagesModel {
    model_id: String,
    client: reqwest_middleware::ClientWithMiddleware,
    config: AnthropicConfig,
    supported_urls: HashMap<String, Regex>,
}

impl AnthropicMessagesModel {
    pub fn new(model_id: impl Into<String>, api_key: impl Into<String>) -> Self {
        let client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build();
        Self::with_client(model_id, client, AnthropicConfig::new(api_key))
    }

    pub fn with_client(
        model_id: impl Into<String>,
        client: reqwest_middleware::ClientWithMiddleware,
        config: AnthropicConfig,
    ) -> Self {
        Self {
            model_id: model_id.into(),
            client,
            config,
            // Anthropic requires base64-encoded image data; it does not
            // natively support image URLs, so no URL patterns are matched.
            supported_urls: HashMap::new(),
        }
    }

    async fn generate_impl(
        &self,
        options: LanguageModelCallOptions,
    ) -> Result<LanguageModelGenerateResult> {
        let request = build_messages_request(&self.model_id, &options, false)?;
        let request_body = serde_json::to_value(&request).map_err(|error| {
            BitrouterError::invalid_request(
                Some(ANTHROPIC_PROVIDER_NAME),
                format!("failed to serialize messages request: {error}"),
                None,
            )
        })?;
        let (builder, request_headers) = self.request_builder(&request_body, &options.headers)?;
        let response = self
            .send_request(builder, options.abort_signal.clone(), "messages")
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
                        Some(ANTHROPIC_PROVIDER_NAME),
                        format!("failed to decode messages response body: {error}"),
                        None,
                    )
                },
                || {
                    BitrouterError::cancelled(
                        Some(ANTHROPIC_PROVIDER_NAME),
                        "messages response decoding was cancelled",
                    )
                },
            )
            .await?;
        let message: bitrouter_core::api::anthropic::messages::types::MessagesResponse =
            serde_json::from_value(response_body.clone()).map_err(|error| {
                BitrouterError::response_decode(
                    Some(ANTHROPIC_PROVIDER_NAME),
                    format!("failed to parse messages response: {error}"),
                    Some(response_body.clone()),
                )
            })?;

        response_to_generate_result(
            message,
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
        let request = build_messages_request(&self.model_id, &options, true)?;
        let request_body = serde_json::to_value(&request).map_err(|error| {
            BitrouterError::invalid_request(
                Some(ANTHROPIC_PROVIDER_NAME),
                format!("failed to serialize streaming messages request: {error}"),
                None,
            )
        })?;
        let (builder, request_headers) = self.request_builder(&request_body, &options.headers)?;
        let response = self
            .send_request(builder, options.abort_signal.clone(), "streaming messages")
            .await?;
        let response_headers = response.headers().clone();
        if !response.status().is_success() {
            return Err(self.decode_error_response(response).await);
        }

        let include_raw_chunks = options.include_raw_chunks.unwrap_or(false);
        let abort_signal = options.abort_signal.clone();
        let bytes_stream: ByteStream = Box::pin(
            response
                .bytes_stream()
                .map(|r| r.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)),
        );
        let (sender, receiver) = mpsc::channel(32);
        tokio::spawn(drive_sse_stream(
            bytes_stream,
            abort_signal,
            sender,
            include_raw_chunks,
        ));
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
    ) -> Result<(reqwest_middleware::RequestBuilder, HeaderMap)> {
        let endpoint = format!("{}/v1/messages", self.config.base_url.trim_end_matches('/'));
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
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_str(&self.config.api_key).map_err(|error| {
                BitrouterError::invalid_request(
                    Some(ANTHROPIC_PROVIDER_NAME),
                    format!("invalid x-api-key header: {error}"),
                    None,
                )
            })?,
        );
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_str(&self.config.api_version).map_err(|error| {
                BitrouterError::invalid_request(
                    Some(ANTHROPIC_PROVIDER_NAME),
                    format!("invalid anthropic-version header: {error}"),
                    None,
                )
            })?,
        );

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
            .get("request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = match response.text().await {
            Ok(text) if text.trim().is_empty() => None,
            Ok(text) => serde_json::from_str::<JsonValue>(&text)
                .ok()
                .or(Some(JsonValue::String(text))),
            Err(_) => None,
        };

        parse_anthropic_error(status.as_u16(), request_id, body)
    }

    async fn send_request(
        &self,
        builder: reqwest_middleware::RequestBuilder,
        abort_signal: Option<CancellationToken>,
        operation: &str,
    ) -> Result<reqwest::Response> {
        self.await_with_cancellation(
            abort_signal,
            builder.send(),
            |error| {
                BitrouterError::transport(
                    Some(ANTHROPIC_PROVIDER_NAME),
                    format!("failed to send {operation} request: {error}"),
                )
            },
            || {
                BitrouterError::cancelled(
                    Some(ANTHROPIC_PROVIDER_NAME),
                    format!("{operation} request was cancelled"),
                )
            },
        )
        .await
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

impl LanguageModel for AnthropicMessagesModel {
    fn provider_name(&self) -> &str {
        ANTHROPIC_PROVIDER_NAME
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn supported_urls(&self) -> impl Future<Output = HashMap<String, Regex>> {
        let supported_urls = self.supported_urls.clone();
        async move { supported_urls }
    }

    async fn generate(
        &self,
        options: LanguageModelCallOptions,
    ) -> Result<LanguageModelGenerateResult> {
        self.generate_impl(options).await
    }

    async fn stream(&self, options: LanguageModelCallOptions) -> Result<LanguageModelStreamResult> {
        self.stream_impl(options).await
    }
}
