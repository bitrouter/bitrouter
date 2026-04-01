use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{AppState, Focus};

pub fn render(frame: &mut Frame, state: &AppState, area: Rect) {
    let focus_hint = if state.conversation.pending_permission.is_some() {
        " j/k: select option | Enter: confirm | Esc: cancel "
    } else {
        match state.focus {
            Focus::Sidebar => " Tab: next panel | j/k: select agent | Shift+Tab: switch tab ",
            Focus::Conversation => " Tab: next panel | j/k: scroll | Shift+Tab: switch tab ",
            Focus::Input => " Enter: send | Esc: back | Tab: next panel ",
        }
    };

    let line = Line::from(vec![
        Span::styled(
            focus_hint,
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ),
        Span::styled(
            format!(
                "  {} agents | {}",
                state.sidebar.agents.len(),
                state.config.listen_addr
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}
