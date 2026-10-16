//! Bounded service SSE consumption, without reconnect or provider conversion.

use std::pin::Pin;

use bitrouter_ai::gateway::GatewayErrorEnvelope;
use futures_core::Stream;
use futures_util::StreamExt;
use serde::de::DeserializeOwned;

use crate::error::{Error, Result, ServiceFailure};

/// A service event with its SSE name and last event ID. IDs do not authorize replay.
#[derive(Debug)]
pub struct ServerEvent<T> {
    pub event: String,
    pub id: String,
    pub data: T,
}

/// Dropping the stream closes local consumption; it does not prove upstream cancellation.
pub type EventStream<T> = Pin<Box<dyn Stream<Item = Result<ServerEvent<T>>> + Send>>;

#[derive(Clone, Copy)]
pub(crate) enum Protocol {
    Chat,
    Messages,
    Responses,
    ManagedResponses,
}

pub(crate) fn decode<T: DeserializeOwned + Send + 'static>(
    response: reqwest::Response,
    limit: usize,
    protocol: Protocol,
) -> EventStream<T> {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let mut chunks = response.bytes_stream();
    Box::pin(async_stream::try_stream! {
        let mut decoder = SseDecoder::new(limit);
        let mut terminal = false;
        'body: while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(Error::transport)?;
            for byte in chunk {
                let Some(event) = decoder.accept(byte)? else { continue; };
                if matches!(protocol, Protocol::Chat) && event.data == "[DONE]" {
                    terminal = true;
                    break 'body;
                }
                let data: serde_json::Value = serde_json::from_str(&event.data)
                    .map_err(|_| Error::Response("SSE data is not JSON"))?;
                let kind = data.get("type").and_then(serde_json::Value::as_str);
                if data.get("error").is_some() || event.event == "error" || kind == Some("error") {
                    let envelope = if data.get("error").is_some() { data.clone() } else { serde_json::json!({"error": data}) };
                    if matches!(protocol, Protocol::ManagedResponses) {
                        let (body, body_error) = match serde_json::from_value::<bitrouter_ai::core::protocol::ErrorEnvelope>(envelope) {
                            Ok(body) => (Some(body), None),
                            Err(_) => (None, Some(Error::Response("invalid core stream error envelope"))),
                        };
                        Err(Error::CoreStream(Box::new(ServiceFailure { status, headers: headers.clone(), body, body_error })))?;
                    } else {
                        let body = serde_json::from_value::<GatewayErrorEnvelope>(envelope)
                            .map_err(|_| Error::Response("invalid stream error envelope"))?;
                        Err(Error::Stream(Box::new(ServiceFailure { status, headers: headers.clone(), body: Some(body), body_error: None })))?;
                    }
                }
                if !matches!(protocol, Protocol::Chat) && event.event != "message" && Some(event.event.as_str()) != kind {
                    Err(Error::Response("SSE event name disagrees with its data type"))?;
                }
                terminal = match protocol {
                    Protocol::Chat => false,
                    Protocol::Messages => kind == Some("message_stop"),
                    Protocol::Responses | Protocol::ManagedResponses => matches!(kind, Some("response.completed" | "response.incomplete" | "response.failed")),
                };
                if terminal && matches!(protocol, Protocol::Responses | Protocol::ManagedResponses) {
                    let response = data.get("response").and_then(serde_json::Value::as_object);
                    let expected = kind.and_then(|kind| kind.strip_prefix("response."));
                    if response.and_then(|value| value.get("status")).and_then(serde_json::Value::as_str) != expected {
                        Err(Error::Response("terminal event is missing its matching response status"))?;
                    }
                }
                let data = serde_json::from_value(data)
                    .map_err(|_| Error::Response("SSE data does not match the public contract"))?;
                yield ServerEvent { event: event.event, id: event.id, data };
                if terminal { break 'body; }
            }
        }
        if !terminal {
            decoder.finish()?;
            Err(Error::IncompleteStream)?;
        }
    })
}

pub(crate) struct SseEvent {
    pub(crate) event: String,
    pub(crate) id: String,
    pub(crate) data: String,
}

/// SSE framing only: comments, one initial BOM, CR/LF, data concatenation and ID
/// retention. Never reconnects or interprets provider content. Both the current
/// line and accumulated event share one input-byte budget, including invalid
/// UTF-8; a malformed suffix cannot hide in a second unbounded parser buffer.
/// https://html.spec.whatwg.org/multipage/server-sent-events.html#parsing-an-event-stream
pub(crate) struct SseDecoder {
    limit: usize,
    size: usize,
    line: Vec<u8>,
    data: String,
    event: String,
    id: String,
    after_cr: bool,
    first_line: bool,
}

impl SseDecoder {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            size: 0,
            line: Vec::new(),
            data: String::new(),
            event: String::new(),
            id: String::new(),
            after_cr: false,
            first_line: true,
        }
    }

    pub(crate) fn accept(&mut self, byte: u8) -> Result<Option<SseEvent>> {
        if self.after_cr && byte == b'\n' {
            self.after_cr = false;
            // A CRLF is one line ending, including across HTTP chunks.
            if self.size != 0 {
                self.size = self.size.saturating_add(1);
            }
            if self.size > self.limit {
                return Err(Error::Limit);
            }
            return Ok(None);
        }
        self.after_cr = byte == b'\r';
        self.size = self.size.saturating_add(1);
        if self.size > self.limit {
            return Err(Error::Limit);
        }
        if !matches!(byte, b'\r' | b'\n') {
            self.line.push(byte);
            return Ok(None);
        }
        let line =
            std::str::from_utf8(&self.line).map_err(|_| Error::Response("invalid SSE UTF-8"))?;
        let line = if self.first_line {
            self.first_line = false;
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        if line.is_empty() {
            self.size = 0;
            self.line.clear();
            let event = std::mem::take(&mut self.event);
            if self.data.is_empty() {
                return Ok(None);
            }
            // Every data line appends exactly one LF; dispatch removes the last.
            self.data.pop();
            return Ok(Some(SseEvent {
                event: if event.is_empty() {
                    "message".into()
                } else {
                    event
                },
                id: self.id.clone(),
                data: std::mem::take(&mut self.data),
            }));
        }
        if !line.starts_with(':') {
            let (name, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match name {
                "data" => {
                    self.data.push_str(value);
                    self.data.push('\n');
                }
                "event" => {
                    self.event.clear();
                    self.event.push_str(value);
                }
                "id" if !value.contains('\0') => {
                    self.id.clear();
                    self.id.push_str(value);
                }
                _ => {} // Unknown fields and reconnection hints have no effect.
            }
        }
        self.line.clear();
        Ok(None)
    }

    fn finish(&self) -> Result<()> {
        // The spec does not dispatch unfinished data at EOF. Validate the last
        // bytes, then let the caller report the missing protocol terminator.
        std::str::from_utf8(&self.line).map_err(|_| Error::Response("invalid SSE UTF-8"))?;
        Ok(())
    }

    /// An attachment has no terminal sentinel, but EOF must not hide a partial
    /// event. A completed comment or retry hint carries no observation data.
    pub(crate) fn finish_attachment(&self) -> Result<()> {
        self.finish()?;
        if !self.line.is_empty() || !self.data.is_empty() || !self.event.is_empty() {
            return Err(Error::Response("incomplete harness observation frame"));
        }
        Ok(())
    }
}
