//! Bounded responses composable with ordinary Axum handlers and middleware.

mod relay;

use ::axum::{
    body::Body,
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};
use futures_core::Stream;
use serde::Serialize;

use super::error::Error;
use crate::{
    protocol::{chat_completions::ChatChunk, messages::MessagesEvent, responses::ResponsesEvent},
    stream::StreamItem,
    wire,
};

/// A prepared JSON response or a pull-driven protocol stream.
///
/// Construction never executes work or spawns a task. Dropping a body stops
/// local consumption; it does not establish upstream cancellation or settlement.
pub struct Reply(Response);

impl Reply {
    /// Serialize a public response, including Decisions, before returning success.
    ///
    /// # Errors
    /// Returns a bounded serialization failure; no partial JSON is published.
    pub fn json<T: Serialize + ?Sized>(value: &T, limit: usize) -> Result<Self, wire::Error> {
        let bytes = wire::json(value, limit)?;
        let mut response = Response::new(Body::from(bytes));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        Ok(Self(response))
    }

    /// Relay Chat chunks, emitting `[DONE]` only after explicit ChatDone.
    pub fn chat_stream<S>(stream: S, limit: usize) -> Self
    where
        S: Stream<Item = Result<StreamItem<ChatChunk>, Error>> + Send + 'static,
    {
        Self::sse_body(relay::body(stream, relay::Protocol::Chat, limit, |event| {
            if event.extra.contains_key("error")
                || event.extra.get("type").and_then(serde_json::Value::as_str) == Some("error")
            {
                Err(Error::bad_gateway())
            } else {
                Ok(relay::Info::default())
            }
        }))
    }

    /// Relay Messages events, holding message_stop until its matching end.
    pub fn messages_stream<S>(stream: S, limit: usize) -> Self
    where
        S: Stream<Item = Result<StreamItem<MessagesEvent>, Error>> + Send + 'static,
    {
        Self::sse_body(relay::body(
            stream,
            relay::Protocol::Messages,
            limit,
            relay::messages_info,
        ))
    }

    /// Relay Responses events without conflating completed/incomplete/failed.
    pub fn responses_stream<S>(stream: S, limit: usize) -> Self
    where
        S: Stream<Item = Result<StreamItem<ResponsesEvent>, Error>> + Send + 'static,
    {
        Self::sse_body(relay::body(
            stream,
            relay::Protocol::Responses,
            limit,
            relay::responses_info,
        ))
    }

    /// Wrap already-framed bytes without interpreting completion or executing work.
    ///
    /// The host must bound and validate these bytes. This is the application
    /// integration boundary: receipt/delivery gates stay in the host's body.
    /// No terminal or keepalive is synthesized. Headers select no-store because
    /// model output can contain private content.
    pub fn sse_body(body: Body) -> Self {
        let mut response = Response::new(body);
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        Self(response)
    }
}

impl IntoResponse for Reply {
    fn into_response(self) -> Response {
        self.0
    }
}
