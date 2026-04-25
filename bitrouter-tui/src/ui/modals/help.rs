use ratatui::Frame;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::ui::layout::centered_rect;

pub fn render(frame: &mut Frame) {
    let area = centered_rect(60, 75, frame.area());
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
        Line::from(Span::styled("  Normal Mode", header_style)),
        Line::from(vec![
            Span::styled("  Enter         ", key_style),
            Span::styled("Send message", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  Shift+Enter   ", key_style),
            Span::styled("New line", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  @agent        ", key_style),
            Span::styled("Address a specific agent", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  @all          ", key_style),
            Span::styled("Broadcast to all agents", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  Tab           ", key_style),
            Span::styled("Accept autocomplete", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  Esc           ", key_style),
            Span::styled("Enter scroll mode", desc_style),
        ]),
        Line::raw(""),
        Line::from(Span::styled("  Scroll Mode", header_style)),
        Line::from(vec![
            Span::styled("  j / k         ", key_style),
            Span::styled("Scroll up/down", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  G             ", key_style),
            Span::styled("Jump to bottom, return to input", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  i             ", key_style),
            Span::styled("Return to input", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  /             ", key_style),
            Span::styled("Search scrollback", desc_style),
        ]),
        Line::raw(""),
        Line::from(Span::styled("  Session Mode (Alt+T)", header_style)),
        Line::from(vec![
            Span::styled("  h / l         ", key_style),
            Span::styled("Switch session left/right", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  1-9           ", key_style),
            Span::styled("Jump to session by number", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  n             ", key_style),
            Span::styled("New session (opens agent list)", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  x             ", key_style),
            Span::styled("Close current session", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  /             ", key_style),
            Span::styled("Filter sessions by title/agent", desc_style),
        ]),
        Line::raw(""),
        Line::from(Span::styled("  Agent Mode (Alt+A)", header_style)),
        Line::from(vec![
            Span::styled("  j / k         ", key_style),
            Span::styled("Navigate agent list", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  Enter / c     ", key_style),
            Span::styled("Connect and switch to tab", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  d             ", key_style),
            Span::styled("Disconnect agent", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  r             ", key_style),
            Span::styled("Rediscover agents", desc_style),
        ]),
        Line::raw(""),
        Line::from(Span::styled("  Permissions", header_style)),
        Line::from(vec![
            Span::styled("  y             ", key_style),
            Span::styled("Allow", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  n             ", key_style),
            Span::styled("Deny", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  a             ", key_style),
            Span::styled("Always allow", desc_style),
        ]),
        Line::raw(""),
        Line::from(Span::styled("  Global", header_style)),
        Line::from(vec![
            Span::styled("  Alt+1..9      ", key_style),
            Span::styled("Quick switch session", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl+Tab      ", key_style),
            Span::styled("MRU session cycle (older)", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl+Shift+Tab", key_style),
            Span::styled("MRU session cycle (newer)", desc_style),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl+B        ", key_style),
            Span::styled("Toggle threads sidebar", desc_style),
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
