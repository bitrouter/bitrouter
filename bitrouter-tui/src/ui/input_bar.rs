use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders};

use crate::app::{AppState, Focus};

pub fn render(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let focused = state.focus == Focus::Input;
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let title = if focused {
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

    if focused {
        state
            .input
            .set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));
    } else {
        state.input.set_cursor_style(Style::default());
    }

    frame.render_widget(&state.input, area);
}
