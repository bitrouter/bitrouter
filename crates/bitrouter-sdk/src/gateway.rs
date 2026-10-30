//! Explicit HTTP access to an independently running gateway.

mod core;
mod eval;
pub mod execution;
pub mod reply;

use std::time::Duration;

use bitrouter_ai::{
    decisions::DecisionRequest,
    gateway::{HealthResponse, ModelsResponse},
    protocol::{
        chat_completions::{ChatChunk, ChatRequest, ChatResponse},
        decisions::DecisionsResponse,
        messages::{MessagesEvent, MessagesRequest, MessagesResponse},
        responses::{ResponsesEvent, ResponsesRequest, ResponsesResponse},
    },
};
use reqwest::{Response, header};
use serde::{Serialize, de::DeserializeOwned};
use url::Url;

use crate::{
    error::{Error, Result},
    stream::{EventStream, Protocol},
    transport,
};

/// HTTP protocol selection for ordinary requests and SSE streams.
///
/// Protocol selection never retries a request using another HTTP version.
/// Core WebSocket upgrades always use a separate HTTP/1.1 client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum HttpVersion {
    /// Negotiate HTTP/2 or HTTP/1.1 over HTTPS using ALPN when the `http2`
    /// feature is enabled. Cleartext HTTP uses HTTP/1.1 without an h2c upgrade.
    /// Without `http2`, use HTTP/1.1 even if another dependency enables it
    /// on reqwest through Cargo feature unification.
    #[default]
    Auto,
    /// Use HTTP/1.1 for both HTTP and HTTPS endpoints.
    Http1Only,
    /// Require HTTP/2, sending the h2c preface directly for cleartext HTTP
    /// and selecting only HTTP/2 over HTTPS. The endpoint must support HTTP/2;
    /// a failed request is not retried over HTTP/1.1.
    #[cfg(feature = "http2")]
    Http2PriorKnowledge,
}

/// Transport bounds and protocol choice. No total inference deadline is imposed by default.
#[derive(Clone, Debug)]
pub struct TransportOptions {
    /// HTTP version policy; defaults to [`HttpVersion::Auto`].
    pub http_version: HttpVersion,
    /// Opt in to reqwest's environment/system proxy discovery.
    pub system_proxy: bool,
    pub connect_timeout: Duration,
    /// Maximum wait for a response read; `None` disables this timeout.
    pub read_timeout: Option<Duration>,
    pub request_timeout: Option<Duration>,
    pub max_response_bytes: usize,
    pub max_event_bytes: usize,
}

impl Default for TransportOptions {
    fn default() -> Self {
        Self {
            http_version: HttpVersion::default(),
            system_proxy: false,
            connect_timeout: Duration::from_secs(10),
            read_timeout: Some(Duration::from_secs(120)),
            request_timeout: None,
            max_response_bytes: 16 * 1024 * 1024,
            max_event_bytes: 8 * 1024 * 1024,
        }
    }
}

/// A client for one gateway origin or reverse-proxy prefix.
///
/// An endpoint may end in `/v1`; otherwise that path is appended for model
/// calls. Health and metrics use the corresponding service prefix. HTTP is
/// supported for local/private service networks. Use HTTPS across untrusted
/// networks. Credentials are never sent through redirects.
#[derive(Clone)]
pub struct GatewayClient {
    endpoint: Url,
    http: reqwest::Client,
    #[cfg(feature = "core")]
    channel_http: reqwest::Client,
    options: TransportOptions,
}

