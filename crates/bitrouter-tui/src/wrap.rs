//! Wrapping one logical [`Line`] into the physical rows it occupies.
//!
//! # Why this exists at all
//!
//! The renderer diffs **rows**, not lines. If one line wraps to three rows,
//! arithmetic over logical-line indices is wrong by two from that point on and
//! the error compounds down the document. So the wrap has to happen before the
//! diff, and the diff never sees a logical line.
//!
//! Ratatui already does this internally and will not lend it: `mod reflow` is
//! private, and `Paragraph::line_count(width)` returns a count with no way to
//! get the rows themselves. So the wrap is ours, built from the two public
//! pieces that matter — `Line::styled_graphemes`, which clusters the text and
//! carries each grapheme's style, and `unicode-width`, which says how many
//! columns a cluster occupies.
//!
//! # Why it agrees with ratatui to the row
//!
//! This is a *word* wrapper, matching `Wrap { trim: false }` — the same breaks,
//! the same handling of whitespace at a break, the same rule that a grapheme
//! wider than the whole line is skipped. That is not imitation for its own
//! sake: it makes `Paragraph::line_count` an exact oracle, so every fixture in
//! this module's tests is checked against ratatui's own arithmetic rather than
//! against a number someone wrote down.
//!
//! The parity is inherited warts and all: an unbreakable run of double-width
//! clusters can leave a row one column wider than the terminal, because the
//! word goes onto an empty row whole. Ratatui does this too and clips at
//! render time. It is pinned by a test rather than papered over, and it is why
//! **the writer must clip a row to the width when it paints** instead of
//! trusting the wrap.

use std::collections::VecDeque;

use ratatui::style::Style;
use ratatui::text::{Line, Span, StyledGrapheme};
use unicode_width::UnicodeWidthStr as _;

/// A zero-width space breaks lines but occupies no column.
const ZWSP: &str = "\u{200b}";
/// A non-breaking space occupies a column and, as its name says, does not
/// break.
const NBSP: &str = "\u{a0}";

/// The physical rows `line` occupies at `width` columns.
///
/// Styles survive a break: a span split across two rows becomes a styled span
/// on each. A width of zero yields no rows, because nothing can be drawn in no
/// columns.
pub fn wrap(line: &Line<'_>, width: u16) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }

    let mut rows: Vec<Vec<StyledGrapheme<'_>>> = Vec::new();
    // The row being built, plus the two buffers that make this a *word* wrap:
    // a word is held back until it is known to fit, and the whitespace before
    // it is held separately so it can be dropped at a break.
    let mut row: Vec<StyledGrapheme<'_>> = Vec::new();
    let mut word: Vec<StyledGrapheme<'_>> = Vec::new();
    let mut spaces: VecDeque<StyledGrapheme<'_>> = VecDeque::new();
    let mut row_width: u16 = 0;
    let mut word_width: u16 = 0;
    let mut space_width: u16 = 0;
    let mut in_word = false;

    for grapheme in line.styled_graphemes(Style::default()) {
        let is_space = is_whitespace(grapheme.symbol);
        let symbol_width = u16::try_from(grapheme.symbol.width()).unwrap_or(u16::MAX);
        // A grapheme too wide for the whole line can never be placed. Dropping
        // it is what ratatui does, and the alternative — an infinite loop
        // looking for a row it fits on — is not a better answer.
        if symbol_width > width {
            continue;
        }

        let word_ended = in_word && is_space;
        // The word, with the whitespace in front of it, no longer fits on a
        // row of its own. Commit what is held so the break can be taken.
        let word_overflows_alone = row.is_empty()
            && word_width
                .saturating_add(space_width)
                .saturating_add(symbol_width)
                > width;
        if word_ended || word_overflows_alone {
            row.extend(spaces.drain(..));
            row.append(&mut word);
            row_width = row_width
                .saturating_add(space_width)
                .saturating_add(word_width);
            space_width = 0;
            word_width = 0;
        }

        let row_full = row_width >= width;
        let no_room_for_the_word = symbol_width > 0
            && row_width
                .saturating_add(space_width)
                .saturating_add(word_width)
                >= width;
        if row_full || no_room_for_the_word {
            let mut remaining = width.saturating_sub(row_width);
            rows.push(std::mem::take(&mut row));
            row_width = 0;
            // Whitespace that fitted on the row just closed is consumed by it
            // rather than indenting the next one — a break is where trailing
            // spaces go to die.
            while let Some(front) = spaces.front() {
                let front_width = u16::try_from(front.symbol.width()).unwrap_or(u16::MAX);
                if front_width > remaining {
                    break;
                }
                space_width = space_width.saturating_sub(front_width);
                remaining = remaining.saturating_sub(front_width);
                spaces.pop_front();
            }
            if is_space && spaces.is_empty() {
                continue;
            }
        }

        if is_space {
            space_width = space_width.saturating_add(symbol_width);
            spaces.push_back(grapheme);
        } else {
            word_width = word_width.saturating_add(symbol_width);
            word.push(grapheme);
        }
        in_word = !is_space;
    }

    // Whitespace with nothing after it still occupied a row.
    if row.is_empty() && word.is_empty() && !spaces.is_empty() {
        rows.push(Vec::new());
    }
    row.extend(spaces.drain(..));
    row.append(&mut word);
    if !row.is_empty() {
        rows.push(row);
    }
    // An empty line is one row, not none: it is a blank line in the
    // transcript, and the document has to account for its height.
    if rows.is_empty() {
        rows.push(Vec::new());
    }

    rows.iter().map(|row| to_line(row)).collect()
}

