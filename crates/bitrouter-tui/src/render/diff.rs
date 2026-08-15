//! File edits, as hunks.
//!
//! # The defect this fixes
//!
//! ACP's `Diff` carries the **whole** `old_text` and the **whole** `new_text`,
//! not a patch. Rendering it literally — every old line prefixed `-`, then
//! every new line prefixed `+` — means a one-character edit to a 500-line file
//! commits a thousand rows to the terminal. That is defect 3, and it is not a
//! cosmetic one: those rows scroll the rest of the session above the fold,
//! where the writer can no longer revise anything.
//!
//! So the two texts are diffed and only the changed neighbourhoods are shown,
//! with three lines of context each. What is skipped is *counted*
//! (`… 480 unchanged lines`), and the whole diff is capped at a terminal
//! height.
//!
//! # Why a dependency
//!
//! `similar` computes the line diff and groups the ops into hunks with
//! context. Hunk grouping is exactly where a hand-rolled diff goes subtly
//! wrong — off-by-one context, hunks that should have merged — and subtly
//! wrong diff output is worse than a dependency. See `TUI_RENDERER_PLAN` §C.1.
//!
//! # The path is not optional
//!
//! Every diff names its file, and names it absolutely, on a row of its own.
//! Expand/collapse is deferred, so a summary that says `… 480 unchanged lines`
//! with no path is a dead end: there is nothing the reader can do with it and
//! nowhere to go. With the path they can open the file.

use std::path::Path;

use agent_client_protocol_schema::v1::Diff;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use similar::{ChangeTag, TextDiff};

use crate::render::content::{more, thousands};

/// Lines of unchanged context either side of a change.
///
/// Three is the `diff -u` default, and the reason is the same here: one line
/// is not enough to locate a change in a file you have open, and five is
/// mostly noise.
const CONTEXT: usize = 3;

/// The floor for the row cap, so a diff rendered against an implausibly short
/// terminal still shows its path, one change, and its tail.
const MIN_ROWS: usize = 4;

/// Render one file's changes, capped at `height` rows.
///
/// `height` is the terminal's, not the diff's: a single edit must never occupy
/// more than a screen, because the screen is what the reader is holding the
/// rest of the session in.
pub fn render(diff: &Diff, height: u16) -> Vec<Line<'static>> {
    let mut lines = vec![path_line(&diff.path)];
    let old = diff.old_text.as_deref().unwrap_or("");
    let text = TextDiff::from_lines(old, &diff.new_text);

    let mut body: Vec<Line<'static>> = Vec::new();
    let mut previous_end = 0_usize;
    for group in text.grouped_ops(CONTEXT) {
        let Some(first) = group.first() else {
            continue;
        };
        // Everything between the last hunk and this one is unchanged, and is
        // summarized rather than shown.
        let start = first.old_range().start;
        if start > previous_end {
            body.push(unchanged(start.saturating_sub(previous_end)));
        }
        for op in &group {
            for change in text.iter_changes(op) {
                body.push(change_line(change.tag(), change.value()));
            }
            previous_end = op.old_range().end;
        }
    }
    // And whatever is left after the last hunk.
    let old_lines = old.lines().count();
    if old_lines > previous_end {
        body.push(unchanged(old_lines.saturating_sub(previous_end)));
    }

    // The cap. One row is already spent on the path, and one more is needed
    // for the tail that explains the truncation.
    let cap = usize::from(height).max(MIN_ROWS);
    let room = cap.saturating_sub(2);
    if body.len() > room {
        let dropped = body.len().saturating_sub(room);
        body.truncate(room);
        body.push(more(dropped));
    }
    lines.extend(body);
    lines
}

/// The file, absolutely, in bold — the row a reader navigates by.
///
/// `std::path::absolute` is syntactic: it joins the working directory without
/// touching the filesystem, so this stays a pure renderer. A path that cannot
/// be made absolute is shown as the agent sent it, because a relative path
/// still beats no path.
fn path_line(path: &Path) -> Line<'static> {
    let shown = match std::path::absolute(path) {
        Ok(absolute) => absolute.display().to_string(),
        Err(_) => path.display().to_string(),
    };
    Line::from(Span::styled(
        format!("  {shown}"),
        Style::default().add_modifier(Modifier::BOLD),
    ))
}

/// One line of the diff, marked by what happened to it.
fn change_line(tag: ChangeTag, value: &str) -> Line<'static> {
    let value = value.trim_end_matches(['\n', '\r']).to_string();
    let (marker, style) = match tag {
        ChangeTag::Delete => ("-", Style::default().fg(Color::Red)),
        ChangeTag::Insert => ("+", Style::default().fg(Color::Green)),
        ChangeTag::Equal => (" ", Style::default().fg(Color::DarkGray)),
    };
    Line::from(Span::styled(format!("  {marker}{value}"), style))
}

