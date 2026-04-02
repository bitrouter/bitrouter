use ratatui::Frame;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::AppState;
use crate::model::{AgentStatus, ObsEventKind, ObservabilityState};
use crate::ui::layout::centered_rect;

pub fn render(frame: &mut Frame, state: &AppState, modal: &ObservabilityState) {
    let area = centered_rect(80, 70, frame.area());
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = Vec::new();

    // Agent summary.
    lines.push(Line::from(Span::styled(
        " Agent Summary",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        " ─────────────────────────────────────",
        Style::default().fg(Color::DarkGray),
    )));

    for agent in &state.agents {
        let (status_str, color) = match &agent.status {
            AgentStatus::Idle => ("idle", Color::DarkGray),
            AgentStatus::Connecting => ("connecting", Color::Cyan),
            AgentStatus::Connected => ("connected", Color::Green),
            AgentStatus::Busy => ("busy", Color::Yellow),
            AgentStatus::Error(_) => ("error", Color::Red),
        };
        let has_tab = state.tabs.iter().any(|t| t.agent_name == agent.name);
        let tab_mark = if has_tab { " ●" } else { "" };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<14}", agent.name),
                Style::default().fg(agent.color),
            ),
            Span::styled(
                format!("{status_str}{tab_mark}"),
                Style::default().fg(color),
            ),
        ]));
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        " Live Event Log",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        " ─────────────────────────────────────",
        Style::default().fg(Color::DarkGray),
    )));

    let events: Vec<_> = state.obs_log.events.iter().rev().collect();
    let visible_start = modal.scroll_offset;
    let visible_area_height = area.height.saturating_sub(
        // header lines + border + footer
        (lines.len() as u16) + 4,
    ) as usize;

    for event in events.iter().skip(visible_start).take(visible_area_height) {
        let elapsed = event.timestamp.elapsed();
        let time_str = if elapsed.as_secs() < 60 {
            format!("{}s ago", elapsed.as_secs())
        } else {
            format!("{}m ago", elapsed.as_secs() / 60)
        };

        let kind_str = match &event.kind {
            ObsEventKind::Connected => "connected".to_string(),
            ObsEventKind::Disconnected => "disconnected".to_string(),
            ObsEventKind::PromptSent => "prompt sent".to_string(),
            ObsEventKind::PromptDone => "prompt done".to_string(),
            ObsEventKind::ToolCall { title } => format!("tool: {title}"),
            ObsEventKind::Error { message } => format!("error: {message}"),
        };

        lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<8}", time_str),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("[{}] ", event.agent_id),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw(kind_str),
        ]));
    }

    if state.obs_log.events.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No events yet",
            Style::default().fg(Color::DarkGray),
        )));
    }

    // Footer.
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        " j/k: scroll │ Esc: close",
        Style::default().fg(Color::DarkGray),
    )));

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(" Observability "),
    );

    frame.render_widget(para, area);
}
