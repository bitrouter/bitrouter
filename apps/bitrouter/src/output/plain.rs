//! Shared transcript rows written to a non-terminal sink.
//!
//! Deliberately not a second renderer: it is the same journal and the same
//! [`render`](crate::presentation::transcript) implementations, written without a backend. The
//! terminal consumer alone maps annotations to escape sequences. This serializer
//! writes only the shared text, including its original spacing and row boundaries.

use std::io::{self, Write};

use agent_client_protocol_schema::v1::SessionUpdate;

use crate::presentation::journal::Journal;
use crate::presentation::transcript::Registry;
use crate::presentation::transcript::cache::Cache;
use crate::presentation::transcript::text::{Extent, Row};

#[cfg(test)]
mod baseline;

/// The terminal a pipe does not have.
///
/// Eighty columns is the convention every tool that has had to guess has
/// guessed. Shared Markdown and code formatting use this extent. Tool output,
/// plans and literal user text retain their existing wrapping rules; there is
/// no additional whole-document wrap before serialization.
pub const PIPED: Extent = Extent {
    width: 80,
    height: 24,
};

/// A rendered line as plain text: the spans concatenated, every style dropped.
pub fn text(line: &Row) -> String {
    line.runs.iter().map(|span| span.content.as_str()).collect()
}

/// The session as a pipe receives it: the journal, rendered once per turn,
/// and written from the row after the last one written.
///
/// A pipe cannot take a row back, so nothing is emitted until the turn that
/// produces it has settled — which is also what makes in-place patching
/// arrive as one finished tool call rather than three. Shared by every path
/// that writes a session to something that is not a terminal, so they all
/// print the same document.
#[derive(Default)]
pub struct Transcript {
    journal: Journal,
    cache: Cache,
    registry: Registry,
    /// How many rows of the document have already been handed out.
    written: usize,
}

impl Transcript {
    /// Record one update. Nothing is rendered until [`Transcript::unwritten`].
    pub fn apply(&mut self, update: SessionUpdate) {
        self.journal.apply(update);
    }

    /// The rows added since the last call, rendered at [`PIPED`] width.
    pub fn unwritten(&mut self) -> Vec<Row> {
        let document = self
            .cache
            .document(&self.journal, &self.registry, PIPED, &[]);
        let fresh = document.get(self.written..).unwrap_or_default().to_vec();
        self.written = document.len();
        fresh
    }
}

/// Write rendered lines to a non-terminal sink, one per row.
///
/// `writeln!` rather than `println!`: a closed pipe makes `println!` panic,
/// and piping a session into `head` closes the pipe as a matter of course.
pub fn write(out: &mut impl Write, lines: &[Row]) -> io::Result<()> {
    for line in lines {
        writeln!(out, "{}", text(line))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::transcript::text::{Format, Run, Tone};
    use agent_client_protocol_schema::v1::{ContentBlock, ContentChunk, TextContent};

    #[test]
    fn annotations_do_not_reach_a_pipe() -> io::Result<()> {
        let row = Row::from(vec![
            Run::annotated("all callers ", Format::default().tone(Tone::Pending)),
            Run::annotated("USD 1.3200", Format::default().bold()),
        ]);
        let mut output = Vec::new();
        write(&mut output, &[row])?;
        assert_eq!(output, b"all callers USD 1.3200\n");
        Ok(())
    }

    #[test]
    fn a_closed_pipe_is_a_write_error() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let result = write(&mut Closed, &[Row::from("settled turn")]);
        assert_eq!(
            result.err().map(|error| error.kind()),
            Some(io::ErrorKind::BrokenPipe)
        );
    }
    /// A transcript hands out each row once: what a pipe already has is never
    /// written again, and what arrived since is everything it gets.
    ///
    /// The second message is keyed. An unkeyed chunk continues the open run
    /// *in place* — the journal's sticky rule — which changes a row the pipe
    /// already has and adds none; that is the case a pipe cannot show, and
    /// the reason the transcript is written once per turn rather than per
    /// chunk.
    #[test]
    fn a_transcript_hands_out_each_row_once() {
        use agent_client_protocol_schema::v1::MessageId;

        let chunk = |text: &str, key: &str| {
            let mut chunk =
                ContentChunk::new(ContentBlock::Text(TextContent::new(text.to_string())));
            chunk.message_id = Some(MessageId::from(key.to_string()));
            SessionUpdate::AgentMessageChunk(chunk)
        };
        let mut transcript = Transcript::default();
        transcript.apply(chunk("first", "m1"));
        let rows = transcript.unwritten();
        assert!(
            rows.iter().any(|row| text(row).contains("first")),
            "the first turn's rows are written: {rows:?}"
        );
        assert!(
            transcript.unwritten().is_empty(),
            "nothing new, nothing written"
        );
        transcript.apply(chunk("second", "m2"));
        let rows = transcript.unwritten();
        assert!(
            rows.iter().any(|row| text(row).contains("second")),
            "the second message's rows are written: {rows:?}"
        );
        assert!(
            rows.iter().all(|row| !text(row).contains("first")),
            "and the first's are not written again: {rows:?}"
        );
    }
}
