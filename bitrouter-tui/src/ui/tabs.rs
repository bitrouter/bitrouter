use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Tabs;

use crate::app::{AppState, Tab};

pub fn render(frame: &mut Frame, state: &AppState, area: Rect) {
    let titles = vec![Line::from(" Conversation "), Line::from(" Logs ")];
    let selected = match state.tab {
        Tab::Conversation => 0,
        Tab::Logs => 1,
    };
    let tabs = Tabs::new(titles)
        .select(selected)
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .divider("|");
    frame.render_widget(tabs, area);
}
