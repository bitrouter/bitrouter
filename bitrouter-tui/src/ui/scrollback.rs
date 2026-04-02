use std::collections::HashMap;
use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};

use crate::app::{AppState, Focus};
use crate::model::{BackgroundAgentSummary, EntryKind, RenderContext, Renderable};

const PROMPT_PREFIX: &str = "› ";

pub fn render(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let width = area.width;
    let ctx = build_render_context(state);
    let mut lines: Vec<Line> = Vec::new();

    if state.scrollback.entries.is_empty() && state.input.is_empty() {
        // Empty state hint — vertically centered.
        let half = area.height / 2;
        for _ in 0..half {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(Span::styled(
            "  Type a message to start — use @agent to address a specific agent",
            Style::default().fg(Color::DarkGray),
        )));
    }

    // Render entries.
    for entry in &state.scrollback.entries {
        let agent_id = entry_agent_id(&entry.kind);

        // Background agent: render as summary line instead of full output.
        if let Some(aid) = agent_id
            && state.scrollback.agent_focus.is_background(aid)
        {
            let summary = build_background_summary(aid, &state.scrollback.entries);
            lines.extend(summary.render_lines(width, false, &ctx));
            continue;
        }

        lines.extend(entry.kind.render_lines(width, entry.collapsed, &ctx));
    }

    // Render inline prompt.
    render_inline_prompt(state, &mut lines);

    // Compute scroll.
    let total = lines.len();
    state.scrollback.total_rendered_lines = total;

    let visible = area.height as usize;
    let max_scroll = total.saturating_sub(visible);

    if state.scrollback.follow {
        state.scrollback.scroll_offset = max_scroll;
    } else {
        state.scrollback.scroll_offset = state.scrollback.scroll_offset.min(max_scroll);
    }

    let offset = state.scrollback.scroll_offset as u16;

    let para = Paragraph::new(lines).scroll((offset, 0));
    frame.render_widget(para, area);

    // Cursor positioning for inline input.
    if state.focus == Focus::Input {
        let prompt_start_line = total.saturating_sub(prompt_line_count(state));
        let cursor_line = prompt_start_line + state.input.cursor.0;
        let cursor_col = if state.input.cursor.0 == 0 {
            PROMPT_PREFIX.len() + state.input.cursor.1
        } else {
            // Continuation lines have 2-space indent.
            2 + state.input.cursor.1
        };

        let screen_line = cursor_line.saturating_sub(state.scrollback.scroll_offset);
        if screen_line < visible {
            frame.set_cursor_position((
                area.x + (cursor_col as u16).min(area.width.saturating_sub(1)),
                area.y + screen_line as u16,
            ));
        }
    }

    // Scrollbar.
    if max_scroll > 0 {
        let mut scroll_state =
            ScrollbarState::new(max_scroll).position(state.scrollback.scroll_offset);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
        let sb_area = Rect::new(
            area.x + area.width.saturating_sub(1),
            area.y,
            1,
            area.height,
        );
        frame.render_stateful_widget(scrollbar, sb_area, &mut scroll_state);
    }

    // Autocomplete popup.
    render_autocomplete(frame, state, area);
}

fn render_inline_prompt(state: &AppState, lines: &mut Vec<Line<'static>>) {
    let input_lines = &state.input.lines;
    let focused = state.focus == Focus::Input;

    let prompt_style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    for (i, line) in input_lines.iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                Span::styled(PROMPT_PREFIX.to_string(), prompt_style),
                Span::raw(line.clone()),
            ]));
        } else {
            lines.push(Line::from(vec![Span::raw("  "), Span::raw(line.clone())]));
        }
    }

    // If input is empty and focused, show a blank prompt line (cursor will appear).
    // The line is already rendered above with empty text.
}

fn prompt_line_count(state: &AppState) -> usize {
    state.input.lines.len()
}

fn render_autocomplete(frame: &mut Frame, state: &AppState, area: Rect) {
    let ac = match &state.autocomplete {
        Some(ac) if state.focus == Focus::Input && !ac.candidates.is_empty() => ac,
        _ => return,
    };

    let popup_height = (ac.candidates.len() as u16 + 2).min(8);
    let popup_width = 24u16.min(area.width);

    // Anchor above the prompt (bottom of scrollback area).
    let popup_y = (area.y + area.height)
        .saturating_sub(prompt_line_count(state) as u16)
        .saturating_sub(popup_height);
    let popup_x = area.x + PROMPT_PREFIX.len() as u16;
    let popup_area = Rect::new(popup_x, popup_y, popup_width, popup_height);

    let items: Vec<ListItem> = ac
        .candidates
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let style = if i == ac.selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let marker = if i == ac.selected { "▸ " } else { "  " };
            ListItem::new(Line::from(Span::styled(format!("{marker}@{name}"), style)))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title("@mention"),
    );

    frame.render_widget(Clear, popup_area);
    frame.render_widget(list, popup_area);
}

fn build_render_context(state: &AppState) -> RenderContext {
    let mut agent_colors = HashMap::new();
    for agent in &state.agents {
        agent_colors.insert(agent.name.clone(), agent.color);
    }
    RenderContext { agent_colors }
}

fn entry_agent_id(kind: &EntryKind) -> Option<&str> {
    match kind {
        EntryKind::AgentResponse(e) => Some(&e.agent_id),
        EntryKind::ToolCall(e) => Some(&e.agent_id),
        EntryKind::Thinking(e) => Some(&e.agent_id),
        EntryKind::Permission(e) => Some(&e.agent_id),
        EntryKind::UserPrompt(_) | EntryKind::System(_) => None,
    }
}

fn build_background_summary(
    agent_id: &str,
    entries: &[crate::model::ActivityEntry],
) -> BackgroundAgentSummary {
    let mut tool_call_count = 0usize;
    let mut earliest = Instant::now();

    for entry in entries {
        match &entry.kind {
            EntryKind::ToolCall(tc) if tc.agent_id == agent_id => {
                tool_call_count += 1;
                if entry.timestamp < earliest {
                    earliest = entry.timestamp;
                }
            }
            EntryKind::AgentResponse(ar) if ar.agent_id == agent_id => {
                if entry.timestamp < earliest {
                    earliest = entry.timestamp;
                }
            }
            EntryKind::Thinking(th) if th.agent_id == agent_id => {
                if entry.timestamp < earliest {
                    earliest = entry.timestamp;
                }
            }
            _ => {}
        }
    }

    BackgroundAgentSummary {
        agent_id: agent_id.to_string(),
        tool_call_count,
        elapsed_secs: earliest.elapsed().as_secs(),
    }
}
