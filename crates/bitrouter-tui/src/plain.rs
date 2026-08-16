//! The same document, for a sink that is not a terminal.
//!
//! Deliberately not a second renderer: it is the same journal and the same
//! [`render`](crate::render) implementations, written without a backend. The
//! styles are the whole difference between the two paths — a [`Line`] is only
//! escape sequences once a backend writes it, and written this way it never
//! becomes any.

use std::io::{self, Write};

use ratatui::layout::Size;
use ratatui::text::Line;

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
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::Span;

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
