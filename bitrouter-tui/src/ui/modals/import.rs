//! Import-existing-sessions modal.

use ratatui::Frame;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::model::{ImportEntry, ImportState};
use crate::ui::layout::centered_rect;

pub fn render(frame: &mut Frame, state: &ImportState) {
    let area = centered_rect(70, 70, frame.area());
    frame.render_widget(Clear, area);

    let lines = build_lines(state);
    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(" Import Threads "),
    );
    frame.render_widget(para, area);
}

/// Build the modal's display lines. Extracted for unit-testing the
/// rendering of mixed selection / cursor states without setting up a
/// `ratatui::Frame`.
pub(super) fn build_lines(state: &ImportState) -> Vec<Line<'static>> {
    let mut out: Vec<Line> = Vec::new();
    out.push(Line::from(Span::styled(
        " Pick threads to replay via session/load. Space toggles, Enter confirms, Esc dismisses.",
        Style::default().fg(Color::DarkGray),
    )));
    out.push(Line::raw(""));

    let n_items = state
        .entries
        .iter()
        .filter(|e| matches!(e, ImportEntry::Item(_)))
        .count();
    let n_selected = state.selected.len();
    out.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{n_selected}/{n_items} selected"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "    a: select all   n: clear",
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    out.push(Line::raw(""));

    if state.entries.is_empty() {
        out.push(Line::from(Span::styled(
            "  (no on-disk sessions for this cwd)",
            Style::default().fg(Color::DarkGray),
        )));
        return out;
    }

    for (idx, entry) in state.entries.iter().enumerate() {
        match entry {
            ImportEntry::Group { agent_id, count } => {
                out.push(Line::from(vec![
                    Span::raw(""),
                    Span::styled(
                        format!(" ─── {agent_id}  ({count})"),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
            }
            ImportEntry::Item(c) => {
                let is_cursor = idx == state.cursor;
                let is_selected = state.selected.contains(&idx);
                let cursor_marker = if is_cursor { "▸ " } else { "  " };
                let checkbox = if is_selected { "[x] " } else { "[ ] " };
                let label = c
                    .title_hint
                    .clone()
                    .unwrap_or_else(|| format!("(session {id})", id = c.external_session_id));
                let style = if is_cursor {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                out.push(Line::from(vec![
                    Span::styled(cursor_marker, Style::default().fg(Color::Cyan)),
                    Span::styled(
                        checkbox.to_string(),
                        if is_selected {
                            Style::default().fg(Color::Green)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        },
                    ),
                    Span::styled(label, style),
                    Span::styled(
                        format!("  ({})", c.external_session_id),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ImportCandidate, ImportEntry};
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn cand(agent: &str, id: &str, title: Option<&str>) -> ImportCandidate {
        ImportCandidate {
            agent_id: agent.to_string(),
            external_session_id: id.to_string(),
            title_hint: title.map(|t| t.to_string()),
            last_active_at: 0,
            source_path: PathBuf::from(format!("/tmp/{id}.jsonl")),
        }
    }

    fn state_with(entries: Vec<ImportEntry>, cursor: usize, selected: &[usize]) -> ImportState {
        ImportState {
            entries,
            cursor,
            selected: selected.iter().copied().collect::<HashSet<_>>(),
        }
    }

    fn join(line: &Line) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn empty_state_renders_placeholder() {
        let state = state_with(Vec::new(), 0, &[]);
        let lines = build_lines(&state);
        let joined: String = lines.iter().map(join).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("(no on-disk sessions"));
    }

    #[test]
    fn renders_group_header_and_items_with_cursor_and_check() {
        let entries = vec![
            ImportEntry::Group {
                agent_id: "claude-code".to_string(),
                count: 2,
            },
            ImportEntry::Item(cand("claude-code", "abc-123", Some("refactor router"))),
            ImportEntry::Item(cand("claude-code", "def-456", None)),
        ];
        // Cursor on the first item, selection on the second.
        let state = state_with(entries, 1, &[2]);
        let lines = build_lines(&state);
        let joined: String = lines.iter().map(join).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("claude-code"));
        // Cursor marker on item 1.
        assert!(joined.contains("▸ "));
        // Selected box on item 2.
        assert!(joined.contains("[x] "));
        // Title hint shown for the first.
        assert!(joined.contains("refactor router"));
        // Fallback label for the second (no title).
        assert!(joined.contains("(session def-456)"));
    }

    #[test]
    fn header_row_shows_selection_counter() {
        let entries = vec![
            ImportEntry::Group {
                agent_id: "claude-code".to_string(),
                count: 1,
            },
            ImportEntry::Item(cand("claude-code", "abc-123", Some("foo"))),
        ];
        let state = state_with(entries, 1, &[1]);
        let lines = build_lines(&state);
        let joined: String = lines.iter().map(join).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("1/1 selected"));
    }
}