impl std::fmt::Debug for GatewayClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayClient")
            .field("endpoint", &self.endpoint)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl GatewayClient {
    pub fn new(endpoint: &str, credential: Option<&str>) -> Result<Self> {
        Self::with_options(endpoint, credential, TransportOptions::default())
    }

    /// Construct without environment, filesystem or daemon discovery.
    pub fn with_options(
        endpoint: &str,
        credential: Option<&str>,
        options: TransportOptions,
    ) -> Result<Self> {
        let endpoint = endpoint_url(endpoint)?;
        if options.max_response_bytes == 0 || options.max_event_bytes == 0 {
            return Err(Error::Options(
                "response and event byte limits must be positive",
            ));
        }
        let mut headers = header::HeaderMap::new();
        if let Some(credential) = credential {
            if credential.trim().is_empty() {
                return Err(Error::Options("credential must not be empty"));
            }
            let mut bearer = header::HeaderValue::from_str(&format!("Bearer {credential}"))
                .map_err(|_| Error::Options("credential is not a valid HTTP header value"))?;
            bearer.set_sensitive(true);
            headers.insert(header::AUTHORIZATION, bearer);
        }
        #[cfg(feature = "core")]
        let channel_http = transport::build_http(headers.clone(), &options, true)?;
        let http = transport::build_http(headers, &options, false)?;
        Ok(Self {
            endpoint,
            http,
            #[cfg(feature = "core")]
            channel_http,
            options,
        })
    }

    pub async fn models(&self) -> Result<ModelsResponse> {
        self.get("v1/models").await
    }
    pub async fn health(&self) -> Result<HealthResponse> {
        self.get("health").await
    }

    pub async fn metrics(&self) -> Result<String> {
        let response = self
            .http
            .get(self.url("metrics")?)
            .send()
            .await
            .map_err(Error::transport)?;
        let response = transport::successful(response, self.options.max_response_bytes).await?;
        String::from_utf8(transport::body(response, self.options.max_response_bytes).await?)
            .map_err(|_| Error::Response("metrics body is not UTF-8"))
    }

    pub async fn chat_completions(&self, request: &ChatRequest) -> Result<ChatResponse> {
        require_stream(request.stream, false)?;
        self.post("v1/chat/completions", request).await
    }

    pub async fn messages(&self, request: &MessagesRequest) -> Result<MessagesResponse> {
        require_stream(request.stream, false)?;
        self.post("v1/messages", request).await
    }

    pub async fn responses(&self, request: &ResponsesRequest) -> Result<ResponsesResponse> {
        require_stream(request.stream, false)?;
        self.post("v1/responses", request).await
    }

    pub async fn decisions(&self, request: &DecisionRequest) -> Result<DecisionsResponse> {
        self.post("v1/decisions", request).await
    }

    pub async fn stream_chat_completions(
        &self,
        request: &ChatRequest,
    ) -> Result<EventStream<ChatChunk>> {
        require_stream(request.stream, true)?;
        self.events("v1/chat/completions", request, Protocol::Chat)
            .await
    }

    pub async fn stream_messages(
        &self,
        request: &MessagesRequest,
    ) -> Result<EventStream<MessagesEvent>> {
        require_stream(request.stream, true)?;
        self.events("v1/messages", request, Protocol::Messages)
            .await
    }

    pub async fn stream_responses(
        &self,
        request: &ResponsesRequest,
    ) -> Result<EventStream<ResponsesEvent>> {
        require_stream(request.stream, true)?;
        self.events("v1/responses", request, Protocol::Responses)
            .await
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self
            .http
            .get(self.url(path)?)
            .send()
            .await
            .map_err(Error::transport)?;
        self.json(response).await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, request: &impl Serialize) -> Result<T> {
        let response = self
            .http
            .post(self.url(path)?)
            .json(request)
            .send()
            .await
            .map_err(Error::transport)?;
        let response = execution::reject_existing(response)?;
        self.json(response).await
    }

    async fn json<T: DeserializeOwned>(&self, response: Response) -> Result<T> {
        let response = transport::successful(response, self.options.max_response_bytes).await?;
        if !transport::has_content_type(&response, "application/json") {
            return Err(Error::Response("expected application/json"));
        }
        serde_json::from_slice(&transport::body(response, self.options.max_response_bytes).await?)
            .map_err(|_| Error::Response("body does not match the public JSON contract"))
    }

    async fn events<T: DeserializeOwned + Send + 'static>(
        &self,
        path: &str,
        request: &impl Serialize,
        protocol: Protocol,
    ) -> Result<EventStream<T>> {
        let response = self
            .http
            .post(self.url(path)?)
            .header(header::ACCEPT, "text/event-stream")
            .json(request)
            .send()
            .await
            .map_err(Error::transport)?;
        let response = execution::reject_existing(response)?;
        let response = transport::successful(response, self.options.max_response_bytes).await?;
        if !transport::has_content_type(&response, "text/event-stream") {
            return Err(Error::Response("expected text/event-stream"));
        }
        Ok(crate::stream::decode(
            response,
            self.options.max_event_bytes,
            protocol,
        ))
    }

    fn url(&self, path: &str) -> Result<Url> {
        self.endpoint
            .join(path)
            .map_err(|_| Error::Options("invalid service path"))
    }
}

fn require_stream(actual: bool, required: bool) -> Result<()> {
    if actual != required {
        return Err(Error::Request(
            "request.stream must match the selected client method",
        ));
    }
    Ok(())
}

fn endpoint_url(endpoint: &str) -> Result<Url> {
    let mut url = Url::parse(endpoint)
        .map_err(|_| Error::Options("endpoint must be an absolute HTTP(S) URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err(Error::Options("endpoint must be an absolute HTTP(S) URL"));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Options(
            "endpoint must not contain credentials, a query or a fragment",
        ));
    }
    let path = url.path().trim_end_matches('/');
    let path = path.strip_suffix("/v1").unwrap_or(path);
    url.set_path(&format!("{path}/"));
    Ok(url)
}
