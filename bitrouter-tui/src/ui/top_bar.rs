use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::AppState;
use crate::model::{AgentStatus, TabBadge};

pub fn render(frame: &mut Frame, state: &AppState, area: Rect) {
    let mut spans: Vec<Span> = Vec::new();

    for (i, tab) in state.tabs.iter().enumerate() {
        let is_active = i == state.active_tab;

        // Look up agent status for the dot indicator.
        let agent_status = state
            .agents
            .iter()
            .find(|a| a.name == tab.agent_name)
            .map(|a| &a.status);

        let agent_color = state
            .agents
            .iter()
            .find(|a| a.name == tab.agent_name)
            .map(|a| a.color)
            .unwrap_or(Color::White);

        let (dot, dot_color) = match agent_status {
            Some(AgentStatus::Idle) => ("○", Color::DarkGray),
            Some(AgentStatus::Connecting) => ("◌", Color::Cyan),
            Some(AgentStatus::Connected) => ("●", Color::Green),
            Some(AgentStatus::Busy) => ("◎", Color::Yellow),
            Some(AgentStatus::Error(_)) => ("✗", Color::Red),
            None => ("○", Color::DarkGray),
        };

        // Badge suffix.
        let badge_str = match &tab.badge {
            TabBadge::None => String::new(),
            TabBadge::Unread(n) => format!(" [{n}]"),
            TabBadge::Permission => " ⚠".to_string(),
        };

        if !spans.is_empty() {
            spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
        }

        if is_active {
            spans.push(Span::styled(
                format!("{dot} "),
                Style::default().fg(dot_color),
            ));
            spans.push(Span::styled(
                tab.agent_name.clone(),
                Style::default()
                    .fg(agent_color)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ));
        } else {
            spans.push(Span::styled(
                format!("{dot} "),
                Style::default().fg(dot_color),
            ));
            spans.push(Span::styled(
                tab.agent_name.clone(),
                Style::default().fg(agent_color),
            ));
        }

        if !badge_str.is_empty() {
            let badge_color = match &tab.badge {
                TabBadge::Permission => Color::Yellow,
                _ => Color::DarkGray,
            };
            spans.push(Span::styled(badge_str, Style::default().fg(badge_color)));
        }
    }

    // If no tabs, show a hint.
    if state.tabs.is_empty() {
        spans.push(Span::styled(
            "No tabs — Alt+A to connect an agent",
            Style::default().fg(Color::DarkGray),
        ));
    }

    // Right-aligned hints.
    let left_width: usize = spans.iter().map(|s| s.width()).sum();
    let right_text = "Alt+T tabs  Alt+A agents  Ctrl+P cmd  ? help";
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
