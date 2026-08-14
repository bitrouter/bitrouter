//! The differential writer: paint what changed, and nothing else.
//!
//! # Two index spaces, named once
//!
//! Confusing them is the whole class of bug this module has to avoid, so:
//!
//! - `prev` and `next` are **full-document** vectors of physical rows, in
//!   **document space**. Their length is the whole rendered journal, not the
//!   screen.
//! - `viewport_top` is the document index of the first row still on screen.
//! - `anchor` is the absolute **screen** row at which document row
//!   `viewport_top` is painted. It is read once, at construction, from the
//!   cursor — the session starts under the user's prompt, not at the top of
//!   their terminal — and falls to `0` as the document scrolls past it.
//!
//! **Document row `r` paints at screen row `anchor + (r - viewport_top)`**, and
//! is on screen only while `viewport_top <= r < viewport_top + (height -
//! anchor)`.
//!
//! # Why rows, and not lines
//!
//! If one logical line wraps to three rows, arithmetic over logical indices is
//! wrong by two from that point down, and the error compounds. So [`Writer`]
//! wraps first ([`crate::wrap`]) and diffs the physical rows. A caller hands it
//! logical lines and never has to know the width.
//!
//! # The above-the-fold rule
//!
//! A change can land on a row that has already scrolled off. The paint range is
//! **clamped** to what is still on screen, so that change is simply not drawn —
//! but `prev` is still replaced wholesale. That second half is what stops the
//! writer wedging: without it, `prev` and `next` stay divergent above the fold,
//! the first change resolves there on every later frame, and nothing is ever
//! painted again.
//!
//! The alternative — what pi does — is to clear the screen *and the scrollback*
//! and reprint the document. That loses everything the terminal held which we
//! did not write: the user's shell history, other programs' output, and any of
//! our own past the emulator's cap. For a tool whose transcript is the artifact
//! and which shares a terminal with a shell, that is the wrong trade. The cost
//! is a scrolled-off tool call keeping its last-painted status forever.
//!
//! # Two places this refines §4.1 of the spec
//!
//! - **Paint and scroll interleave, a screenful at a time.** The spec has one
//!   paint step and one growth step. Run in either order that loses rows the
//!   moment a document grows by more than a screen: scrolling is what pushes
//!   rows into the terminal's scrollback, so a row not painted *before* its
//!   scroll is gone from the screen and absent from the history. So the frame
//!   loops — draw what fits, scroll by at most a screenful, draw the next —
//!   which collapses to the spec's two steps whenever the growth fits on
//!   screen, as it does on every ordinary frame.
//! - **The anchor decrements before it clamps.** The spec says growth sets
//!   `anchor` to `0`. That is right once the document is taller than the
//!   screen, but not on the way there: a session anchored at row 5 of a
//!   24-row terminal that grows to 20 rows scrolls by one, which consumes one
//!   row of blank space *above* the document and none of the document itself.
//!   Zeroing the anchor there would shift every row up by five. So the scroll
//!   is charged to the anchor first and to `viewport_top` only once the anchor
//!   is spent — which degenerates to the spec's rule the moment `anchor` is 0,
//!   as it permanently is after the first real overflow.

use std::io;
use std::io::Write as _;

use ratatui::backend::{Backend, ClearType};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Rect, Size};
use ratatui::text::Line;
use ratatui::widgets::Widget as _;
use unicode_width::UnicodeWidthStr as _;

use crate::wrap::wrap;

/// Synchronized output — DEC private mode 2026 — around a frame, so a terminal
/// presents the whole repaint at once instead of tearing through it.
///
/// Not on `Backend`, and rather than special-casing crossterm inside the
/// writer, it is one method that each backend answers in its own way.
pub trait SyncSink {
    /// Begin or end a synchronized update.
    fn synchronized(&mut self, begin: bool) -> io::Result<()>;
}

impl<W: io::Write> SyncSink for ratatui::backend::CrosstermBackend<W> {
    /// Emitted **unconditionally**, with no capability query.
    ///
    /// An unrecognised DEC private mode is ignored by every conforming
    /// terminal, so the worst case is the brief tear we already accept — and
    /// the alternative, a query round-trip, would block the frame on a reply
    /// and race the session's stdin owner for it.
    fn synchronized(&mut self, begin: bool) -> io::Result<()> {
        self.write_all(if begin {
            b"\x1b[?2026h"
        } else {
            b"\x1b[?2026l"
        })
    }
}

