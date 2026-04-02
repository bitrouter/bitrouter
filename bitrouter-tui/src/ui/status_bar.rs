use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{AppState, Focus};
use crate::model::AgentStatus;

pub fn render(frame: &mut Frame, state: &AppState, area: Rect) {
    let has_permission = state.feed.entries.iter().any(|e| {
        matches!(
            &e.kind,
            crate::model::EntryKind::PermissionRequest {
                resolved: false,
                ..
            }
        )
    });

    let focus_hint = if has_permission {
        " j/k: select option │ Enter: confirm │ Esc: cancel "
    } else {
        match state.focus {
            Focus::Feed => " i/Tab: input │ j/k: scroll │ Enter: expand/collapse │ ?: help ",
            Focus::Input => " Enter: send │ Alt+Enter: newline │ @agent: mention │ Esc: feed ",
        }
    };

    let connected_count = state
        .agents
        .iter()
        .filter(|a| matches!(a.status, AgentStatus::Connected | AgentStatus::Busy))
        .count();

    let right_info = format!(
        "  {connected_count} connected │ {}",
        state.config.listen_addr
    );

    let line = Line::from(vec![
        Span::styled(
            focus_hint,
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ),
        Span::styled(right_info, Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}
