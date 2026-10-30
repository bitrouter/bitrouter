//! Pull-driven framing; the caller retains all execution and completion authority.

use ::axum::body::Body;
use futures_core::Stream;
use futures_util::StreamExt;
use serde::Serialize;
use std::pin::Pin;

use super::Error;
use crate::{
    protocol::{messages::MessagesEvent, responses::ResponsesEvent},
    stream::{ProtocolEnd, StreamItem},
    wire,
};

#[derive(Clone, Copy)]
pub(super) enum Protocol {
    Chat,
    Messages,
    Responses,
}

#[derive(Default)]
pub(super) struct Info<'a> {
    kind: Option<&'a str>,
    terminal: Option<ProtocolEnd>,
    sequence: Option<u64>,
}

pub(super) fn messages_info(event: &MessagesEvent) -> Result<Info<'_>, Error> {
    if event.kind == "error"
        || ["error", "type"]
            .iter()
            .any(|key| event.extra.contains_key(*key))
    {
        return Err(Error::bad_gateway());
    }
    Ok(Info {
        kind: Some(&event.kind),
        terminal: (event.kind == "message_stop").then_some(ProtocolEnd::MessagesStop),
        sequence: None,
    })
}

pub(super) fn responses_info(event: &ResponsesEvent) -> Result<Info<'_>, Error> {
    // Public DTOs are constructible: flattened fields must not shadow the
    // fields used for termination validation when they are serialized.
    if ["error", "type", "response", "sequence_number"]
        .iter()
        .any(|key| event.extra.contains_key(*key))
        || event
            .response
            .as_ref()
            .and_then(Option::as_ref)
            .is_some_and(|response| response.extra.contains_key("status"))
    {
        return Err(Error::bad_gateway());
    }
    let terminal = match event.kind.as_str() {
        "response.completed" => Some(ProtocolEnd::ResponsesCompleted),
        "response.incomplete" => Some(ProtocolEnd::ResponsesIncomplete),
        "response.failed" => Some(ProtocolEnd::ResponsesFailed),
        "error" => return Err(Error::bad_gateway()),
        _ => None,
    };
    if terminal.is_some()
        && event
            .response
            .as_ref()
            .and_then(Option::as_ref)
            .is_none_or(|response| {
                Some(response.status.as_str()) != event.kind.strip_prefix("response.")
            })
    {
        return Err(Error::bad_gateway());
    }
    Ok(Info {
        kind: Some(&event.kind),
        terminal,
        sequence: event.sequence_number.flatten(),
    })
}

struct State<S> {
    stream: Pin<Box<S>>,
    pending: Option<(ProtocolEnd, String)>,
    sequence: u64,
    done: bool,
}

pub(super) fn body<S, T>(
    stream: S,
    protocol: Protocol,
    limit: usize,
    inspect: for<'a> fn(&'a T) -> Result<Info<'a>, Error>,
) -> Body
where
    S: Stream<Item = Result<StreamItem<T>, Error>> + Send + 'static,
    T: Serialize + Send + 'static,
{
    let state = State {
        stream: Box::pin(stream),
        pending: None,
        sequence: 0,
        done: false,
    };
    // Axum polls this stream through its body: no producer task or output queue.
    // https://docs.rs/axum/0.8.9/axum/body/struct.Body.html#method.from_stream
    Body::from_stream(futures_util::stream::unfold(
        state,
        move |mut state| async move {
            if state.done {
                return None;
            }
            loop {
                let item = state.stream.next().await;
                let result = match item {
                    Some(Ok(StreamItem::Event(event))) if state.pending.is_none() => {
                        let encoded = (|| {
                            let info = inspect(&event.data)?;
                            if event.event == "error"
                                || (event.event != "message"
                                    && info.kind.is_some_and(|kind| kind != event.event))
                            {
                                return Err(Error::bad_gateway());
                            }
                            if let Some(sequence) = info.sequence {
                                state.sequence =
                                    sequence.checked_add(1).ok_or_else(Error::bad_gateway)?;
                            }
                            let bytes =
                                wire::json(&event.data, limit).map_err(|_| Error::bad_gateway())?;
                            let data =
                                std::str::from_utf8(&bytes).map_err(|_| Error::bad_gateway())?;
                            let encoded = wire::Frame::Event {
                                event: Some(&event.event),
                                id: Some(&event.id),
                                data,
                            }
                            .encode(limit)
                            .map_err(|_| Error::bad_gateway())?;
                            Ok((info.terminal, encoded))
                        })();
                        match encoded {
                            Ok((Some(end), frame)) => {
                                state.pending = Some((end, frame));
                                continue;
                            }
                            Ok((None, frame)) => Ok(frame),
                            Err(error) => Err(error),
                        }
                    }
                    Some(Ok(StreamItem::End(end))) => {
                        state.done = true;
                        match (protocol, end, state.pending.take()) {
                            (Protocol::Chat, ProtocolEnd::ChatDone, None) => wire::Frame::Event {
                                event: None,
                                id: None,
                                data: "[DONE]",
                            }
                            .encode(limit)
                            .map_err(|_| Error::bad_gateway()),
                            (_, end, Some((expected, frame))) if end == expected => Ok(frame),
                            _ => Err(Error::bad_gateway()),
                        }
                    }
                    Some(Err(error)) => Err(error),
                    _ => Err(Error::bad_gateway()),
                };
                let result = match result {
                    Ok(frame) => Ok(frame.into_bytes()),
                    Err(error) => {
                        state.done = true;
                        error_frame(protocol, &error, state.sequence, limit)
                            .map(String::into_bytes)
                            .map_err(std::io::Error::other)
                    }
                };
                return Some((result, state));
            }
        },
    ))
}

fn error_frame(
    protocol: Protocol,
    error: &Error,
    sequence: u64,
    limit: usize,
) -> Result<String, wire::Error> {
    // Responses error event schema:
    // https://platform.openai.com/docs/api-reference/responses-streaming/error
    let (event, bytes) = match protocol {
        Protocol::Chat => (None, wire::json(&error.envelope(), limit)?),
        Protocol::Messages => (Some("error"), wire::json(&error.messages_event(), limit)?),
        Protocol::Responses => (
            Some("error"),
            wire::json(&error.responses_event(sequence), limit)?,
        ),
    };
    let data = std::str::from_utf8(&bytes).map_err(|_| wire::Error::Serialization)?;
    wire::Frame::Event {
        event,
        id: None,
        data,
    }
    .encode(limit)
}
