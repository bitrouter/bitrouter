use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};

use crate::app::{AppState, InputMode};
use crate::model::{AgentStatus, RenderContext, Renderable};

const PROMPT_PREFIX: &str = "› ";

pub fn render(frame: &mut Frame, state: &mut AppState, area: Rect) {
    // Agent mode: render inline agent list instead of scrollback.
    if state.mode == InputMode::Agent {
        render_agent_list(frame, state, area);
        return;
    }

    let width = area.width;
    let ctx = build_render_context(state);

    let has_scrollback = state
        .active_scrollback()
        .is_some_and(|sb| !sb.entries.is_empty());

    // Empty-state hints (rendered directly, no caching needed).
    let mut empty_lines: Vec<Line> = Vec::new();
    if !has_scrollback && state.input.is_empty() && state.tabs.is_empty() {
        let half = area.height / 2;
        for _ in 0..half {
            empty_lines.push(Line::raw(""));
        }
        empty_lines.push(Line::from(Span::styled(
            "  Type a message to start — use @agent to address a specific agent",
            Style::default().fg(Color::DarkGray),
        )));
        empty_lines.push(Line::from(Span::styled(
            "  Press Alt+A to connect an agent",
            Style::default().fg(Color::DarkGray),
        )));
    } else if !has_scrollback && state.input.is_empty() {
        let half = area.height / 2;
        for _ in 0..half {
            empty_lines.push(Line::raw(""));
        }
        let agent_name = state.active_agent_name().unwrap_or("agent");
        empty_lines.push(Line::from(Span::styled(
            format!("  Connected to {agent_name} — type a message to start"),
            Style::default().fg(Color::DarkGray),
        )));
    }

    // Fill line-count cache for all entries.
    if let Some(sb) = state.active_scrollback_mut() {
        // Invalidate all on width change.
        if sb.cached_width != width {
            sb.invalidate_all();
            sb.cached_width = width;
        }

        // Fill uncached slots.
        for i in 0..sb.entries.len() {
            if sb.line_counts.get(i).copied().flatten().is_none() {
                let count = sb.entries[i]
                    .kind
                    .render_lines(width, sb.entries[i].collapsed, &ctx)
                    .len();
                if i < sb.line_counts.len() {
                    sb.line_counts[i] = Some(count);
                }
            }
        }

        let total_entry_lines = sb.rebuild_offsets();

        // Prompt lines.
        let prompt_lines = prompt_line_count(state);
        let total = empty_lines.len() + total_entry_lines + prompt_lines;
        let visible = area.height as usize;

        // Update total_rendered_lines and clamp scroll.
        if let Some(sb) = state.active_scrollback_mut() {
            sb.total_rendered_lines = total;
            let max_scroll = total.saturating_sub(visible);
            if sb.follow {
                sb.scroll_offset = max_scroll;
            } else {
                sb.scroll_offset = sb.scroll_offset.min(max_scroll);
            }
        }
    }

    // Now build the visible lines using viewport math.
    let visible = area.height as usize;
    let scroll_offset = state
        .active_scrollback()
        .map(|sb| sb.scroll_offset)
        .unwrap_or(0);

    let mut lines: Vec<Line> = Vec::new();

    // Figure out what's visible: empty_lines first, then entries, then prompt.
    let empty_count = empty_lines.len();
    let entry_total = state
        .active_scrollback()
        .map(|sb| sb.line_offsets.last().copied().unwrap_or(0))
        .unwrap_or(0);

    // Viewport range in the global line space.
    let vp_start = scroll_offset;
    let vp_end = scroll_offset + visible;

    // 1. Empty-state lines (if visible).
    if !empty_lines.is_empty() && vp_start < empty_count {
        let start = vp_start;
        let end = vp_end.min(empty_count);
        lines.extend(empty_lines.into_iter().skip(start).take(end - start));
    }

    // 2. Entry lines (viewport-aware).
    if entry_total > 0 {
        let entry_global_start = empty_count;
        // Only render entries overlapping the viewport.
        if vp_end > entry_global_start && vp_start < entry_global_start + entry_total {
            let local_vp_start = vp_start.saturating_sub(entry_global_start);
            let local_vp_end = (vp_end - entry_global_start).min(entry_total);

            if let Some(sb) = state.active_scrollback() {
                let (first, last) =
                    visible_entry_range(&sb.line_offsets, local_vp_start, local_vp_end);
                let scroll_cursor = sb.scroll_cursor;

                for entry_idx in first..last {
                    if let Some(entry) = sb.entries.get(entry_idx) {
                        let mut entry_lines = entry.kind.render_lines(width, entry.collapsed, &ctx);

                        // Apply scroll cursor highlight.
                        if scroll_cursor == Some(entry_idx) {
                            apply_cursor_highlight(&mut entry_lines);
                        }

                        let entry_start = sb.line_offsets.get(entry_idx).copied().unwrap_or(0);

                        // Clip entry lines to viewport.
                        let skip = local_vp_start.saturating_sub(entry_start);
                        let take = local_vp_end.saturating_sub(entry_start.max(local_vp_start));
                        lines.extend(entry_lines.into_iter().skip(skip).take(take));
                    }
                }
            }
        }
    }

    // 3. Inline prompt lines.
    let prompt_global_start = empty_count + entry_total;
    if vp_end > prompt_global_start {
        let mut prompt_lines_vec: Vec<Line> = Vec::new();
        render_inline_prompt(state, &mut prompt_lines_vec);
        let local_start = vp_start.saturating_sub(prompt_global_start);
        let local_end = (vp_end - prompt_global_start).min(prompt_lines_vec.len());
        if local_start < local_end {
            lines.extend(
                prompt_lines_vec
                    .into_iter()
                    .skip(local_start)
                    .take(local_end - local_start),
            );
        }
    }

    let para = Paragraph::new(lines);
    frame.render_widget(para, area);

    // Cursor positioning for inline input.
    if state.mode == InputMode::Normal {
        let total = empty_count + entry_total + prompt_line_count(state);
        let prompt_start_line = total.saturating_sub(prompt_line_count(state));
        let cursor_line = prompt_start_line + state.input.cursor.0;
        let cursor_col = if state.input.cursor.0 == 0 {
            PROMPT_PREFIX.len() + state.input.cursor.1
        } else {
            2 + state.input.cursor.1
        };

        let screen_line = cursor_line.saturating_sub(scroll_offset);
        if screen_line < visible {
            frame.set_cursor_position((
                area.x + (cursor_col as u16).min(area.width.saturating_sub(1)),
                area.y + screen_line as u16,
            ));
        }
    }

    // Scrollbar.
    let total = empty_count + entry_total + prompt_line_count(state);
    let max_scroll = total.saturating_sub(visible);
    if max_scroll > 0 {
        let mut scroll_state = ScrollbarState::new(max_scroll).position(scroll_offset);
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

/// Returns (first_entry_idx, last_entry_idx_exclusive) that overlap the viewport.
fn visible_entry_range(line_offsets: &[usize], vp_start: usize, vp_end: usize) -> (usize, usize) {
    if line_offsets.len() < 2 {
        return (0, 0);
    }
    let entry_count = line_offsets.len() - 1;

    // First visible entry: last entry whose offset <= vp_start.
    let first = line_offsets
        .partition_point(|&o| o <= vp_start)
        .saturating_sub(1)
        .min(entry_count.saturating_sub(1));

    // Last visible entry: first entry whose offset >= vp_end.
    let last = line_offsets
        .partition_point(|&o| o < vp_end)
        .min(entry_count);

    (first, last)
}

/// Apply a visual highlight to the first line of the cursor entry.
fn apply_cursor_highlight(lines: &mut [Line<'static>]) {
    if let Some(first_line) = lines.first_mut() {
        let mut new_spans = vec![Span::styled(
            "▸ ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )];
        new_spans.append(&mut first_line.spans);
        first_line.spans = new_spans;
    }
}

fn render_inline_prompt(state: &AppState, lines: &mut Vec<Line<'static>>) {
    let input_lines = &state.input.lines;
    let focused = state.mode == InputMode::Normal;

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
}

fn prompt_line_count(state: &AppState) -> usize {
    state.input.lines.len()
}

fn render_autocomplete(frame: &mut Frame, state: &AppState, area: Rect) {
    let ac = match &state.autocomplete {
        Some(ac) if state.mode == InputMode::Normal && !ac.candidates.is_empty() => ac,
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

fn render_agent_list(frame: &mut Frame, state: &AppState, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "  Agents",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        "  ─────────────────────────────────────────────",
        Style::default().fg(Color::DarkGray),
    )));

    // Partition agents into tiers for display (keeping original indices for selection).
    let connected: Vec<usize> = state
        .agents
        .iter()
        .enumerate()
        .filter(|(_, a)| {
            matches!(
                a.status,
                AgentStatus::Connected | AgentStatus::Busy | AgentStatus::Connecting
            )
        })
        .map(|(i, _)| i)
        .collect();

    let installed: Vec<usize> = state
        .agents
        .iter()
        .enumerate()
        .filter(|(_, a)| matches!(a.status, AgentStatus::Idle | AgentStatus::Error(_)))
        .map(|(i, _)| i)
        .collect();

    let available: Vec<usize> = state
        .agents
        .iter()
        .enumerate()
        .filter(|(_, a)| {
            matches!(
                a.status,
                AgentStatus::Available | AgentStatus::Installing { .. }
            )
        })
        .map(|(i, _)| i)
        .collect();

    if !connected.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "  CONNECTED",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )));
        for &i in &connected {
            render_agent_row(&mut lines, state, i);
        }
    }

    if !installed.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "  INSTALLED",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )));
        for &i in &installed {
            render_agent_row(&mut lines, state, i);
        }
    }

    if !available.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "  AVAILABLE",
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        )));
        for &i in &available {
            render_agent_row(&mut lines, state, i);
        }
    }

    if state.agents.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No agents discovered. Ensure an ACP agent is on PATH.",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let para = Paragraph::new(lines);
    frame.render_widget(para, area);
}

