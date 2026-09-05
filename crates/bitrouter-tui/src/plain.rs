//! The same document, for a sink that is not a terminal.
//!
//! Deliberately not a second renderer: it is the same journal and the same
//! [`render`](crate::render) implementations, written without a backend. The
//! styles are the whole difference between the two paths — a [`Line`] is only
//! escape sequences once a backend writes it, and written this way it never
//! becomes any.

use std::io::{self, Write};

use agent_client_protocol_schema::v1::SessionUpdate;
use ratatui::layout::Size;
use ratatui::text::Line;

use crate::journal::Journal;
use crate::render::Registry;
use crate::writer::Cache;

/// The terminal a pipe does not have.
///
/// Eighty columns is the convention every tool that has had to guess has
/// guessed. It matters because the document is wrapped before it is written,
/// so a pipe still gets rows a reader can follow rather than one long line.
pub const PIPED: Size = Size {
    width: 80,
    height: 24,
};

/// A rendered line as plain text: the spans concatenated, every style dropped.
pub fn text(line: &Line<'static>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
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
    pub fn unwritten(&mut self) -> Vec<Line<'static>> {
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
pub fn write(out: &mut impl Write, lines: &[Line<'static>]) -> io::Result<()> {
    for line in lines {
        writeln!(out, "{}", text(line))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol_schema::v1::{ContentBlock, ContentChunk, TextContent};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::Span;

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

    /// Styling is dropped, not encoded: a piped transcript that carried escape
    /// sequences would corrupt the file it was redirected into.
    #[test]
    fn styles_never_reach_a_pipe() {
        let line = Line::from(vec![
            Span::styled("all callers ", Style::default().fg(Color::Yellow)),
            Span::styled("USD 1.3200", Style::default().add_modifier(Modifier::BOLD)),
        ]);
        assert_eq!(text(&line), "all callers USD 1.3200");

        let mut out = Vec::new();
        write(&mut out, std::slice::from_ref(&line)).expect("writing to a vec");
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "all callers USD 1.3200\n"
        );
    }
}
