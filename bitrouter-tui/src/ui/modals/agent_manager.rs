use ratatui::Frame;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::AppState;
use crate::model::{AgentManagerState, AgentStatus};
use crate::ui::layout::centered_rect;

pub fn render(frame: &mut Frame, state: &AppState, modal: &AgentManagerState) {
    let area = centered_rect(70, 60, frame.area());
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = Vec::new();

    // Header.
    lines.push(Line::from(vec![Span::styled(
        " Name           Status       Session       ",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::from(Span::styled(
        " ─────────────────────────────────────────────",
        Style::default().fg(Color::DarkGray),
    )));

    for (i, agent) in state.agents.iter().enumerate() {
        let is_selected = i == modal.selected;
        let marker = if is_selected { "▸" } else { " " };

        let error_display: String;
        let (status_str, status_color) = match &agent.status {
            AgentStatus::Idle => ("idle", Color::DarkGray),
            AgentStatus::Connecting => ("connecting", Color::Cyan),
            AgentStatus::Connected => ("connected", Color::Green),
            AgentStatus::Busy => ("busy", Color::Yellow),
            AgentStatus::Error(msg) => {
                // Truncate long error messages for display.
                if msg.len() > 30 {
                    error_display = format!("err: {}…", &msg[..30]);
                } else {
                    error_display = format!("err: {msg}");
                }
                (error_display.as_str(), Color::Red)
            }
        };

        let session_str = agent
            .session_id
            .as_ref()
            .map(|s| {
                let s_str = format!("{s:?}");
                if s_str.len() > 12 {
                    format!("{}…", &s_str[..12])
                } else {
                    s_str
                }
            })
            .unwrap_or_else(|| "—".to_string());

        let is_default = state.default_agent.as_deref() == Some(&agent.name);
        let default_marker = if is_default { "*" } else { " " };

        let row_style = if is_selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };

        lines.push(Line::from(vec![
            Span::styled(format!("{marker} "), row_style),
            Span::styled(format!("{:<14}", agent.name), row_style.fg(agent.color)),
            Span::styled(default_marker, row_style.fg(Color::Yellow)),
            Span::styled(format!(" {:<12}", status_str), row_style.fg(status_color)),
            Span::styled(format!(" {session_str}"), row_style.fg(Color::DarkGray)),
        ]));
    }

    // Footer with keybinding hints.
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        " c: connect │ d: disconnect │ s: set default │ r: rediscover │ Esc: close",
        Style::default().fg(Color::DarkGray),
    )));

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(" Agent Manager "),
    );

    frame.render_widget(para, area);
}
