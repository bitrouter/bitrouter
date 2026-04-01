use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders};

use crate::app::{AppState, Focus};

pub fn render(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let focused = state.focus == Focus::Input;
    let has_permission = state.conversation.pending_permission.is_some();

    let border_style = if has_permission {
        Style::default().fg(Color::Yellow)
    } else if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let title = if has_permission {
        "Permission required — j/k: select, Enter: confirm, Esc: cancel"
    } else if focused {
        "Input (Enter: send, Esc: back)"
    } else {
        "Input"
    };

    state.input.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(title),
    );
    state.input.set_cursor_line_style(Style::default());

    if focused && !has_permission {
        state
            .input
            .set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));
    } else {
        state.input.set_cursor_style(Style::default());
    }

    frame.render_widget(&state.input, area);
}
