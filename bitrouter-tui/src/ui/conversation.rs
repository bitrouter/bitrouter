use agent_client_protocol as acp;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};

use crate::app::{AppState, Focus};
use crate::model::{RenderedBlock, RenderedRole};

pub fn render(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let focused = state.focus == Focus::Conversation;
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let inner_width = area.width.saturating_sub(4) as usize;
    let mut lines: Vec<Line> = Vec::new();

    let title = match &state.conversation.agent_name {
        Some(name) => format!("Conversation — {name}"),
        None => "Conversation".to_string(),
    };

    if state.conversation.messages.is_empty() {
        lines.push(Line::from(Span::styled(
            "No active session — type a message to connect to an agent",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for msg in &state.conversation.messages {
            let (prefix, prefix_style) = match msg.role {
                RenderedRole::User => (
                    "You  ",
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                ),
                RenderedRole::Agent => (
                    "Agent",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                RenderedRole::System => ("Sys  ", Style::default().fg(Color::DarkGray)),
            };

            let mut first = true;
            for block in &msg.blocks {
                match block {
                    RenderedBlock::Text(text) => {
                        for raw_line in text.lines() {
                            let wrapped = wrap_line(raw_line, inner_width);
                            for segment in wrapped {
                                if first {
                                    lines.push(Line::from(vec![
                                        Span::styled(format!("{prefix} │ "), prefix_style),
                                        Span::raw(segment),
                                    ]));
                                    first = false;
                                } else {
                                    lines.push(Line::from(vec![
                                        Span::raw("      │ "),
                                        Span::raw(segment),
                                    ]));
                                }
                            }
                        }
                    }
                    RenderedBlock::ToolCall {
                        title: tool_title,
                        status,
                        ..
                    } => {
                        let (icon, color) = match status {
                            acp::ToolCallStatus::Pending => ("○", Color::DarkGray),
                            acp::ToolCallStatus::InProgress => ("⟳", Color::Yellow),
                            acp::ToolCallStatus::Completed => ("✓", Color::Green),
                            acp::ToolCallStatus::Failed => ("✗", Color::Red),
                            _ => ("?", Color::DarkGray),
                        };
                        let prefix_span = if first {
                            first = false;
                            Span::styled(format!("{prefix} │ "), prefix_style)
                        } else {
                            Span::raw("      │ ")
                        };
                        lines.push(Line::from(vec![
                            prefix_span,
                            Span::styled(
                                format!("{icon} {tool_title}"),
                                Style::default().fg(color),
                            ),
                        ]));
                    }
                }
            }

            // Streaming indicator
            if msg.is_streaming {
                lines.push(Line::from(vec![
                    Span::raw("      │ "),
                    Span::styled("▍", Style::default().fg(Color::Cyan)),
                ]));
            }

            lines.push(Line::raw("")); // message separator
        }
    }

    // Permission prompt — rendered inline at the bottom of the conversation
    if let Some(perm) = &state.conversation.pending_permission {
        lines.push(Line::from(Span::styled(
            "─── Permission Required ───",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));

        // Show the tool call title
        if let Some(title_text) = &perm.request.tool_call.fields.title {
            lines.push(Line::from(vec![
                Span::styled("  Tool: ", Style::default().fg(Color::Yellow)),
                Span::raw(title_text.clone()),
            ]));
        }

        // Show options
        for (i, opt) in perm.request.options.iter().enumerate() {
            let selected = i == perm.selected;
            let marker = if selected { "▸ " } else { "  " };
            let style = if selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            lines.push(Line::from(Span::styled(
                format!("  {marker}{}", opt.name),
                style,
            )));
        }

        lines.push(Line::from(Span::styled(
            "  Enter: select │ Esc: cancel",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let total = lines.len();
    state.conversation.total_lines = total;

    // Clamp scroll offset.
    let visible = area.height.saturating_sub(2) as usize;
    let max_scroll = total.saturating_sub(visible);
    state.conversation.scroll_offset = state.conversation.scroll_offset.min(max_scroll);
    let offset = state.conversation.scroll_offset as u16;

    let para = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(title),
        )
        .scroll((offset, 0));

    frame.render_widget(para, area);

    // Scrollbar overlay.
    if max_scroll > 0 {
        let mut scroll_state =
            ScrollbarState::new(max_scroll).position(state.conversation.scroll_offset);
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
