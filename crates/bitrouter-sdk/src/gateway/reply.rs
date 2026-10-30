//! First-party relay replies, without an HTTP server or framework dependency.

use bitrouter_ai::protocol::{
    chat_completions::{ChatChunk, ChatRequest, ChatResponse},
    messages::{MessagesEvent, MessagesRequest, MessagesResponse},
    responses::{ResponsesEvent, ResponsesRequest, ResponsesResponse},
};
use reqwest::header::{self, HeaderMap};
use serde::{Serialize, de::DeserializeOwned};

use super::GatewayClient;
use crate::{
    error::{Error, Result, ServiceFailure},
    stream::{Protocol, RelayStream},
    transport,
};

/// Bounded response metadata. These values grant no replay or control authority.
/// Consumers explicitly choose which values to expose on their own HTTP hop.
#[derive(Debug)]
pub struct Metadata {
    /// Successful upstream HTTP status before body consumption.
    pub status: u16,
    /// Optional service correlation identity.
    pub request_id: Option<String>,
    /// Optional execution identity; never inferred when absent.
    pub execution_id: Option<String>,
}

/// Same-protocol output, selected by the request's stream flag.
pub enum Body<T, E> {
    /// A fully decoded public JSON response.
    Json(T),
    /// Public events with an explicit end; errors never reconnect the stream.
    Stream(RelayStream<E>),
}

/// Inspect metadata before forwarding or transforming the public response body.
pub struct Reply<T, E> {
    /// Allowlisted, validated metadata; no raw headers are forwarded.
    pub metadata: Metadata,
    /// JSON or a lazily consumed event stream.
    pub body: Body<T, E>,
}

impl GatewayClient {
    /// Call Chat Completions and retain relay metadata and explicit termination.
    ///
    /// # Errors
    /// Returns request/transport/service/contract errors, without retry. A body
    /// failure may follow accepted execution and does not authorize redispatch.
    pub async fn chat_reply(
        &self,
        request: &ChatRequest,
    ) -> Result<Reply<ChatResponse, ChatChunk>> {
        self.relay(
            "v1/chat/completions",
            request,
            request.stream,
            Protocol::Chat,
        )
        .await
    }

    /// Call Messages with the request's JSON/stream mode.
    ///
    /// # Errors
    /// Returns transport/service/contract errors; accepted execution may persist.
    pub async fn messages_reply(
        &self,
        request: &MessagesRequest,
    ) -> Result<Reply<MessagesResponse, MessagesEvent>> {
        self.relay("v1/messages", request, request.stream, Protocol::Messages)
            .await
    }

    /// Call Responses, retaining completed/incomplete/failed protocol ends.
    ///
    /// # Errors
    /// Returns transport/service/contract errors; accepted execution may persist.
    pub async fn responses_reply(
        &self,
        request: &ResponsesRequest,
    ) -> Result<Reply<ResponsesResponse, ResponsesEvent>> {
        self.relay("v1/responses", request, request.stream, Protocol::Responses)
            .await
    }

    async fn relay<T: DeserializeOwned, E: DeserializeOwned + Send + 'static>(
        &self,
        path: &str,
        request: &impl Serialize,
        streaming: bool,
        protocol: Protocol,
    ) -> Result<Reply<T, E>> {
        let accept = if streaming {
            "text/event-stream"
        } else {
            "application/json"
        };
        let response = self
            .http
            .post(self.url(path)?)
            .header(header::ACCEPT, accept)
            .json(request)
            .send()
            .await
            .map_err(Error::transport)?;
        let response = super::execution::reject_existing(response)?;
        let response = transport::successful(response, self.options.max_response_bytes).await?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let failure = |error| {
            Error::Service(Box::new(ServiceFailure {
                status,
                headers: headers.clone(),
                body: None,
                body_error: Some(error),
            }))
        };
        let metadata = metadata(status, &headers).map_err(failure)?;
        if !transport::has_content_type(&response, accept) {
            return Err(failure(Error::Response("unexpected relay content type")));
        }
        let body = if streaming {
            Body::Stream(crate::stream::decode_relay(
                response,
                self.options.max_event_bytes,
                protocol,
            ))
        } else {
            let bytes = transport::body(response, self.options.max_response_bytes)
                .await
                .map_err(failure)?;
            Body::Json(serde_json::from_slice(&bytes).map_err(|_| {
                failure(Error::Response(
                    "body does not match the public JSON contract",
                ))
            })?)
        };
        Ok(Reply { metadata, body })
    }
}

fn metadata(status: u16, headers: &HeaderMap) -> Result<Metadata> {
    fn identity(headers: &HeaderMap, name: &str) -> Result<Option<String>> {
        let mut values = headers.get_all(name).iter();
        let Some(value) = values.next() else {
            return Ok(None);
        };
        if values.next().is_some() {
            return Err(Error::Response("duplicate relay identity"));
        }
        let value = value
            .to_str()
            .map_err(|_| Error::Response("invalid relay identity"))?;
        if value.is_empty()
            || value.len() > 256
            || !value.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(Error::Response("invalid relay identity"));
        }
        Ok(Some(value.into()))
    }
    Ok(Metadata {
        status,
        request_id: identity(headers, "x-bitrouter-request-id")?,
        execution_id: identity(headers, "x-bitrouter-execution-id")?,
    })
}
