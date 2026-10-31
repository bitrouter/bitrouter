//! Bounded service SSE consumption, without reconnect or provider conversion.

use std::pin::Pin;

use bitrouter_ai::gateway::GatewayErrorEnvelope;
use bitrouter_ai::stream::{ProtocolEnd, StreamItem};
use futures_core::Stream;
use futures_util::StreamExt;
use serde::de::DeserializeOwned;

use crate::error::{Error, Result, ServiceFailure};

/// Compatibility name for the shared public event contract.
/// New consumers may import its owning type from `bitrouter_ai::stream`.
pub type ServerEvent<T> = bitrouter_ai::stream::ServerEvent<T>;

/// Dropping the stream closes local consumption; it does not prove upstream cancellation.
pub type EventStream<T> = Pin<Box<dyn Stream<Item = Result<ServerEvent<T>>> + Send>>;

/// A relay stream retains explicit termination instead of treating EOF as success.
/// Dropping it does not establish upstream cancellation or permit redispatch.
pub type RelayStream<T> = Pin<Box<dyn Stream<Item = Result<StreamItem<T>>> + Send>>;

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
    let mut stream = decode_relay(response, limit, protocol);
    Box::pin(async_stream::try_stream! {
        while let Some(item) = stream.next().await {
            match item? {
                StreamItem::Event(event) => yield event,
                StreamItem::End(_) => break,
            }
        }
    })
}

pub(crate) fn decode_relay<T: DeserializeOwned + Send + 'static>(
    response: reqwest::Response,
    limit: usize,
    protocol: Protocol,
) -> RelayStream<T> {
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
                if matches!(protocol, Protocol::Chat) && event.event != "error" && event.data == "[DONE]" {
                    terminal = true;
                    yield StreamItem::End(ProtocolEnd::ChatDone);
                    break 'body;
                }
                let data: serde_json::Value = serde_json::from_str(&event.data)
                    .map_err(|_| {
                        let error = Error::Response("SSE data is not JSON");
                        if event.event == "error" {
                            error_event(Err(error), protocol, status, &headers)
                        } else {
                            error
                        }
                    })?;
                let kind = data.get("type").and_then(serde_json::Value::as_str);
                if data.get("error").is_some() || event.event == "error" || kind == Some("error") {
                    Err(error_event(Ok(data.clone()), protocol, status, &headers))?;
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
                let end = if terminal {
                    match kind {
                        Some("message_stop") => Some(ProtocolEnd::MessagesStop),
                        Some("response.completed") => Some(ProtocolEnd::ResponsesCompleted),
                        Some("response.incomplete") => Some(ProtocolEnd::ResponsesIncomplete),
                        Some("response.failed") => Some(ProtocolEnd::ResponsesFailed),
                        _ => None,
                    }
                } else { None };
                let data = serde_json::from_value(data)
                    .map_err(|_| Error::Response("SSE data does not match the public contract"))?;
                yield StreamItem::Event(ServerEvent { event: event.event, id: event.id, data });
                if let Some(end) = end { yield StreamItem::End(end); }
                if terminal { break 'body; }
            }
        }
        if !terminal {
            decoder.finish()?;
            Err(Error::IncompleteStream)?;
        }
    })
}

fn error_event(
    data: Result<serde_json::Value>,
    protocol: Protocol,
    status: u16,
    headers: &reqwest::header::HeaderMap,
) -> Error {
    let envelope = data.map(|data| {
        // Only wrap a bare error detail, not arbitrary JSON from a named error
        // event. Raw upstream values remain supported inside an error envelope.
        if data.get("error").is_none()
            && data
                .get("message")
                .is_some_and(serde_json::Value::is_string)
        {
            serde_json::json!({"error": data})
        } else {
            data
        }
    });
    if matches!(protocol, Protocol::ManagedResponses) {
        let decoded = envelope.and_then(|envelope| {
            serde_json::from_value::<bitrouter_ai::core::protocol::ErrorEnvelope>(envelope)
                .map_err(|_| Error::Response("invalid core stream error envelope"))
        });
        let (body, body_error) = match decoded {
            Ok(body) => (Some(body), None),
            Err(error) => (None, Some(error)),
        };
        Error::CoreStream(Box::new(ServiceFailure {
            status,
            headers: headers.clone(),
            body,
            body_error,
        }))
    } else {
        let decoded = envelope.and_then(|envelope| {
            serde_json::from_value::<GatewayErrorEnvelope>(envelope)
                .map_err(|_| Error::Response("invalid stream error envelope"))
        });
        let (body, body_error) = match decoded {
            Ok(body) => (Some(body), None),
            Err(error) => (None, Some(error)),
        };
        Error::Stream(Box::new(ServiceFailure {
            status,
            headers: headers.clone(),
            body,
            body_error,
        }))
    }
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
