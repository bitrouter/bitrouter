use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::AppState;
use crate::model::AgentStatus;

pub fn render(frame: &mut Frame, state: &AppState, area: Rect) {
    let mut spans: Vec<Span> = Vec::new();

    for agent in &state.agents {
        let (dot, dot_color) = match &agent.status {
            AgentStatus::Idle => ("○", Color::DarkGray),
            AgentStatus::Connecting => ("◌", Color::Cyan),
            AgentStatus::Connected => ("●", Color::Green),
            AgentStatus::Busy => ("◎", Color::Yellow),
            AgentStatus::Error(_) => ("✗", Color::Red),
        };

        let is_default = state.default_agent.as_deref() == Some(&agent.name);
        let name_label = if is_default {
            format!("{}*", agent.name)
        } else {
            agent.name.clone()
        };

        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!("{dot} "),
            Style::default().fg(dot_color),
        ));
        spans.push(Span::styled(
            name_label,
            Style::default()
                .fg(agent.color)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Right-aligned hints.
    let left_width: usize = spans.iter().map(|s| s.width()).sum();
    let right_text = "Ctrl+G agents  Ctrl+P cmd  ? help";
    let padding = (area.width as usize).saturating_sub(left_width + right_text.len() + 1);
    if padding > 0 {
        spans.push(Span::raw(" ".repeat(padding)));
    }
    spans.push(Span::styled(
        right_text,
        Style::default().fg(Color::DarkGray),
    ));

    let line = Line::from(spans);
    frame.render_widget(Paragraph::new(line), area);
}
