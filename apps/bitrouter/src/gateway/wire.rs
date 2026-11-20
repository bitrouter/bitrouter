//! Bounded JSON serialization and SSE framing, without protocol conversion.

use std::io::Write;

use serde::Serialize;

/// Default maximum encoded JSON response size.
pub const DEFAULT_JSON_LIMIT: usize = 16 * 1024 * 1024;
/// Default maximum encoded SSE frame size.
pub const DEFAULT_EVENT_LIMIT: usize = 8 * 1024 * 1024;

/// Encoding failed before a complete output was available.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The selected encoded-byte bound was exceeded.
    #[error("encoded output exceeds its byte limit")]
    Limit,
    /// A field could introduce another SSE field or invalid event ID.
    #[error("invalid SSE field")]
    InvalidField,
    /// Payload serialization failed; payload details are deliberately absent.
    #[error("payload serialization failed")]
    Serialization,
    /// Memory could not be reserved within the selected bound.
    #[error("output allocation failed")]
    Allocation,
}

struct Buffer {
    bytes: Vec<u8>,
    limit: usize,
    failure: Option<Error>,
}

impl Buffer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            failure: None,
        }
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(Error::Limit);
        }
        self.bytes
            .try_reserve(bytes.len())
            .map_err(|_| Error::Allocation)?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
}

impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self.append(bytes) {
            Ok(()) => Ok(bytes.len()),
            Err(error) => {
                self.failure = Some(error);
                Err(std::io::Error::other("bounded serialization failed"))
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Serialize before authorizing or committing a successful response.
///
/// # Errors
/// Returns a bound/allocation error or a redacted serialization failure.
pub fn json<T: Serialize + ?Sized>(value: &T, limit: usize) -> Result<Vec<u8>, Error> {
    let mut buffer = Buffer::new(limit);
    if serde_json::to_writer(&mut buffer, value).is_err() {
        return Err(buffer.failure.unwrap_or(Error::Serialization));
    }
    Ok(buffer.bytes)
}

/// Borrowed SSE framing. Data is already serialized; no payload conversion occurs.
pub enum Frame<'a> {
    /// A data event with optional event name and event ID.
    Event {
        /// Event name, excluding CR/LF and NUL.
        event: Option<&'a str>,
        /// Event ID; an empty ID explicitly resets the receiver's ID.
        id: Option<&'a str>,
        /// UTF-8 data; lines are independently prefixed with `data:`.
        data: &'a str,
    },
    /// A keepalive/comment with no execution meaning.
    Comment(&'a str),
}

impl Frame<'_> {
    /// Encode one complete frame within an explicit wire-byte bound.
    ///
    /// # Errors
    /// Rejects field injection, excess output and failed allocations.
    pub fn encode(&self, limit: usize) -> Result<String, Error> {
        // SSE line splitting and field rules:
        // https://html.spec.whatwg.org/multipage/server-sent-events.html#parsing-an-event-stream
        let mut buffer = Buffer::new(limit);
        match self {
            Self::Event { event, id, data } => {
                for (name, value) in [("event: ", event), ("id: ", id)] {
                    if let Some(value) = value {
                        if value.contains(['\r', '\n', '\0']) {
                            return Err(Error::InvalidField);
                        }
                        buffer.append(name.as_bytes())?;
                        buffer.append(value.as_bytes())?;
                        buffer.append(b"\n")?;
                    }
                }
                lines(&mut buffer, "data: ", data)?;
            }
            Self::Comment(text) => lines(&mut buffer, ":", text)?,
        }
        buffer.append(b"\n")?;
        String::from_utf8(buffer.bytes).map_err(|_| Error::InvalidField)
    }
}

fn lines(buffer: &mut Buffer, prefix: &str, text: &str) -> Result<(), Error> {
    // Split CR, LF and CRLF without allocating a normalized copy.
    let mut remaining = text;
    loop {
        let end = remaining.find(['\r', '\n']).unwrap_or(remaining.len());
        let (line, suffix) = remaining.split_at(end);
        buffer.append(prefix.as_bytes())?;
        buffer.append(line.as_bytes())?;
        buffer.append(b"\n")?;
        if suffix.is_empty() {
            return Ok(());
        }
        remaining = suffix
            .strip_prefix("\r\n")
            .or_else(|| suffix.strip_prefix('\r'))
            .or_else(|| suffix.strip_prefix('\n'))
            .ok_or(Error::InvalidField)?;
    }
}
