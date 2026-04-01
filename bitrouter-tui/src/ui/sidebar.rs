use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem};

use crate::app::{AppState, Focus};
use crate::model::AgentStatus;

pub fn render(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let focused = state.focus == Focus::Sidebar;
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let items: Vec<ListItem> = state
        .sidebar
        .agents
        .iter()
        .map(|agent| {
            let (indicator, color) = match &agent.status {
                AgentStatus::Idle => ("● ", Color::Green),
                AgentStatus::Running => ("◎ ", Color::Yellow),
                AgentStatus::Error(_) => ("✗ ", Color::Red),
            };
            ListItem::new(Line::from(vec![
                Span::styled(indicator, Style::default().fg(color)),
                Span::raw(&agent.name),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title("Agents"),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");

    frame.render_stateful_widget(list, area, &mut state.sidebar.list_state);
}