impl SyncSink for ratatui::backend::TestBackend {
    /// A test backend has no terminal to tear.
    fn synchronized(&mut self, _begin: bool) -> io::Result<()> {
        Ok(())
    }
}

/// Paints a document of rows into a terminal, one difference at a time.
pub struct Writer<B> {
    backend: B,
    /// The document as it was last painted — every row of it, painted or
    /// scrolled away.
    prev: Vec<Line<'static>>,
    /// Document index of the first row still on screen.
    viewport_top: usize,
    /// Screen row where `viewport_top` is painted.
    anchor: u16,
    width: u16,
    height: u16,
}

impl<B: Backend + SyncSink> Writer<B> {
    /// Open a writer over `backend`, anchored at the cursor.
    ///
    /// This is the **only** call to `get_cursor_position` in the writer's
    /// life. On a real terminal it is a DSR query that blocks on the reply,
    /// which would both stall a frame and race the session's stdin owner for
    /// the answer, so the frame path may never do it.
    pub fn new(mut backend: B) -> io::Result<Self> {
        let size = backend.size()?;
        let cursor = backend.get_cursor_position()?;
        let height = size.height.max(1);
        Ok(Self {
            backend,
            prev: Vec::new(),
            viewport_top: 0,
            anchor: cursor.y.min(height.saturating_sub(1)),
            width: size.width.max(1),
            height,
        })
    }

