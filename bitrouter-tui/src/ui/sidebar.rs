use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::app::AppState;
use crate::model::{Agent, AgentStatus, Session, SessionBadge};

/// Render the threads sidebar.
pub fn render(frame: &mut Frame, state: &AppState, area: Rect) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = build_lines(
        &state.session_store.active,
        &state.agents,
        state.active_session,
    );
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Build the sidebar's line list. Extracted for unit-testing.
fn build_lines(
    sessions: &[Session],
    agents: &[Agent],
    active_session: usize,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(Span::styled(
        " Threads",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::raw(""));

    if sessions.is_empty() {
        lines.push(Line::from(Span::styled(
            " (no sessions)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (i, session) in sessions.iter().enumerate() {
            lines.push(session_line(session, agents, i == active_session));
        }
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        " [+ new]   [↓ import]",
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(Span::styled(
        " Ctrl-B hide",
        Style::default().fg(Color::DarkGray),
    )));

    lines
}

fn session_line(session: &Session, agents: &[Agent], is_active: bool) -> Line<'static> {
    let agent = agents.iter().find(|a| a.name == session.agent_id);
    let (dot, dot_color) = status_dot(agent.map(|a| &a.status));

    let (badge, badge_color) = match &session.badge {
        SessionBadge::None => (String::new(), Color::DarkGray),
        SessionBadge::Unread(n) => (format!(" [{n}]"), Color::DarkGray),
        SessionBadge::Permission => (" ⚠".to_string(), Color::Yellow),
    };

    // Per-session display: prefer title (auto-derived from first
    // prompt), fall back to agent_id when the session is fresh.
    let label = session
        .title
        .clone()
        .unwrap_or_else(|| session.agent_id.clone());
    let id_tag = format!("#{} ", session.id.0);
    let name_style = if is_active {
        Style::default()
            .fg(session.color)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(session.color)
    };
    let marker = if is_active { "▸ " } else { "  " };

    Line::from(vec![
        Span::styled(marker, Style::default().fg(Color::Cyan)),
        Span::styled(format!("{dot} "), Style::default().fg(dot_color)),
        Span::styled(id_tag, Style::default().fg(Color::DarkGray)),
        Span::styled(label, name_style),
        Span::styled(badge, Style::default().fg(badge_color)),
    ])
}

fn status_dot(status: Option<&AgentStatus>) -> (&'static str, Color) {
    match status {
        Some(AgentStatus::Idle) => ("○", Color::DarkGray),
        Some(AgentStatus::Available) => ("◇", Color::Blue),
        Some(AgentStatus::Installing { .. }) => ("⟳", Color::Cyan),
        Some(AgentStatus::Connecting) => ("◌", Color::Cyan),
        Some(AgentStatus::Connected) => ("●", Color::Green),
        Some(AgentStatus::Busy) => ("◎", Color::Yellow),
        Some(AgentStatus::Error(_)) => ("✗", Color::Red),
        None => ("○", Color::DarkGray),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ScrollbackState, SessionId, SessionStatus};

    fn mk_session(id: u64, agent_id: &str) -> Session {
        Session {
            id: SessionId(id),
            agent_id: agent_id.to_string(),
            title: None,
            color: Color::Green,
            acp_session_id: None,
            status: SessionStatus::Connected,
            scrollback: ScrollbackState::new(),
            badge: SessionBadge::None,
        }
    }

    fn mk_agent(name: &str) -> Agent {
        Agent {
            name: name.to_string(),
            config: None,
            status: AgentStatus::Connected,
            color: Color::Green,
        }
    }

    fn join_line(line: &Line) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn empty_sessions_shows_placeholder() {
        let lines = build_lines(&[], &[], 0);
        let joined: String = lines.iter().map(join_line).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("Threads"));
        assert!(joined.contains("(no sessions)"));
    }

    #[test]
    fn single_session_shows_id_tag_and_name() {
        let sessions = vec![mk_session(0, "claude-code")];
        let agents = vec![mk_agent("claude-code")];
        let lines = build_lines(&sessions, &agents, 0);
        let joined: String = lines.iter().map(join_line).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("#0"));
        assert!(joined.contains("claude-code"));
        // Active marker.
        assert!(joined.contains("▸ "));
    }

    #[test]
    fn multiple_sessions_assign_unique_id_tags() {
        let sessions = vec![
            mk_session(0, "claude-code"),
            mk_session(1, "codex"),
            mk_session(2, "gemini-cli"),
        ];
        let agents = vec![
            mk_agent("claude-code"),
            mk_agent("codex"),
            mk_agent("gemini-cli"),
        ];
        let lines = build_lines(&sessions, &agents, 1);
        let joined: String = lines.iter().map(join_line).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("#0"));
        assert!(joined.contains("#1"));
        assert!(joined.contains("#2"));
    }

    #[test]
    fn session_title_replaces_agent_id_when_present() {
        let mut sess = mk_session(0, "claude-code");
        sess.title = Some("refactor router".to_string());
        let lines = build_lines(&[sess], &[mk_agent("claude-code")], 0);
        let joined: String = lines.iter().map(join_line).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("refactor router"));
        // agent_id no longer shown when title takes its slot
        assert!(!joined.contains("claude-code"));
    }

    #[test]
    fn permission_badge_rendered() {
        let mut sess = mk_session(0, "claude-code");
        sess.badge = SessionBadge::Permission;
        let lines = build_lines(&[sess], &[mk_agent("claude-code")], 0);
        let joined: String = lines.iter().map(join_line).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("⚠"));
    }

    #[test]
    fn unread_badge_rendered() {
        let mut sess = mk_session(0, "claude-code");
        sess.badge = SessionBadge::Unread(5);
        let lines = build_lines(&[sess], &[mk_agent("claude-code")], 0);
        let joined: String = lines.iter().map(join_line).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("[5]"));
    }
}
