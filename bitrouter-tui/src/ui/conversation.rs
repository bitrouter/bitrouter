use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};

use crate::app::{AppState, Focus};
use crate::model::{ContentBlock, Role, ToolCallStatus};

pub fn render(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let focused = state.focus == Focus::Conversation;
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let inner_width = area.width.saturating_sub(4) as usize;
    let mut lines: Vec<Line> = Vec::new();

    let title = if let Some(session) = &state.conversation.session {
        format!("Conversation — {} ({})", session.agent_id, session.id)
    } else {
        "Conversation".to_string()
    };

    if let Some(session) = &state.conversation.session {
        for msg in &session.messages {
            let (prefix, prefix_style) = match msg.role {
                Role::User => (
                    "You  ",
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                ),
                Role::Agent => (
                    "Agent",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Role::System => ("Sys  ", Style::default().fg(Color::DarkGray)),
            };

            let mut first = true;
            for block in &msg.blocks {
                match block {
                    ContentBlock::Text(text) => {
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
                    ContentBlock::ToolCall {
                        tool_name,
                        status,
                        summary,
                    } => {
                        let (icon, color) = match status {
                            ToolCallStatus::Running => ("⟳", Color::Yellow),
                            ToolCallStatus::Done => ("✓", Color::Green),
                            ToolCallStatus::Failed => ("✗", Color::Red),
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
                                format!("{icon} {tool_name}: "),
                                Style::default().fg(color),
                            ),
                            Span::raw(summary.clone()),
                        ]));
                    }
                }
            }
            lines.push(Line::raw("")); // message separator
        }
    } else {
        lines.push(Line::from(Span::styled(
            "No active session",
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
