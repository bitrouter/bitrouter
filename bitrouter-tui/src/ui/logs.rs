use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, List, ListItem};

use crate::app::AppState;

pub fn render(frame: &mut Frame, state: &AppState, area: Rect) {
    let border_style = Style::default().fg(Color::DarkGray);

    let items: Vec<ListItem> = if state.logs.lines.is_empty() {
        vec![ListItem::new(Span::styled(
            "(no log entries)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        state
            .logs
            .lines
            .iter()
            .map(|line| ListItem::new(Span::raw(line.as_str())))
            .collect()
    };

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title("Logs"),
    );

    frame.render_widget(list, area);
}
