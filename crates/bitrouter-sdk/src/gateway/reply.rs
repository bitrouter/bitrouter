//! First-party relay replies, without an HTTP server or framework dependency.

use bitrouter_ai::protocol::{
    chat_completions::{ChatChunk, ChatRequest, ChatResponse},
    messages::{MessagesEvent, MessagesRequest, MessagesResponse},
    responses::{ResponsesEvent, ResponsesRequest, ResponsesResponse},
};
use reqwest::header::{self, HeaderMap};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

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
    /// Stable handle for reading a BitRouter background stream.
    pub background_request_id: Option<String>,
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

    /// Relay native Chat JSON without filling absent request fields or decoding event payloads.
    ///
    /// # Errors
    /// Returns request, transport, service or framing errors without retry.
    pub async fn chat_event_reply(&self, request: &Value) -> Result<Reply<ChatResponse, Value>> {
        let streaming = request["stream"].as_bool().ok_or(Error::Request(
            "raw native relay requires a boolean stream field",
        ))?;
        self.relay("v1/chat/completions", request, streaming, Protocol::Chat)
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

    /// Relay native Responses JSON without filling absent request fields or decoding event payloads.
    ///
    /// # Errors
    /// Returns request, transport, service or framing errors without retry.
    pub async fn responses_event_reply(
        &self,
        request: &Value,
    ) -> Result<Reply<ResponsesResponse, Value>> {
        let streaming = request["stream"].as_bool().ok_or(Error::Request(
            "raw native relay requires a boolean stream field",
        ))?;
        self.relay("v1/responses", request, streaming, Protocol::Responses)
            .await
    }

    /// Reattach to one already-created native background Responses stream.
    ///
    /// # Errors
    /// A missing result or interrupted stream is not proof of non-execution.
    pub async fn resume_responses_reply(
        &self,
        response_id: &str,
        starting_after: u64,
    ) -> Result<Reply<ResponsesResponse, ResponsesEvent>> {
        self.relay_request(
            self.responses_resume(response_id, starting_after)?,
            true,
            Protocol::Responses,
        )
        .await
    }

    /// Reattach to the original Responses stream and retain native event JSON.
    ///
    /// # Errors
    /// Returns identity, transport, service or framing errors without retry.
    pub async fn resume_responses_event_reply(
        &self,
        response_id: &str,
        starting_after: u64,
    ) -> Result<Reply<ResponsesResponse, Value>> {
        self.relay_request(
            self.responses_resume(response_id, starting_after)?,
            true,
            Protocol::Responses,
        )
        .await
    }

    fn responses_resume(
        &self,
        response_id: &str,
        starting_after: u64,
    ) -> Result<reqwest::RequestBuilder> {
        if response_id.is_empty()
            || response_id.len() > 256
            || !response_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(Error::Request("invalid native Responses identity"));
        }
        let path = format!("v1/responses/{response_id}");
        Ok(self.http.get(self.url(&path)?).query(&[
            ("stream", "true".to_owned()),
            ("starting_after", starting_after.to_string()),
        ]))
    }

    /// Replay committed native Chat events using the market base URL (e.g. `/market`).
    ///
    /// # Errors
    /// Returns an error for an invalid request ID or a failed replay connection.
    pub async fn resume_background_chat(
        &self,
        request_id: &str,
        after: Option<u64>,
    ) -> Result<Reply<ChatResponse, ChatChunk>> {
        self.relay_request(
            self.background_replay(request_id, after)?,
            true,
            Protocol::Chat,
        )
        .await
    }

    /// Replay committed native Messages events using the market base URL (e.g. `/market`).
    ///
    /// # Errors
    /// Returns an error for an invalid request ID or a failed replay connection.
    pub async fn resume_background_messages(
        &self,
        request_id: &str,
        after: Option<u64>,
    ) -> Result<Reply<MessagesResponse, MessagesEvent>> {
        self.relay_request(
            self.background_replay(request_id, after)?,
            true,
            Protocol::Messages,
        )
        .await
    }

    fn background_replay(
        &self,
        request_id: &str,
        after: Option<u64>,
    ) -> Result<reqwest::RequestBuilder> {
        let request_id = uuid::Uuid::parse_str(request_id)
            .map_err(|_| Error::Request("invalid background request ID"))?;
        let path = format!("v1/requests/{request_id}/events");
        let request = self.http.get(self.url(&path)?);
        Ok(match after {
            Some(cursor) => request.query(&[("after", cursor)]),
            None => request,
        })
    }

    async fn relay<T: DeserializeOwned, E: DeserializeOwned + Send + 'static>(
        &self,
        path: &str,
        request: &impl Serialize,
        streaming: bool,
        protocol: Protocol,
    ) -> Result<Reply<T, E>> {
        self.relay_request(
            self.http.post(self.url(path)?).json(request),
            streaming,
            protocol,
        )
        .await
    }

    async fn relay_request<T: DeserializeOwned, E: DeserializeOwned + Send + 'static>(
        &self,
        request: reqwest::RequestBuilder,
        streaming: bool,
        protocol: Protocol,
    ) -> Result<Reply<T, E>> {
        let accept = if streaming {
            "text/event-stream"
        } else {
            "application/json"
        };
        let response = request
            .header(header::ACCEPT, accept)
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
        background_request_id: identity(headers, "bitrouter-request-id")?,
    })
}