/// Ratatui's own rule, which is `pub(crate)` there: a zero-width space breaks,
/// a non-breaking space does not, everything else follows `char::is_whitespace`.
fn is_whitespace(symbol: &str) -> bool {
    symbol == ZWSP || symbol.chars().all(char::is_whitespace) && symbol != NBSP
}

/// Re-assemble graphemes into a line, coalescing each run of equal style back
/// into one span.
///
/// Per-grapheme spans would render identically and cost a `Span` per character
/// on every frame, which the diff then walks. The runs are what the input had
/// and what the output should have.
fn to_line(graphemes: &[StyledGrapheme<'_>]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut content = String::new();
    let mut current: Option<Style> = None;

    for grapheme in graphemes {
        match current {
            Some(style) if style == grapheme.style => content.push_str(grapheme.symbol),
            Some(style) => {
                spans.push(Span::styled(std::mem::take(&mut content), style));
                content.push_str(grapheme.symbol);
                current = Some(grapheme.style);
            }
            None => {
                content.push_str(grapheme.symbol);
                current = Some(grapheme.style);
            }
        }
    }
    if let Some(style) = current {
        spans.push(Span::styled(content, style));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier};
    use ratatui::widgets::{Paragraph, Wrap};

    use super::*;

    /// Ratatui's arithmetic for the same line at the same width. The workspace
    /// already enables `unstable-rendered-line-info` for exactly this.
    fn oracle(line: &Line<'static>, width: u16) -> usize {
        Paragraph::new(line.clone())
            .wrap(Wrap { trim: false })
            .line_count(width)
    }

    fn text_of(rows: &[Line<'static>]) -> String {
        rows.iter()
            .map(|row| {
                row.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn row_width(row: &Line<'static>) -> usize {
        row.spans
            .iter()
            .map(|span| span.content.as_ref().width())
            .sum()
    }

    /// The invariant that makes this module trustworthy: our row count is
    /// ratatui's row count, for every fixture at every width it will meet.
    #[test]
    fn every_fixture_wraps_to_ratatui_s_own_row_count() {
        let fixtures = [
            (
                "ascii prose",
                Line::from("the quick brown fox jumps over it"),
            ),
            (
                "one long token",
                Line::from("supercalifragilisticexpialidocious"),
            ),
            // No spaces at all, and every cluster two columns wide: the case
            // a character-counting wrap gets wrong by a factor of two.
            ("cjk", Line::from("你好世界这是一个终端渲染器的测试")),
            ("emoji", Line::from("🚀 launched 🎉 twice 🧪 over here")),
            (
                "mixed widths",
                Line::from("edit 文件.rs ✅ done — 12 lines"),
            ),
            (
                "leading whitespace",
                Line::from("    indented continuation text"),
            ),
            ("trailing whitespace", Line::from("trailing   ")),
            ("only whitespace", Line::from("     ")),
            ("empty", Line::from("")),
            (
                "styled runs",
                Line::from(vec![
                    Span::styled("tool ", Style::default().fg(Color::Cyan)),
                    Span::styled("Edit", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" src/lib.rs completed in 1.2s"),
                ]),
            ),
        ];

        for (name, line) in &fixtures {
            for width in [1_u16, 2, 3, 5, 8, 13, 20, 40, 80] {
                assert_eq!(
                    wrap(line, width).len(),
                    oracle(line, width),
                    "{name} at width {width}"
                );
            }
        }
    }

    /// Wrapping may move text between rows; it may not lose or invent any.
    #[test]
    fn wrapping_preserves_every_grapheme() {
        let line = Line::from("你好 world 🚀 and some more text to push past the width");
        let joined = text_of(&wrap(&line, 11)).replace(['\n', ' '], "");
        let original: String = "你好 world 🚀 and some more text to push past the width"
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert_eq!(joined, original);
    }

    /// Single-column text has a break wherever one is needed, so every row
    /// fits. (Wide clusters are the exception, immediately below.)
    #[test]
    fn single_width_text_never_exceeds_the_row() {
        for line in [
            Line::from("plain ascii that simply has to be broken up somewhere"),
            Line::from("supercalifragilisticexpialidocious antidisestablishmentarian"),
        ] {
            for width in [8_u16, 12, 20, 30, 40] {
                for row in wrap(&line, width) {
                    assert!(
                        row_width(&row) <= usize::from(width),
                        "row {:?} is wider than {width} columns",
                        text_of(std::slice::from_ref(&row))
                    );
                }
            }
        }
    }

    /// A known, inherited overflow, pinned so it is a documented property
    /// rather than a surprise.
    ///
    /// The word wrapper decides a word fits using the width it had *before*
    /// the next cluster, so a two-column cluster can be placed with one column
    /// left. The row then ends one column over. Ratatui's `Paragraph` does
    /// exactly this and clips at render time; our row count still agrees with
    /// it, which is what this asserts.
    ///
    /// **The writer must clip a row to the width when it paints**, and must
    /// never assume `row_width <= width` — an unclipped row would wrap in the
    /// terminal and add a physical row the diff does not know about.
    #[test]
    fn wide_clusters_can_overflow_a_row_exactly_as_ratatui_does() {
        // A whole unbroken 32-column run, on one 31-column row.
        let unbreakable = Line::from("你好世界这是一个终端渲染器的测试");
        assert_eq!(wrap(&unbreakable, 31).len(), oracle(&unbreakable, 31));
        assert_eq!(wrap(&unbreakable, 31).iter().map(row_width).max(), Some(32));

        // And the everyday case: a mixed-width line whose first row lands one
        // column over. This is not exotic — any CJK or emoji in a transcript
        // can do it.
        let mixed = Line::from("edit 文件 ✅ done — twelve lines changed");
        assert_eq!(wrap(&mixed, 8).len(), oracle(&mixed, 8));
        assert_eq!(
            text_of(&wrap(&mixed, 8)).lines().next(),
            Some("edit 文件"),
            "nine columns on an eight-column row"
        );
    }

    /// Row *count* parity is not enough on its own: two wrappers can agree on
    /// how many rows there are and disagree about where the breaks fall. So
    /// compare against what `Paragraph` actually renders, cell for cell.
    ///
    /// Ratatui clips each row to the buffer, so its rendered row is a prefix
    /// of ours wherever the overflow above applies, and equal to ours
    /// everywhere else.
    #[test]
    fn the_breaks_land_where_paragraph_puts_them() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget as _;

        for text in [
            "the quick brown fox jumps over the lazy dog",
            "你好世界这是一个终端渲染器的测试",
            "edit 文件 ✅ done — twelve lines changed",
            "🚀 launched 🎉 twice 🧪 over here",
        ] {
            for width in [7_u16, 8, 11, 16, 31] {
                let line = Line::from(text.to_string());
                let ours = wrap(&line, width);
                let height = u16::try_from(ours.len()).unwrap_or(u16::MAX);
                let area = Rect::new(0, 0, width, height);
                let mut buffer = Buffer::empty(area);
                Paragraph::new(line.clone())
                    .wrap(Wrap { trim: false })
                    .render(area, &mut buffer);

                for (y, row) in ours.iter().enumerate() {
                    let y = u16::try_from(y).unwrap_or(u16::MAX);
                    // A double-width cluster occupies two cells and the second
                    // holds a blank, so step by the cluster's width rather than
                    // reading every cell — otherwise every wide character comes
                    // back with a space glued to it.
                    let mut rendered = String::new();
                    let mut x = 0_u16;
                    while x < width {
                        let symbol = buffer[(x, y)].symbol();
                        rendered.push_str(symbol);
                        x = x.saturating_add(u16::try_from(symbol.width()).unwrap_or(1).max(1));
                    }
                    let rendered = rendered.trim_end();
                    let ours_text = text_of(std::slice::from_ref(row));
                    assert!(
                        ours_text.starts_with(rendered),
                        "row {y} at width {width} of {text:?}: ratatui rendered \
                         {rendered:?}, we produced {ours_text:?}"
                    );
                }
            }
        }
    }

    /// A span that straddles a break has to arrive on both rows still styled.
    /// Losing the style at the seam would be invisible in a row count and
    /// obvious on screen.
    #[test]
    fn style_survives_a_break() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let line = Line::from(vec![
            Span::raw("plain "),
            Span::styled("emphasised text that will not fit", bold),
        ]);
        let rows = wrap(&line, 12);
        assert!(rows.len() > 2, "the fixture must actually wrap: {rows:?}");
        for row in &rows[1..] {
            for span in &row.spans {
                assert_eq!(
                    span.style, bold,
                    "the continuation rows are all inside the styled span"
                );
            }
        }
        assert_eq!(
            rows[0].spans.first().map(|s| s.style),
            Some(Style::default())
        );
    }

    /// Adjacent graphemes of equal style come back as one span, not one span
    /// per character.
    #[test]
    fn equal_styles_coalesce_into_runs() {
        let line = Line::from(vec![
            Span::styled("aaa", Style::default().fg(Color::Red)),
            Span::styled("bbb", Style::default().fg(Color::Blue)),
        ]);
        let rows = wrap(&line, 40);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].spans.len(), 2, "two runs, not six: {:?}", rows[0]);
    }

    /// Zero columns can hold nothing, and saying so with an empty vector is
    /// what stops a caller dividing by a row height of zero.
    #[test]
    fn a_zero_width_terminal_yields_no_rows() {
        assert!(wrap(&Line::from("anything at all"), 0).is_empty());
    }

    /// A blank line is a row. The document's height depends on it.
    #[test]
    fn an_empty_line_still_occupies_one_row() {
        assert_eq!(wrap(&Line::from(""), 40).len(), 1);
    }
}