fn render_agent_row(lines: &mut Vec<Line>, state: &AppState, i: usize) {
    let agent = &state.agents[i];
    let is_selected = i == state.agent_list_selected;
    let marker = if is_selected { "▸" } else { " " };

    let (status_str, status_color) = match &agent.status {
        AgentStatus::Idle => ("disconnected", Color::DarkGray),
        AgentStatus::Available => ("available", Color::Blue),
        AgentStatus::Installing { .. } => ("installing", Color::Cyan),
        AgentStatus::Connecting => ("connecting", Color::Cyan),
        AgentStatus::Connected => ("connected", Color::Green),
        AgentStatus::Busy => ("busy", Color::Yellow),
        AgentStatus::Error(_) => ("error", Color::Red),
    };

    // Build a more descriptive status for Installing.
    let status_display = if let AgentStatus::Installing { percent } = &agent.status {
        format!("installing {percent}%")
    } else {
        status_str.to_string()
    };

    let session_str = agent
        .session_id
        .as_ref()
        .map(|s| {
            if s.len() > 12 {
                format!("session: {}…", &s[..12])
            } else {
                format!("session: {s}")
            }
        })
        .unwrap_or_default();

    let has_tab = state.tabs.iter().any(|t| t.agent_name == agent.name);
    let tab_indicator = if has_tab { " [tab]" } else { "" };

    let row_style = if is_selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };

    lines.push(Line::from(vec![
        Span::styled(format!("  {marker} "), row_style),
        Span::styled(format!("{:<14}", agent.name), row_style.fg(agent.color)),
        Span::styled(
            format!(" {:<16}", status_display),
            row_style.fg(status_color),
        ),
        Span::styled(format!(" {session_str}"), row_style.fg(Color::DarkGray)),
        Span::styled(tab_indicator, row_style.fg(Color::Cyan)),
    ]));
}

fn build_render_context(state: &AppState) -> RenderContext {
    let mut agent_colors = HashMap::new();
    for agent in &state.agents {
        agent_colors.insert(agent.name.clone(), agent.color);
    }
    RenderContext { agent_colors }
}
