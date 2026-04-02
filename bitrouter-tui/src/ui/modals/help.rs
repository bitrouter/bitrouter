use ratatui::Frame;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::ui::layout::centered_rect;

pub fn render(frame: &mut Frame) {
    let area = centered_rect(60, 70, frame.area());
    frame.render_widget(Clear, area);

    let header_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let key_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(Color::White);

    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled("  Navigation", header_style)),
        Line::from(vec![
            Span::styled("  i / Tab       ", key_style),
            Span::styled("Focus input", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  Esc           ", key_style),
            Span::styled("Exit input / close modal", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  j / k         ", key_style),
            Span::styled("Scroll up/down in feed", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  Enter         ", key_style),
            Span::styled("Expand/collapse block (feed) or send (input)", desc_style),
        ]),
        Line::raw(""),
        Line::from(Span::styled("  Input", header_style)),
        Line::from(vec![
            Span::styled("  @agent        ", key_style),
            Span::styled("Address a specific agent", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  @all          ", key_style),
            Span::styled("Broadcast to all connected agents", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  Tab           ", key_style),
            Span::styled("Accept autocomplete suggestion", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  Alt+Enter     ", key_style),
            Span::styled("New line in input", desc_style),
        ]),
        Line::raw(""),
        Line::from(Span::styled("  Panels", header_style)),
        Line::from(vec![
            Span::styled("  Ctrl+G        ", key_style),
            Span::styled("Agent manager", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl+O        ", key_style),
            Span::styled("Observability", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl+P        ", key_style),
            Span::styled("Command palette", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  ?             ", key_style),
            Span::styled("Toggle this help", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl+C        ", key_style),
            Span::styled("Quit", desc_style),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "  Press Esc or ? to close",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(" Keyboard Shortcuts "),
    );

    frame.render_widget(para, area);
}