    /// The terminal as the writer currently understands it.
    ///
    /// Renderers need it: a diff is capped at a terminal height, and what a
    /// row costs depends on the width.
    pub fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }

    /// Paint one frame of `lines`, which are logical lines — the writer wraps
    /// them itself, because only it knows the width they will be wrapped to.
    ///
    /// A frame in which nothing changed writes nothing at all. That is not an
    /// optimisation but the contract the scheduler is built on: it may call
    /// this on every tick.
    pub fn frame(&mut self, lines: &[Line<'static>]) -> io::Result<()> {
        let size = self.backend.size()?;
        let width = size.width.max(1);
        let height = size.height.max(1);
        let next: Vec<Line<'static>> = lines.iter().flat_map(|line| wrap(line, width)).collect();

        // A resize invalidates every row position we hold: the terminal has
        // reflowed underneath us and the old `viewport_top` describes a
        // document that no longer exists at this width.
        let resized = width != self.width || height != self.height;
        if resized {
            self.width = width;
            self.height = height;
            self.anchor = 0;
            self.viewport_top = next.len().saturating_sub(usize::from(height));
        }
        // A document that shrank has rows on screen that are no longer in it.
        let shrank = next.len() < self.prev.len();

        let changed = if resized || shrank {
            // Repaint everything still on screen rather than trusting a diff
            // against positions that have moved.
            (!next.is_empty()).then(|| (self.viewport_top, next.len().saturating_sub(1)))
        } else {
            changed_range(&self.prev, &next)
        };
        let Some((first, last)) = changed else {
            return Ok(());
        };

        self.backend.synchronized(true)?;
        let result = self.paint(&next, first, last);
        // End the synchronized update even when the paint failed: leaving a
        // terminal inside one would freeze its display.
        self.backend.synchronized(false)?;
        result?;

        // Wholesale, painted rows and clipped-off rows alike. This is what
        // makes "not painted" mean *a decision not to draw* rather than a
        // divergence that resolves above the fold on every later frame.
        self.prev = next;
        self.backend.flush()
    }

    /// Leave the cursor below the document, and nothing else.
    ///
    /// Every committed row stays exactly where it is — teardown removes
    /// nothing, which is why `Ctrl-C` leaves a readable transcript rather than
    /// a blank screen.
    pub fn finish(&mut self) -> io::Result<()> {
        let below = self.prev.len();
        match self.screen_row(below) {
            Some(row) => self.position(0, row)?,
            None => {
                // The document fills the screen; make one row for the shell
                // prompt rather than letting it overwrite the last one.
                self.position(0, self.height.saturating_sub(1))?;
                self.backend.append_lines(1)?;
                self.position(0, self.height.saturating_sub(1))?;
            }
        }
        self.backend.flush()
    }

    /// Draw the changed rows, a screenful at a time, scrolling between them.
    ///
    /// **Paint, then scroll — never the other way round.** A document that
    /// grew by more than a screenful cannot be made room for in one go and
    /// then painted: scrolling is what pushes rows into the terminal's
    /// scrollback, so any row not painted *before* its scroll is a row the
    /// user never receives. It is gone from the screen and absent from the
    /// history, which is the one failure this whole design exists to avoid.
    fn paint(&mut self, next: &[Line<'static>], first: usize, last: usize) -> io::Result<()> {
        let mut start = first.max(self.viewport_top);
        loop {
            let end = last
                .min(self.visible_end())
                .min(next.len().saturating_sub(1));
            if start <= end && !next.is_empty() {
                self.draw_rows(&next[start..=end], start)?;
            }

            let overflow = next
                .len()
                .saturating_sub(self.viewport_top.saturating_add(self.capacity()));
            if overflow == 0 {
                break;
            }
            // At most a screenful per scroll: more would push rows past the
            // screen without ever having drawn them.
            self.scroll(overflow.min(self.capacity()))?;
            start = end.saturating_add(1).max(self.viewport_top);
        }

        // Rows the document no longer occupies. There is no range-clear on the
        // backend trait, and clearing to the bottom of the screen is the only
        // shape that expresses this — which is safe because everything below
        // the document's end is ours and now empty.
        if self.prev.len() > next.len()
            && let Some(row) = self.screen_row(next.len())
        {
            self.position(0, row)?;
            self.backend.clear_region(ClearType::AfterCursor)?;
        }
        Ok(())
    }

    /// Scroll the terminal up by `rows`, and re-file where the document sits.
    fn scroll(&mut self, rows: usize) -> io::Result<()> {
        let Ok(count) = u16::try_from(rows) else {
            return Ok(());
        };
        // The position is not optional: a line feed anywhere but the last row
        // moves the cursor without scrolling anything.
        self.position(0, self.height.saturating_sub(1))?;
        self.backend.append_lines(count)?;
        // The scroll eats the blank space above the document before it eats
        // the document itself.
        let from_anchor = usize::from(self.anchor).min(rows);
        self.anchor = self
            .anchor
            .saturating_sub(u16::try_from(from_anchor).unwrap_or(u16::MAX));
        self.viewport_top = self
            .viewport_top
            .saturating_add(rows.saturating_sub(from_anchor));
        Ok(())
    }

    /// The last document index currently on screen.
    fn visible_end(&self) -> usize {
        self.viewport_top
            .saturating_add(self.capacity())
            .saturating_sub(1)
    }

    /// Draw `rows`, the document rows starting at index `start`.
    fn draw_rows(&mut self, rows: &[Line<'static>], start: usize) -> io::Result<()> {
        let Some(top) = self.screen_row(start) else {
            return Ok(());
        };
        let height = u16::try_from(rows.len()).unwrap_or(u16::MAX);
        let area = Rect::new(0, 0, self.width, height);
        let mut buffer = Buffer::empty(area);
        for (offset, line) in rows.iter().enumerate() {
            let Ok(y) = u16::try_from(offset) else {
                break;
            };
            // Rendering into a buffer of exactly `width` is also the clip: the
            // wrap can leave a row one column over when an unbreakable
            // double-width cluster does not fit, and an unclipped row would
            // wrap in the terminal and add a physical row the diff knows
            // nothing about.
            line.render(Rect::new(0, y, self.width, 1), &mut buffer);
        }

        let mut updates: Vec<(u16, u16, &Cell)> = Vec::new();
        for y in 0..height {
            let mut skip = 0_usize;
            for x in 0..self.width {
                let cell = &buffer[(x, y)];
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                // A double-width cluster owns the cell after it; writing that
                // cell too would overwrite the second half of the character.
                skip = cell.symbol().width().saturating_sub(1);
                updates.push((x, top.saturating_add(y), cell));
            }
        }
        self.backend.draw(updates.into_iter())
    }

    /// How many screen rows the document may occupy.
    fn capacity(&self) -> usize {
        usize::from(self.height.saturating_sub(self.anchor))
    }

    /// Where document row `row` is painted, or `None` if it is off screen.
    ///
    /// One row past the end is a legitimate answer: that is where the cursor
    /// goes at teardown.
    fn screen_row(&self, row: usize) -> Option<u16> {
        let offset = row.checked_sub(self.viewport_top)?;
        let screen = usize::from(self.anchor).checked_add(offset)?;
        (screen < usize::from(self.height)).then_some(u16::try_from(screen).unwrap_or(u16::MAX))
    }

    fn position(&mut self, x: u16, y: u16) -> io::Result<()> {
        // Always set, never inferred. The two backends disagree about where
        // `append_lines` leaves the cursor, so anything derived from it would
        // be right on one and wrong on the other.
        self.backend.set_cursor_position(Position { x, y })
    }
}

/// The first and last document indices at which two documents differ.
///
/// `None` means they are identical, which is the common case on a tick and the
/// reason a frame can cost nothing.
fn changed_range(prev: &[Line<'static>], next: &[Line<'static>]) -> Option<(usize, usize)> {
    let len = prev.len().max(next.len());
    let first = (0..len).find(|&i| prev.get(i) != next.get(i))?;
    let last = (first..len)
        .rev()
        .find(|&i| prev.get(i) != next.get(i))
        .unwrap_or(first);
    Some((first, last))
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;

    use super::*;

    fn document(rows: &[&str]) -> Vec<Line<'static>> {
        rows.iter().map(|row| Line::from(row.to_string())).collect()
    }

    /// The rows of a buffer as text.
    ///
    /// Stepping by the cluster's width rather than reading every cell: a
    /// double-width character owns the cell after it, and that cell holds a
    /// blank, so reading cell by cell glues a space into every wide word.
    fn rows(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0_u16;
                while x < buffer.area.width {
                    let symbol = buffer[(x, y)].symbol();
                    row.push_str(symbol);
                    x = x.saturating_add(u16::try_from(symbol.width()).unwrap_or(1).max(1));
                }
                row.trim_end().to_string()
            })
            .collect()
    }

    fn screen(writer: &Writer<TestBackend>) -> Vec<String> {
        rows(writer.backend.buffer())
    }

    fn scrollback(writer: &Writer<TestBackend>) -> Vec<String> {
        rows(writer.backend.scrollback())
    }

    fn writer(width: u16, height: u16) -> Writer<TestBackend> {
        match Writer::new(TestBackend::new(width, height)) {
            Ok(writer) => writer,
            Err(_) => {
                // `TestBackend` cannot fail; returning a fresh one keeps this
                // helper total without a panic.
                Writer {
                    backend: TestBackend::new(width, height),
                    prev: Vec::new(),
                    viewport_top: 0,
                    anchor: 0,
                    width,
                    height,
                }
            }
        }
    }

    /// The base case: a document shorter than the screen lands on the screen.
    #[test]
    fn a_short_document_is_painted_where_it_belongs() -> io::Result<()> {
        let mut writer = writer(20, 6);
        writer.frame(&document(&["one", "two", "three"]))?;
        assert_eq!(screen(&writer)[..3], ["one", "two", "three"]);
        Ok(())
    }

    /// A patched row is repainted in place — the entity does not appear twice.
    /// This is the whole point of the journal above it: one tool call going
    /// pending → in progress → completed occupies one row, not three.
    #[test]
    fn a_patched_row_is_repainted_in_place() -> io::Result<()> {
        let mut writer = writer(24, 6);
        writer.frame(&document(&["◌ Edit src/lib.rs"]))?;
        writer.frame(&document(&["◍ Edit src/lib.rs"]))?;
        writer.frame(&document(&["● Edit src/lib.rs"]))?;

        let painted = screen(&writer);
        assert_eq!(painted[0], "● Edit src/lib.rs");
        assert!(
            painted[1..].iter().all(String::is_empty),
            "three status changes, one row: {painted:?}"
        );
        Ok(())
    }

    /// An unchanged frame writes nothing. The scheduler calls this on every
    /// tick, so "nothing changed" has to cost nothing.
    #[test]
    fn an_unchanged_frame_writes_nothing() -> io::Result<()> {
        let mut writer = writer(20, 6);
        let doc = document(&["steady"]);
        writer.frame(&doc)?;
        writer.backend.clear()?;
        writer.frame(&doc)?;
        assert!(
            screen(&writer).iter().all(String::is_empty),
            "a second identical frame repainted: {:?}",
            screen(&writer)
        );
        Ok(())
    }

    /// §4.3, in all three parts. A change above `viewport_top` is not painted;
    /// `prev` is still updated; and the very next frame still paints a change
    /// below the fold. The third part is what proves the writer has not
    /// wedged: without the `prev` update it would resolve its first change
    /// above the fold forever and never paint again.
    #[test]
    fn a_change_above_the_fold_is_not_painted_but_is_recorded() -> io::Result<()> {
        let mut writer = writer(20, 4);
        let rows: Vec<String> = (0..10).map(|n| format!("row {n}")).collect();
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        writer.frame(&document(&refs))?;

        // The document is 10 rows in a 4-row terminal, so rows 0..=5 have
        // scrolled off and row 0 is well above the fold.
        assert!(writer.viewport_top >= 6, "top: {}", writer.viewport_top);
        assert!(
            screen(&writer).iter().any(|row| row == "row 9"),
            "the tail is on screen: {:?}",
            screen(&writer)
        );

        let mut above = rows.clone();
        above[0] = "row 0 edited".to_string();
        let refs: Vec<&str> = above.iter().map(String::as_str).collect();
        writer.frame(&document(&refs))?;

        assert!(
            !screen(&writer)
                .iter()
                .any(|row| row.contains("row 0 edited")),
            "an above-the-fold change must not be painted: {:?}",
            screen(&writer)
        );
        assert_eq!(
            writer.prev.first().map(ToString::to_string),
            Some("row 0 edited".to_string()),
            "but it must still be recorded, or the writer wedges"
        );

        // And the next frame still works.
        let mut below = above.clone();
        below[9] = "row 9 edited".to_string();
        let refs: Vec<&str> = below.iter().map(String::as_str).collect();
        writer.frame(&document(&refs))?;
        assert!(
            screen(&writer)
                .iter()
                .any(|row| row.contains("row 9 edited")),
            "a below-the-fold change on the very next frame: {:?}",
            screen(&writer)
        );
        Ok(())
    }

    /// Nothing is erased: every row of a document taller than the terminal is
    /// either still on screen or in the terminal's scrollback, and teardown
    /// removes none of it.
    #[test]
    fn teardown_leaves_every_row_on_screen_or_in_scrollback() -> io::Result<()> {
        let mut writer = writer(24, 5);
        let rows: Vec<String> = (0..12).map(|n| format!("row {n}")).collect();
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        writer.frame(&document(&refs))?;
        writer.finish()?;

        let visible = screen(&writer);
        let history = scrollback(&writer);
        for row in &rows {
            assert!(
                visible.iter().any(|line| line == row) || history.iter().any(|line| line == row),
                "{row:?} was erased\nscreen: {visible:?}\nscrollback: {history:?}"
            );
        }
        Ok(())
    }

    /// A resize repaints the live region rather than leaving half of the old
    /// layout behind.
    #[test]
    fn a_resize_repaints_the_live_region() -> io::Result<()> {
        let mut writer = writer(40, 6);
        writer.frame(&document(&["alpha", "beta", "gamma"]))?;

        writer.backend.resize(12, 6);
        // Narrow enough that this line must wrap, which is what makes the
        // resize a re-wrap and not merely a repaint.
        writer.frame(&document(&["alpha", "beta", "gamma delta epsilon"]))?;

        let painted = screen(&writer);
        let joined = painted.join("\n");
        assert!(joined.contains("alpha"), "{painted:?}");
        assert!(joined.contains("gamma"), "{painted:?}");
        assert!(joined.contains("epsilon"), "the wrapped tail: {painted:?}");
        for row in &painted {
            assert!(
                row.chars().count() <= 12,
                "a row wider than the new terminal: {row:?}"
            );
        }
        Ok(())
    }

    /// A document that shrank must not leave its old tail on screen.
    #[test]
    fn a_shrinking_document_clears_its_tail() -> io::Result<()> {
        let mut writer = writer(20, 6);
        writer.frame(&document(&["one", "two", "three", "four"]))?;
        writer.frame(&document(&["one", "two"]))?;

        let painted = screen(&writer);
        assert_eq!(painted[..2], ["one", "two"]);
        assert!(
            !painted.iter().any(|row| row == "three" || row == "four"),
            "the old tail survived: {painted:?}"
        );
        Ok(())
    }

    /// The anchor is the cursor's row, so a session started under a shell
    /// prompt paints below it rather than over it.
    #[test]
    fn the_document_starts_at_the_cursor_not_the_top_of_the_screen() -> io::Result<()> {
        let mut backend = TestBackend::new(20, 6);
        backend.set_cursor_position(Position::new(0, 2))?;
        let mut writer = Writer::new(backend)?;
        writer.frame(&document(&["first"]))?;

        let painted = screen(&writer);
        assert!(
            painted[0].is_empty() && painted[1].is_empty(),
            "the rows above the prompt are not ours: {painted:?}"
        );
        assert_eq!(painted[2], "first");
        Ok(())
    }

    /// Growth is charged to the blank space above the document before it is
    /// charged to the document. Zeroing the anchor on the first scroll would
    /// shift every row up by however far down the screen the session started.
    #[test]
    fn growth_spends_the_anchor_before_it_scrolls_the_document() -> io::Result<()> {
        let mut backend = TestBackend::new(20, 6);
        backend.set_cursor_position(Position::new(0, 2))?;
        let mut writer = Writer::new(backend)?;
        assert_eq!(writer.anchor, 2);

        // Five rows into four rows of room: one row of overflow, and the
        // anchor has two to give.
        writer.frame(&document(&["a", "b", "c", "d", "e"]))?;
        assert_eq!(writer.anchor, 1, "the anchor absorbed the scroll");
        assert_eq!(writer.viewport_top, 0, "no document row scrolled off");
        assert_eq!(screen(&writer)[1..], ["a", "b", "c", "d", "e"]);
        Ok(())
    }

    /// A long line occupies the rows it really needs, and the row after it is
    /// not overwritten.
    #[test]
    fn a_wrapped_line_occupies_every_row_it_needs() -> io::Result<()> {
        let mut writer = writer(10, 6);
        writer.frame(&document(&["aaa bbb ccc ddd", "tail"]))?;
        let painted = screen(&writer);
        assert_eq!(painted[0], "aaa bbb");
        assert_eq!(painted[1], "ccc ddd");
        assert_eq!(painted[2], "tail", "the next line was not overwritten");
        Ok(())
    }

    /// A row wider than the terminal is clipped rather than allowed to wrap in
    /// the terminal, which would add a physical row the diff does not know
    /// about. This is the case `wrap` documents and cannot fix.
    #[test]
    fn an_overwide_row_is_clipped_to_the_width() -> io::Result<()> {
        let mut writer = writer(7, 4);
        // Eight columns of unbreakable double-width clusters at width 7.
        writer.frame(&document(&["你好世界", "after"]))?;
        let painted = screen(&writer);
        for row in &painted {
            assert!(
                row.width() <= 7,
                "a row escaped the terminal's width: {row:?}"
            );
        }
        assert_eq!(painted[1], "after", "the next row is still its own");
        Ok(())
    }

    /// The diff finds both ends of a change, including one that only appends.
    #[test]
    fn the_changed_range_covers_appends_and_edits() {
        let before = document(&["a", "b"]);
        assert_eq!(changed_range(&before, &before), None);
        assert_eq!(
            changed_range(&before, &document(&["a", "b", "c"])),
            Some((2, 2)),
            "an append changes only the appended row"
        );
        assert_eq!(changed_range(&before, &document(&["A", "b"])), Some((0, 0)));
        assert_eq!(changed_range(&before, &document(&["A", "B"])), Some((0, 1)));
        assert_eq!(
            changed_range(&before, &document(&["a"])),
            Some((1, 1)),
            "a removal is a change at the row that went away"
        );
    }
}
