//! SSE framing and caller-selected terminal error presentation.

pub mod collect;

/// Public error presentation selected by the caller before encoding.
///
/// Codecs preserve these fields; they do not choose HTTP policy or sanitize
/// diagnostics. A server must supply a message that is safe for its client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamError {
    /// Safe public message selected by the caller.
    pub message: String,
    /// Caller-selected error category.
    pub error_type: String,
    /// Caller-selected stable error code.
    pub code: String,
    /// Caller-selected public status for protocols that include it in SSE.
    pub status: u16,
}

/// An outbound Server-Sent-Events frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseFrame {
    /// A data event, optionally named.
    Event {
        /// The `event:` field, if any.
        event: Option<String>,
        /// The `data:` payload (already serialized).
        data: String,
    },
    /// An SSE comment (`:text`). Used for keepalives — every supported protocol
    /// ignores comments.
    Comment(String),
}

impl SseFrame {
    /// Render the frame to its on-wire byte form.
    pub fn to_wire(&self) -> String {
        match self {
            SseFrame::Event { event, data } => match event {
                Some(name) => format!("event: {name}\ndata: {data}\n\n"),
                None => format!("data: {data}\n\n"),
            },
            SseFrame::Comment(text) => format!(":{text}\n\n"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SseFrame;

    #[test]
    fn sse_frame_wire_format() {
        assert_eq!(
            SseFrame::Event {
                event: None,
                data: "{}".to_string()
            }
            .to_wire(),
            "data: {}\n\n"
        );
        assert_eq!(
            SseFrame::Event {
                event: Some("message".to_string()),
                data: "x".to_string()
            }
            .to_wire(),
            "event: message\ndata: x\n\n"
        );
        assert_eq!(
            SseFrame::Comment("keepalive".to_string()).to_wire(),
            ":keepalive\n\n"
        );
    }
}
