use bitrouter_providers::acp::types::ToolCallStatus;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};

use crate::app::{AppState, Focus};
use crate::model::{ContentBlock, EntryKind};

pub fn render(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let focused = state.focus == Focus::Feed;
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let inner_width = area.width.saturating_sub(4) as usize;
    let mut lines: Vec<Line> = Vec::new();
    let cursor = state.feed.cursor;

    if state.feed.entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "No messages yet — type a message to start (use @agent to address a specific agent)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (entry_idx, entry) in state.feed.entries.iter().enumerate() {
            let is_cursor = cursor == Some(entry_idx);
            let cursor_marker = if is_cursor && focused { "▸ " } else { "  " };
            let elapsed = entry.timestamp.elapsed();
            let time_hint = if elapsed.as_secs() < 60 {
                format!("{}s", elapsed.as_secs())
            } else {
                format!("{}m", elapsed.as_secs() / 60)
            };
            let time_span = Span::styled(
                format!("  {time_hint}"),
                Style::default().fg(Color::DarkGray),
            );

            match &entry.kind {
                EntryKind::UserMessage { text, targets } => {
                    let target_str = if targets.is_empty() {
                        String::new()
                    } else {
                        format!(" → {}", targets.join(", "))
                    };
                    lines.push(Line::from(vec![
                        Span::raw(cursor_marker),
                        Span::styled(
                            format!("You{target_str}"),
                            Style::default()
                                .fg(Color::Blue)
                                .add_modifier(Modifier::BOLD),
                        ),
                        time_span.clone(),
                    ]));
                    for raw_line in text.lines() {
                        for segment in wrap_line(raw_line, inner_width.saturating_sub(2)) {
                            lines.push(Line::from(vec![Span::raw("  "), Span::raw(segment)]));
                        }
                    }
                    lines.push(Line::raw(""));
                }

                EntryKind::AgentMessage {
                    agent_id,
                    blocks,
                    is_streaming,
                } => {
                    let color = agent_color_for(state, agent_id);
                    lines.push(Line::from(vec![
                        Span::raw(cursor_marker),
                        Span::styled(
                            agent_id.clone(),
                            Style::default().fg(color).add_modifier(Modifier::BOLD),
                        ),
                        time_span.clone(),
                    ]));

                    if entry.collapsed {
                        lines.push(Line::from(Span::styled(
                            "  [collapsed]",
                            Style::default().fg(Color::DarkGray),
                        )));
                    } else {
                        for block in blocks {
                            match block {
                                ContentBlock::Text(text) => {
                                    for raw_line in text.lines() {
                                        for segment in
                                            wrap_line(raw_line, inner_width.saturating_sub(2))
                                        {
                                            lines.push(Line::from(vec![
                                                Span::raw("  "),
                                                Span::raw(segment),
                                            ]));
                                        }
                                    }
                                }
                                ContentBlock::Other(desc) => {
                                    lines.push(Line::from(vec![
                                        Span::raw("  "),
                                        Span::styled(
                                            desc.clone(),
                                            Style::default().fg(Color::DarkGray),
                                        ),
                                    ]));
                                }
                            }
                        }
                        if *is_streaming {
                            lines.push(Line::from(vec![
                                Span::raw("  "),
                                Span::styled("▍", Style::default().fg(Color::Cyan)),
                            ]));
                        }
                    }
                    lines.push(Line::raw(""));
                }

                EntryKind::ToolCall {
                    agent_id,
                    title,
                    status,
                    ..
                } => {
                    let (icon, icon_color) = tool_status_icon(status);
                    let color = agent_color_for(state, agent_id);

                    lines.push(Line::from(vec![
                        Span::raw(cursor_marker),
                        Span::styled(format!("{agent_id} "), Style::default().fg(color)),
                        Span::styled(format!("{icon} {title}"), Style::default().fg(icon_color)),
                        if entry.collapsed {
                            Span::styled(" [+]", Style::default().fg(Color::DarkGray))
                        } else {
                            Span::raw("")
                        },
                    ]));

                    if !entry.collapsed && matches!(status, ToolCallStatus::InProgress) {
                        lines.push(Line::from(vec![
                            Span::raw("  "),
                            Span::styled("▍", Style::default().fg(Color::Cyan)),
                        ]));
                    }
                }

                EntryKind::Thinking {
                    agent_id,
                    text,
                    is_streaming,
                } => {
                    let color = agent_color_for(state, agent_id);
                    if entry.collapsed {
                        lines.push(Line::from(vec![
                            Span::raw(cursor_marker),
                            Span::styled(
                                format!("{agent_id} ⠿ thinking"),
                                Style::default().fg(color).add_modifier(Modifier::DIM),
                            ),
                            Span::styled(" [+]", Style::default().fg(Color::DarkGray)),
                        ]));
                    } else {
                        lines.push(Line::from(vec![
                            Span::raw(cursor_marker),
                            Span::styled(
                                format!("{agent_id} ⠿ thinking"),
                                Style::default().fg(color).add_modifier(Modifier::DIM),
                            ),
                        ]));
                        for raw_line in text.lines() {
                            for segment in wrap_line(raw_line, inner_width.saturating_sub(2)) {
                                lines.push(Line::from(Span::styled(
                                    format!("  {segment}"),
                                    Style::default().fg(Color::DarkGray),
                                )));
                            }
                        }
                        if *is_streaming {
                            lines.push(Line::from(vec![
                                Span::raw("  "),
                                Span::styled("▍", Style::default().fg(Color::DarkGray)),
                            ]));
                        }
                    }
                }

                EntryKind::PermissionRequest {
                    agent_id,
                    request,
                    selected,
                    resolved,
                    ..
                } => {
                    let color = agent_color_for(state, agent_id);
                    lines.push(Line::from(vec![
                        Span::raw(cursor_marker),
                        Span::styled(format!("{agent_id} "), Style::default().fg(color)),
                        Span::styled(
                            "── Permission Required ──",
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]));

                    if !request.title.is_empty() {
                        lines.push(Line::from(vec![
                            Span::raw("  "),
                            Span::styled("Tool: ", Style::default().fg(Color::Yellow)),
                            Span::raw(request.title.clone()),
                        ]));
                    }

                    if *resolved {
                        lines.push(Line::from(Span::styled(
                            "  [resolved]",
                            Style::default().fg(Color::DarkGray),
                        )));
                    } else {
                        for (i, opt) in request.options.iter().enumerate() {
                            let is_selected = i == *selected;
                            let marker = if is_selected { "▸ " } else { "  " };
                            let style = if is_selected {
                                Style::default()
                                    .fg(Color::Cyan)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(Color::White)
                            };
                            lines.push(Line::from(Span::styled(
                                format!("  {marker}{}", opt.title),
                                style,
                            )));
                        }
                        lines.push(Line::from(Span::styled(
                            "  Enter: select │ Esc: cancel",
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }

                EntryKind::SystemMessage(text) => {
                    lines.push(Line::from(vec![
                        Span::raw(cursor_marker),
                        Span::styled(format!("sys: {text}"), Style::default().fg(Color::DarkGray)),
                    ]));
                }
            }
        }
    }

    let total = lines.len();
    state.feed.total_rendered_lines = total;

    // Clamp scroll offset.
    let visible = area.height.saturating_sub(2) as usize;
    let max_scroll = total.saturating_sub(visible);
    state.feed.scroll_offset = state.feed.scroll_offset.min(max_scroll);

    // Auto-scroll to bottom when new content arrives (if already near bottom).
    // This check: if cursor is at the last entry, keep scroll at bottom.
    if state.feed.cursor.is_none()
        || state.feed.cursor == Some(state.feed.entries.len().saturating_sub(1))
    {
        state.feed.scroll_offset = max_scroll;
    }

    let offset = state.feed.scroll_offset as u16;

    let para = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title("Activity"),
        )
        .scroll((offset, 0));

    frame.render_widget(para, area);

    // Scrollbar overlay.
    if max_scroll > 0 {
        let mut scroll_state = ScrollbarState::new(max_scroll).position(state.feed.scroll_offset);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
        let sb_area = Rect::new(
            area.x + area.width.saturating_sub(1),
            area.y + 1,
            1,
            area.height.saturating_sub(2),
        );
        frame.render_stateful_widget(scrollbar, sb_area, &mut scroll_state);
    }
}

fn agent_color_for(state: &AppState, agent_id: &str) -> Color {
    state
        .agents
        .iter()
        .find(|a| a.name == agent_id)
        .map_or(Color::White, |a| a.color)
}

fn tool_status_icon(status: &ToolCallStatus) -> (&'static str, Color) {
    match status {
        ToolCallStatus::Pending => ("○", Color::DarkGray),
        ToolCallStatus::InProgress => ("⟳", Color::Yellow),
        ToolCallStatus::Completed => ("✓", Color::Green),
        ToolCallStatus::Failed => ("✗", Color::Red),
    }
}

/// Simple word-wrap at the given width.
fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 || line.is_empty() {
        return vec![line.to_string()];
    }
    let mut result = Vec::new();
    let mut current = String::new();
    for word in line.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.len() + 1 + word.len() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            result.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}
