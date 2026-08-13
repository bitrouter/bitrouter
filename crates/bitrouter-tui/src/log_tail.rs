//! What to show when a session dies badly: the end of its log, and where the
//! rest of it is.
//!
//! # Why a tail, and not a pane
//!
//! A permanent log pane costs terminal rows on every session to serve the
//! rare one that fails, and it trains the eye to ignore it. The failure case
//! is better served by showing the log only when something went wrong — at
//! which point the last few lines are usually the whole answer, and the path
//! is the escape hatch when they are not.
//!
//! The path matters as much as the lines. A tail with no path leaves the user
//! knowing something broke and unable to find out what; N lines is a guess
//! about how much context the failure needed, and naming the file is how that
//! guess stops being load-bearing.

use std::path::Path;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// How many trailing log lines to show. Enough for a panic plus its context,
/// short enough not to bury the failure it is explaining.
pub const TAIL_LINES: usize = 20;

/// Render the end of a session log, plus the path to all of it.
///
/// Takes the log's *text* rather than reading the file: this crate does not
/// own the caller's paths (see the crate docs), and a pure function is
/// testable without a filesystem.
///
/// An empty log still renders the path — "it died and wrote nothing" is
/// itself a finding, and the user still needs somewhere to look.
pub fn render(path: &Path, log: &str, max_lines: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        format!("session log: {}", path.display()),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ))];

    let all: Vec<&str> = log.lines().collect();
    let skipped = all.len().saturating_sub(max_lines);
    if skipped > 0 {
        lines.push(Line::from(Span::styled(
            format!("  … {skipped} earlier lines in the file above"),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    for line in all.into_iter().skip(skipped) {
        lines.push(Line::from(Span::styled(
            format!("  {line}"),
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The plan's done-when: on a failure, both the tail and the path appear.
    #[test]
    fn a_failure_shows_the_tail_and_names_the_log() {
        let log = "starting\nconnecting\nagent panicked: index out of bounds";
        let rendered = text(&render(
            Path::new("/home/u/.bitrouter/logs/session-1.log"),
            log,
            TAIL_LINES,
        ));

        assert!(
            rendered.contains("agent panicked: index out of bounds"),
            "the failure itself must be on screen: {rendered:?}"
        );
        assert!(
            rendered.contains("/home/u/.bitrouter/logs/session-1.log"),
            "and the path to the rest of it: {rendered:?}"
        );
    }

    /// A long log is truncated to its end — the failure is at the bottom —
    /// and says how much it dropped, so the tail length is never mistaken for
    /// the whole story.
    #[test]
    fn a_long_log_is_tailed_and_says_what_it_dropped() {
        let log: String = (1..=50)
            .map(|n| format!("line {n}\n"))
            .collect::<Vec<_>>()
            .concat();
        let rendered = text(&render(Path::new("/tmp/s.log"), &log, 5));

        assert!(rendered.contains("line 50"), "the end: {rendered:?}");
        assert!(rendered.contains("line 46"), "five of them: {rendered:?}");
        assert!(
            !rendered.contains("line 45"),
            "and no more than five: {rendered:?}"
        );
        assert!(
            rendered.contains("45 earlier lines"),
            "the user must know the tail is not the whole log: {rendered:?}"
        );
    }

    /// A log shorter than the tail renders whole, with no misleading
    /// "earlier lines" note.
    #[test]
    fn a_short_log_renders_whole() {
        let rendered = text(&render(Path::new("/tmp/s.log"), "only line", TAIL_LINES));
        assert!(rendered.contains("only line"), "{rendered:?}");
        assert!(!rendered.contains("earlier lines"), "{rendered:?}");
    }

    /// An empty log is still worth reporting: "it died writing nothing" is a
    /// finding, and the path is where the user goes next.
    #[test]
    fn an_empty_log_still_names_its_path() {
        let rendered = text(&render(Path::new("/tmp/empty.log"), "", TAIL_LINES));
        assert!(rendered.contains("/tmp/empty.log"), "{rendered:?}");
    }
}
