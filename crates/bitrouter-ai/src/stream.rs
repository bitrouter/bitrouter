//! Framework-independent service events and explicit protocol termination.

/// A decoded SSE event. An ID is transport metadata, not replay authority.
#[derive(Debug)]
pub struct ServerEvent<T> {
    /// SSE event name, with the SSE default represented as `message`.
    pub event: String,
    /// Last SSE event ID, or an empty string when absent/reset.
    pub id: String,
    /// Public protocol payload, including retained extension fields.
    pub data: T,
}

/// An observed protocol terminal, not a receipt or delivery acknowledgement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolEnd {
    /// Chat Completions `[DONE]` sentinel.
    ChatDone,
    /// Messages `message_stop` event.
    MessagesStop,
    /// Responses `response.completed` event.
    ResponsesCompleted,
    /// Responses `response.incomplete` event; not successful completion.
    ResponsesIncomplete,
    /// Responses `response.failed` event; not successful completion.
    ResponsesFailed,
}

/// Relay input retains termination that an event-only consumer may discard.
///
/// Named terminal payloads arrive as `Event`, immediately followed by their
/// matching `End`. A relay holds that payload until `End`. ChatDone has no JSON
/// payload. EOF without End is incomplete, even after a finish reason or usage.
#[derive(Debug)]
pub enum StreamItem<T> {
    /// A public payload, including named terminal payloads.
    Event(ServerEvent<T>),
    /// Explicitly observed end of this protocol stream.
    End(ProtocolEnd),
}
