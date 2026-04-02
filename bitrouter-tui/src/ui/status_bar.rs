use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{AppState, Focus};
use crate::model::{AgentStatus, EntryKind};

pub fn render(frame: &mut Frame, state: &AppState, area: Rect) {
    let has_permission = state
        .scrollback
        .entries
        .iter()
        .any(|e| matches!(&e.kind, EntryKind::Permission(p) if !p.resolved));

    let focus_hint = if has_permission {
        " y: allow │ n: deny │ a: always "
    } else {
        match state.focus {
            Focus::Input => " Enter: send │ Shift+Enter: newline │ @agent: mention │ Esc: scroll ",
            Focus::Scroll => " j/k: scroll │ i: input │ Tab: switch agent │ ?: help ",
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
