//! An inline viewport: a few live rows pinned under the caller's prompt,
//! with everything finished pushed up into the terminal's real scrollback.
//!
//! # Why inline, and not the alternate screen
//!
//! The alternate screen is what the previous terminal UI used, and it is why
//! that UI was worse than running the agent directly. Entering it hides the
//! normal screen, so the session's output stops being *terminal* output: the
//! scrollback the user already had is gone, their terminal's search finds
//! nothing, selection and copy no longer reach the transcript, and on exit
//! the whole conversation vanishes. A renderer that takes those away has to
//! give back more than it takes, and a status row does not.
//!
//! Inline gives them all back for free. Finished content is written *above*
//! the live area with [`Terminal::insert_before`], which means it is ordinary
//! scrollback — indistinguishable from what the agent would have printed on
//! its own. The live rows are the only thing this crate owns, and when the
//! session ends they are the only thing that goes away.
//!
//! Three things follow, and each is the absence of something the old design
//! had: no alternate screen, no lifecycle save/restore, and no panic hook. If
//! this process dies mid-turn the terminal is already in a usable state,
//! because it was never taken out of one.

use std::io;

use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget, Wrap};

/// The live area's height in rows.
///
/// Small on purpose: every row here is a row of the user's terminal that
/// their shell prompt does not get back until the session ends.
pub const LIVE_ROWS: u16 = 1;

/// Enforced at compile time rather than by a test: the bound is a property of
/// the constant, so it should fail the build that violates it, not a run.
const _: () = assert!(
    LIVE_ROWS <= 2,
    "every live row is a row the user's shell does not get back"
);

/// A terminal with an inline viewport, and the two operations a session
/// needs: commit finished lines to scrollback, and redraw the live rows.
pub struct Inline<B: Backend> {
    terminal: Terminal<B>,
}

impl<B: Backend> Inline<B> {
    /// Open an inline viewport of [`LIVE_ROWS`] rows at the cursor.
    ///
    /// Does not clear, does not switch screens, and does not stash terminal
    /// state to put back later — there is nothing to put back.
    pub fn new(backend: B) -> io::Result<Self> {
        let terminal = Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Inline(LIVE_ROWS),
            },
        )?;
        Ok(Self { terminal })
    }

    /// Push finished lines into the terminal's real scrollback, above the
    /// live area.
    ///
    /// This is what makes the transcript survive: these rows belong to the
    /// terminal from the moment they are written, so they are still there
    /// after this process exits, however it exits.
    pub fn commit(&mut self, lines: &[Line<'static>]) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        // Wrapped height, not line count: a long line occupies more than one
        // row, and under-reserving would let the next commit overwrite it.
        let width = self.terminal.get_frame().area().width.max(1);
        let text = ratatui::text::Text::from(lines.to_vec());
        let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
        let height = u16::try_from(paragraph.line_count(width)).unwrap_or(u16::MAX);
        self.terminal.insert_before(height, |buf: &mut Buffer| {
            paragraph.render(
                Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
                buf,
            );
        })
    }

    /// Redraw the live rows.
    pub fn draw(&mut self, status: &Line<'static>) -> io::Result<()> {
        self.terminal.draw(|frame| {
            let paragraph = Paragraph::new(status.clone());
            frame.render_widget(paragraph, frame.area());
        })?;
        Ok(())
    }

    /// End the session: clear the live rows and leave the cursor below
    /// everything committed.
    ///
    /// Only the live area is removed. Every committed line stays exactly
    /// where it is, which is the whole point — `Ctrl-C` leaves a readable
    /// transcript rather than a blank screen.
    pub fn finish(&mut self) -> io::Result<()> {
        self.terminal.clear()?;
        // The viewport's own rows are the only thing we own; releasing them
        // is the entirety of teardown.
        let area = self.terminal.get_frame().area();
        self.terminal
            .backend_mut()
            .set_cursor_position(ratatui::layout::Position {
                x: 0,
                y: area.bottom().saturating_sub(1),
            })?;
        self.terminal.backend_mut().flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn line(text: &str) -> Line<'static> {
        Line::from(text.to_string())
    }

    /// Rendered content must reach the terminal's scrollback, not just a
    /// buffer this crate owns. `insert_before` is what does that, and it is
    /// a no-op on any viewport other than `Inline` — so this also pins that
    /// the viewport really is inline.
    #[test]
    fn committed_lines_reach_the_terminal() {
        let mut inline =
            Inline::new(TestBackend::new(40, 6)).expect("an inline viewport over a test backend");
        inline
            .commit(&[line("agent: hello"), line("agent: goodbye")])
            .expect("commit to scrollback");

        let rendered = inline.terminal.backend().to_string();
        assert!(
            rendered.contains("agent: hello"),
            "committed content must be written to the terminal: {rendered:?}"
        );
        assert!(
            rendered.contains("agent: goodbye"),
            "every committed line, not only the first: {rendered:?}"
        );
    }

    /// A line longer than the terminal is wide must reserve every row it
    /// actually occupies. Reserving one row per line would let the next
    /// commit land on top of the overflow.
    #[test]
    fn a_wrapped_line_reserves_the_rows_it_occupies() {
        let mut inline = Inline::new(TestBackend::new(20, 8)).expect("viewport");
        inline
            .commit(&[line(
                "some text that is comfortably wider than twenty columns",
            )])
            .expect("commit");
        inline.commit(&[line("SENTINEL")]).expect("second commit");

        let rendered = inline.terminal.backend().to_string();
        assert!(
            rendered.contains("SENTINEL"),
            "the later line survives: {rendered:?}"
        );
        assert!(
            rendered.contains("some text"),
            "and did not overwrite the wrapped one: {rendered:?}"
        );
    }

    /// Teardown removes the live rows and nothing else.
    #[test]
    fn finishing_keeps_the_transcript() {
        let mut inline = Inline::new(TestBackend::new(40, 6)).expect("viewport");
        inline.commit(&[line("agent: kept")]).expect("commit");
        inline.draw(&line("thinking…")).expect("draw");
        inline.finish().expect("finish");

        let rendered = inline.terminal.backend().to_string();
        assert!(
            rendered.contains("agent: kept"),
            "Ctrl-C must leave a readable transcript: {rendered:?}"
        );
        assert!(
            !rendered.contains("thinking…"),
            "the live status row is transient and must not outlive the session: {rendered:?}"
        );
    }
}
