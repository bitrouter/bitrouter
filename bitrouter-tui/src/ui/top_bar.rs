use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::AppState;
use crate::model::{AgentStatus, SessionBadge};

pub fn render(frame: &mut Frame, state: &AppState, area: Rect) {
    let mut spans: Vec<Span> = Vec::new();

    for (i, session) in state.session_store.active.iter().enumerate() {
        let is_active = i == state.active_session;

        // Status comes from the agent (provider reachability).
        let agent_status = state
            .agents
            .iter()
            .find(|a| a.name == session.agent_id)
            .map(|a| &a.status);

        let (dot, dot_color) = match agent_status {
            Some(AgentStatus::Idle) => ("○", Color::DarkGray),
            Some(AgentStatus::Available) => ("◇", Color::Blue),
            Some(AgentStatus::Installing { .. }) => ("⟳", Color::Cyan),
            Some(AgentStatus::Connecting) => ("◌", Color::Cyan),
            Some(AgentStatus::Connected) => ("●", Color::Green),
            Some(AgentStatus::Busy) => ("◎", Color::Yellow),
            Some(AgentStatus::Error(_)) => ("✗", Color::Red),
            None => ("○", Color::DarkGray),
        };

        // Badge suffix.
        let badge_str = match &session.badge {
            SessionBadge::None => String::new(),
            SessionBadge::Unread(n) => format!(" [{n}]"),
            SessionBadge::Permission => " ⚠".to_string(),
        };

        if !spans.is_empty() {
            spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
        }

        let label = session
            .title
            .clone()
            .unwrap_or_else(|| session.agent_id.clone());
        let style = if is_active {
            Style::default()
                .fg(session.color)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(session.color)
        };
        spans.push(Span::styled(
            format!("{dot} "),
            Style::default().fg(dot_color),
        ));
        spans.push(Span::styled(label, style));

        if !badge_str.is_empty() {
            let badge_color = match &session.badge {
                SessionBadge::Permission => Color::Yellow,
                _ => Color::DarkGray,
            };
            spans.push(Span::styled(badge_str, Style::default().fg(badge_color)));
        }
    }

    // If no sessions, show a hint.
    if state.session_store.active.is_empty() {
        spans.push(Span::styled(
            "No sessions — Alt+A to connect an agent",
            Style::default().fg(Color::DarkGray),
        ));
    }

    // Right-aligned hints.
    let left_width: usize = spans.iter().map(|s| s.width()).sum();
    let right_text = "Ctrl+B sidebar  Ctrl+Tab MRU  Alt+T sessions  Alt+A agents  Ctrl+P cmd";
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
