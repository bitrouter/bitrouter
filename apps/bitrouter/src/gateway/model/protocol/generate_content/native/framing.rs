//! Bounded native SSE framing, including a checked partial-frame EOF.
//! The general eventsource stream discards unfinished fields at EOF; that must
//! not release an already held native terminal as a successful completion.
//! <https://html.spec.whatwg.org/multipage/server-sent-events.html#parsing-an-event-stream>
use crate::gateway::model::{
    error::{ModelError, Result},
    protocol::SseEvent,
};

pub(crate) struct Framing {
    limit: usize,
    size: usize,
    line: Vec<u8>,
    data: String,
    event: String,
    after_cr: bool,
    first_line: bool,
}
fn invalid() -> ModelError {
    ModelError::InvalidResponse {
        message: "invalid native SSE framing".into(),
        usage: None,
    }
}
impl Framing {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            size: 0,
            line: Vec::new(),
            data: String::new(),
            event: String::new(),
            after_cr: false,
            first_line: true,
        }
    }
    pub(crate) fn push(&mut self, byte: u8) -> Result<Option<SseEvent>> {
        if self.after_cr && byte == b'\n' {
            self.after_cr = false;
            if self.size != 0 {
                self.charge()?;
            }
            return Ok(None);
        }
        self.after_cr = byte == b'\r';
        self.charge()?;
        if !matches!(byte, b'\r' | b'\n') {
            self.line.push(byte);
            return Ok(None);
        }
        let line = std::str::from_utf8(&self.line).map_err(|_| invalid())?;
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
            self.data.pop();
            return Ok(Some(SseEvent {
                event: if event.is_empty() { None } else { Some(event) },
                data: std::mem::take(&mut self.data),
            }));
        }
        if !line.starts_with(':') {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "data" => {
                    self.data.push_str(value);
                    self.data.push('\n');
                }
                "event" => {
                    self.event.clear();
                    self.event.push_str(value);
                }
                _ => {} // IDs, reconnection hints and unknown fields grant no authority.
            }
        }
        self.line.clear();
        Ok(None)
    }
    fn charge(&mut self) -> Result<()> {
        self.size = self
            .size
            .checked_add(1)
            .filter(|size| *size <= self.limit)
            .ok_or_else(invalid)?;
        Ok(())
    }
    pub(crate) fn finish(&self) -> Result<()> {
        std::str::from_utf8(&self.line).map_err(|_| invalid())?;
        if !self.line.is_empty() || !self.data.is_empty() || !self.event.is_empty() {
            return Err(invalid());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;
    #[test]
    fn native_generate_content_sse_framing_preserves_bom_unicode_multiline_and_all_line_endings()
    -> TestResult {
        for ending in ["\n", "\r", "\r\n"] {
            let input = format!(
                "\u{feff}:keepalive{ending}{ending}event: message{ending}data: α{ending}data: β{ending}{ending}:comment{ending}"
            );
            let mut decoder = Framing::new(4096);
            let mut events = Vec::new();
            for byte in input.bytes() {
                if let Some(event) = decoder.push(byte)? {
                    events.push(event);
                }
            }
            decoder.finish()?;
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].data, "α\nβ");
            assert_eq!(events[0].event.as_deref(), Some("message"));
        }
        Ok(())
    }
    #[test]
    fn native_generate_content_sse_framing_never_hides_partial_tail_or_exceeds_limit() -> TestResult
    {
        for suffix in [
            b"data: {}".as_slice(),
            b"data: {}\n",
            b"event: message\n",
            b":partial",
            &[0xf0, 0x9f],
        ] {
            let mut decoder = Framing::new(4096);
            for byte in b"data: {}\n\n".iter().chain(suffix) {
                decoder.push(*byte)?;
            }
            assert!(decoder.finish().is_err());
        }
        let mut decoder = Framing::new(3);
        for byte in b"dat" {
            decoder.push(*byte)?;
        }
        assert!(decoder.push(b'a').is_err());
        Ok(())
    }
}
