//! Diff rendering: unified and structured diffs → scrollback `Line`s.
//!
//! Self-contained (no reducer or pane deps) — split out of `state`.

use crate::tui::event::DiffData;
use bitrouter_substrate::translate::ToolStatus;

/// One rendered scrollback line, tagged for styling by the UI layer.
/// (No user-prompt variant: monitors are read-only per TUI_SPEC_V3 I2 —
/// the transcript only ever shows what the agent side produced.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// Agent message text.
    Message(String),
    /// Agent thinking text.
    Thought(String),
    /// One line inside a fenced code block of an agent message; `lang` is the
    /// fence's info string (may be empty). Syntax-highlighted by the UI layer.
    Code { text: String, lang: String },
    /// A tool call: title + status.
    Tool {
        id: String,
        title: String,
        status: ToolStatus,
    },
    /// One line of a rendered file diff (from a tool call or permission).
    Diff(DiffLine),
    /// A manager-side failure surfaced in the pane (e.g. a prompt that never
    /// reached the agent). Rendered in the danger style.
    Error(String),
    /// An autonomy-tier decision the manager made on the user's behalf.
    /// Nothing auto-resolves silently — every one lands here.
    AutoResolved(String),
    /// A calm manager-side note (e.g. a turn that ended abnormally).
    Note(String),
}

/// One line of the `diff_render` treatment (TUI_SPEC §8b): header chips,
/// added/deleted/context lines, and the `⋮` gap between hunks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    /// `path  +N/-M` header.
    Header {
        path: String,
        adds: usize,
        dels: usize,
    },
    Add(String),
    Del(String),
    Ctx(String),
    /// `⋮` separator between hunks.
    Gap,
}

/// Diffs beyond this size render as a placeholder instead of line-by-line
/// (keeps a runaway rewrite from flooding the scrollback ring).
const MAX_DIFF_BYTES: usize = 200 * 1024;

/// Render a unified diff (`git diff` output) into scrollback lines with the
/// diff_render treatment: `+`/`-` rows tinted, hunk headers as gaps, file
/// headers dimmed.
pub fn unified_to_lines(text: &str) -> Vec<Line> {
    text.lines()
        .map(|l| {
            if l.starts_with("@@") {
                Line::Diff(DiffLine::Gap)
            } else if l.starts_with("+++") || l.starts_with("---") || l.starts_with("diff --git") {
                Line::Diff(DiffLine::Ctx(l.to_string()))
            } else if let Some(rest) = l.strip_prefix('+') {
                Line::Diff(DiffLine::Add(rest.to_string()))
            } else if let Some(rest) = l.strip_prefix('-') {
                Line::Diff(DiffLine::Del(rest.to_string()))
            } else {
                Line::Diff(DiffLine::Ctx(l.strip_prefix(' ').unwrap_or(l).to_string()))
            }
        })
        .collect()
}

/// Render a structured diff into scrollback lines: a `path +N/-M` header, then
/// hunks of added/deleted/context lines separated by `⋮` gaps.
pub fn diff_lines(diff: &DiffData) -> Vec<Line> {
    use similar::{ChangeTag, TextDiff};
    if diff.old.len() + diff.new.len() > MAX_DIFF_BYTES {
        return vec![
            Line::Diff(DiffLine::Header {
                path: diff.path.clone(),
                adds: 0,
                dels: 0,
            }),
            Line::Diff(DiffLine::Ctx("(diff too large to render)".to_string())),
        ];
    }
    let text_diff = TextDiff::from_lines(&diff.old, &diff.new);
    let (mut adds, mut dels) = (0usize, 0usize);
    let mut body: Vec<Line> = Vec::new();
    for (i, group) in text_diff.grouped_ops(2).iter().enumerate() {
        if i > 0 {
            body.push(Line::Diff(DiffLine::Gap));
        }
        for op in group {
            for change in text_diff.iter_changes(op) {
                let text = change
                    .value()
                    .trim_end_matches('\n')
                    .trim_end_matches('\r')
                    .to_string();
                body.push(Line::Diff(match change.tag() {
                    ChangeTag::Insert => {
                        adds += 1;
                        DiffLine::Add(text)
                    }
                    ChangeTag::Delete => {
                        dels += 1;
                        DiffLine::Del(text)
                    }
                    ChangeTag::Equal => DiffLine::Ctx(text),
                }));
            }
        }
    }
    let mut out = vec![Line::Diff(DiffLine::Header {
        path: diff.path.clone(),
        adds,
        dels,
    })];
    out.extend(body);
    out
}

/// Parse the substrate's rendered tool-call diff string
/// (`{path}\n[old]\n{old}\n[new]\n{new}`, from `translate::render_diff`) back
/// into structured form. Tolerant: returns `None` when the markers are absent.
pub fn parse_rendered_diff(s: &str) -> Option<DiffData> {
    let (path, rest) = s.split_once("\n[old]\n")?;
    let (old, new) = rest.split_once("\n[new]\n")?;
    Some(DiffData {
        path: path.to_string(),
        old: old.to_string(),
        new: new.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Diff rendering. ──

    #[test]
    fn diff_lines_render_header_chips_hunks_and_gap() {
        let old: String = (0..30).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l3\n", "L3\n").replace("l25\n", "L25\n");
        let lines = diff_lines(&DiffData {
            path: "src/x.rs".into(),
            old,
            new,
        });
        assert_eq!(
            lines[0],
            Line::Diff(DiffLine::Header {
                path: "src/x.rs".into(),
                adds: 2,
                dels: 2
            })
        );
        assert!(
            lines.contains(&Line::Diff(DiffLine::Gap)),
            "distant hunks separated by a gap: {lines:?}"
        );
        assert!(lines.contains(&Line::Diff(DiffLine::Del("l3".into()))));
        assert!(lines.contains(&Line::Diff(DiffLine::Add("L3".into()))));
    }

    #[test]
    fn oversized_diff_renders_a_placeholder() {
        let lines = diff_lines(&DiffData {
            path: "big".into(),
            old: "x".repeat(300 * 1024),
            new: String::new(),
        });
        assert_eq!(lines.len(), 2);
        assert!(matches!(
            &lines[1],
            Line::Diff(DiffLine::Ctx(t)) if t.contains("too large")
        ));
    }

    #[test]
    fn parse_rendered_diff_roundtrips_the_substrate_format() {
        let rendered = "src/x.rs\n[old]\na\nb\n[new]\na\nc";
        assert_eq!(
            parse_rendered_diff(rendered),
            Some(DiffData {
                path: "src/x.rs".into(),
                old: "a\nb".into(),
                new: "a\nc".into(),
            })
        );
        assert_eq!(parse_rendered_diff("no markers here"), None);
    }

    #[test]
    fn unified_diff_parses_into_diff_lines() {
        let lines = unified_to_lines(
            "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n ctx\n-old\n+new",
        );
        assert!(lines.contains(&Line::Diff(DiffLine::Add("new".into()))));
        assert!(lines.contains(&Line::Diff(DiffLine::Del("old".into()))));
        assert!(lines.contains(&Line::Diff(DiffLine::Gap)), "@@ → gap");
        assert!(lines.contains(&Line::Diff(DiffLine::Ctx("ctx".into()))));
    }
}