/// The rows between two hunks, as a count.
fn unchanged(count: usize) -> Line<'static> {
    Line::from(Span::styled(
        format!("  … {} unchanged lines", thousands(count)),
        Style::default().fg(Color::DarkGray),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The path row is absolute, and what "absolute" *looks* like belongs to
    // the platform: `/tmp/all.rs` renders as `D:\tmp\all.rs` on Windows. So
    // these tests assert the file is *named* — a check that means the same
    // thing everywhere — and `the_path_is_absolute` is the one place holding
    // absoluteness itself, via `Path`, not via string shape.

    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn file(lines: usize) -> String {
        (1..=lines)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The named regression: a one-line edit to a 500-line file produces a
    /// bounded row count and names the path. Rendered literally it would be
    /// a thousand rows.
    #[test]
    fn a_one_line_edit_to_a_large_file_is_bounded_and_names_its_path() {
        let old = file(500);
        let new = old.replace("line 250", "line 250 — edited");
        let diff = Diff::new("src/lib.rs", new).old_text(old);

        let rendered = render(&diff, 40);
        assert!(
            rendered.len() <= 40,
            "a screenful at most, not a thousand rows: {}",
            rendered.len()
        );
        let text = text_of(&rendered);
        assert!(
            text.contains("lib.rs"),
            "the path is always named: {text:?}"
        );
        assert!(text.contains("-line 250\n"), "the old line: {text:?}");
        assert!(text.contains("+line 250 — edited"), "the new one: {text:?}");
        assert!(
            text.contains("line 247"),
            "three lines of context before: {text:?}"
        );
        assert!(!text.contains("line 246"), "and not a fourth: {text:?}");
        assert!(
            text.contains("line 253"),
            "three lines of context after: {text:?}"
        );
        assert!(
            text.contains("… 246 unchanged lines"),
            "what was skipped is counted, not dropped: {text:?}"
        );
    }

    /// The path is absolute, because a relative one is ambiguous the moment
    /// the reader is not in the directory the agent was.
    #[test]
    fn the_path_is_absolute() {
        let diff = Diff::new("src/lib.rs", "new").old_text("old".to_string());
        let first = text_of(&render(&diff, 40));
        let first = first.lines().next().unwrap_or_default().trim().to_string();
        assert!(
            Path::new(&first).is_absolute(),
            "a bare path leaves the reader nowhere to go: {first:?}"
        );
        // By components, not by bytes: the separator is `\` on Windows.
        assert!(Path::new(&first).ends_with("src/lib.rs"), "{first:?}");
    }

    /// Two edits far apart are two hunks, with the gap between them counted.
    #[test]
    fn separate_changes_become_separate_hunks() {
        let old = file(200);
        let new = old
            .replace("line 20\n", "line 20 — first\n")
            .replace("line 180\n", "line 180 — second\n");
        let diff = Diff::new("/tmp/two.rs", new).old_text(old);

        let text = text_of(&render(&diff, 60));
        assert!(text.contains("+line 20 — first"), "{text:?}");
        assert!(text.contains("+line 180 — second"), "{text:?}");
        assert_eq!(
            text.matches("unchanged lines").count(),
            3,
            "before, between, and after: {text:?}"
        );
    }

    /// A diff bigger than the terminal is truncated with the remainder
    /// counted — the same tail the output cap uses, because it is the same
    /// fact.
    #[test]
    fn a_diff_taller_than_the_terminal_is_capped() {
        let old = file(400);
        // Change every line: nothing to summarize, everything to show.
        let new = (1..=400)
            .map(|n| format!("changed {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let diff = Diff::new("/tmp/all.rs", new).old_text(old);

        let rendered = render(&diff, 24);
        assert_eq!(rendered.len(), 24, "exactly one terminal height");
        let text = text_of(&rendered);
        assert!(text.contains("all.rs"), "still names the file: {text:?}");
        assert!(
            text.ends_with("more lines"),
            "and says how much it is not showing: {text:?}"
        );
    }

    /// A new file has no old text at all: every line is an insertion, and the
    /// renderer must not treat the missing side as an error.
    #[test]
    fn a_created_file_renders_as_all_insertions() {
        let diff = Diff::new("/tmp/new.rs", "fn main() {}\n");
        let text = text_of(&render(&diff, 40));
        assert!(text.contains("new.rs"), "{text:?}");
        assert!(text.contains("+fn main() {}"), "{text:?}");
        assert!(!text.contains('-'), "nothing was removed: {text:?}");
    }

    /// An edit with no change at all still names the file rather than
    /// rendering an unexplained blank.
    #[test]
    fn an_unchanged_file_still_names_itself() {
        let diff = Diff::new("/tmp/same.rs", "same\n").old_text("same\n".to_string());
        let rendered = render(&diff, 40);
        let text = text_of(&rendered);
        assert!(text.contains("same.rs"), "{text:?}");
        assert!(text.contains("… 1 unchanged lines"), "{text:?}");
    }

    /// An implausibly short terminal still gets a path and a tail rather than
    /// an empty render.
    #[test]
    fn a_tiny_terminal_still_gets_the_path() {
        let old = file(100);
        let new = old.replace("line 50", "line 50 — edited");
        let diff = Diff::new("/tmp/small.rs", new).old_text(old);
        let rendered = render(&diff, 1);
        let text = text_of(&rendered);
        assert!(rendered.len() >= 2, "{text:?}");
        assert!(text.contains("small.rs"), "{text:?}");
    }
}
