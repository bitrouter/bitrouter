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
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use tokio::{select, sync::mpsc};
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
use tokio_util::sync::CancellationToken;

use crate::chat::provider::OpenAiConfig;

use super::api::{
    ByteStream, OPENAI_PROVIDER_NAME, OpenAiResponse, OpenAiResponsesRequest, drive_sse_stream,
    parse_openai_error,
};

#[derive(Clone)]
pub struct OpenAiResponsesModel {
    client: reqwest::Client,
    config: OpenAiConfig,
    supported_urls: HashMap<String, Regex>,
}

impl OpenAiResponsesModel {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_client(reqwest::Client::new(), OpenAiConfig::new(api_key))
    }

    pub fn with_client(client: reqwest::Client, config: OpenAiConfig) -> Self {
        let http_url_regex = Regex::new(r"^https?://").expect("static regex must compile");
        Self {
            client,
            config,
            supported_urls: HashMap::from([
                ("image/png".to_owned(), http_url_regex.clone()),
                ("image/jpeg".to_owned(), http_url_regex.clone()),
                ("image/webp".to_owned(), http_url_regex.clone()),
                ("image/gif".to_owned(), http_url_regex),
            ]),
        }
    }

    async fn generate_impl(
        &self,
        model_id: &str,
        options: LanguageModelCallOptions,
    ) -> Result<LanguageModelGenerateResult> {
        let request =
            OpenAiResponsesRequest::from_call_options(model_id.to_owned(), &options, false)?;
        let request_body = serde_json::to_value(&request).map_err(|error| {
            BitrouterError::invalid_request(
                Some(OPENAI_PROVIDER_NAME),
                format!("failed to serialize responses request: {error}"),
                None,
            )
        })?;
        let (builder, request_headers) = self.request_builder(&request_body, &options.headers)?;
        let response = self
            .send_request(builder, options.abort_signal.clone(), "responses request")
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
                        format!("failed to decode responses response body: {error}"),
                        None,
                    )
                },
                || {
                    BitrouterError::cancelled(
                        Some(OPENAI_PROVIDER_NAME),
                        "responses response decoding was cancelled",
                    )
                },
            )
            .await?;
        let completion: OpenAiResponse =
            serde_json::from_value(response_body.clone()).map_err(|error| {
                BitrouterError::response_decode(
                    Some(OPENAI_PROVIDER_NAME),
                    format!("failed to parse responses response: {error}"),
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
        model_id: &str,
        options: LanguageModelCallOptions,
    ) -> Result<LanguageModelStreamResult> {
        let request =
            OpenAiResponsesRequest::from_call_options(model_id.to_owned(), &options, true)?;
        let request_body = serde_json::to_value(&request).map_err(|error| {
            BitrouterError::invalid_request(
                Some(OPENAI_PROVIDER_NAME),
                format!("failed to serialize streaming responses request: {error}"),
                None,
            )
        })?;
        let (builder, request_headers) = self.request_builder(&request_body, &options.headers)?;
        let response = self
            .send_request(
                builder,
                options.abort_signal.clone(),
                "streaming responses request",
            )
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
    ) -> Result<(reqwest::RequestBuilder, HeaderMap)> {
        let endpoint = format!("{}/responses", self.config.base_url.trim_end_matches('/'));
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
                .or(Some(JsonValue::String(text))),
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
        self.await_with_cancellation(
            abort_signal,
            builder.send(),
            |error| {
                BitrouterError::transport(
                    Some(OPENAI_PROVIDER_NAME),
                    format!("failed to send {operation}: {error}"),
                )
            },
            || {
                BitrouterError::cancelled(
                    Some(OPENAI_PROVIDER_NAME),
                    format!("{operation} was cancelled"),
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

impl LanguageModel for OpenAiResponsesModel {
    fn provider_name(&self) -> &str {
        OPENAI_PROVIDER_NAME
    }

    fn supported_urls(&self) -> impl Future<Output = HashMap<String, Regex>> {
        let supported_urls = self.supported_urls.clone();
        async move { supported_urls }
    }

    async fn generate(
        &self,
        model_id: &str,
        options: LanguageModelCallOptions,
    ) -> Result<LanguageModelGenerateResult> {
        self.generate_impl(model_id, options).await
    }

    async fn stream(
        &self,
        model_id: &str,
        options: LanguageModelCallOptions,
    ) -> Result<LanguageModelStreamResult> {
        self.stream_impl(model_id, options).await
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
