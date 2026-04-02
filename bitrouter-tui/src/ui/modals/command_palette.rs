use ratatui::Frame;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::AppState;
use crate::model::CommandPaletteState;
use crate::ui::layout::centered_rect;

pub fn render(frame: &mut Frame, _state: &AppState, modal: &CommandPaletteState) {
    let area = centered_rect(50, 50, frame.area());
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = Vec::new();

    // Search input line.
    let query_display = if modal.query.is_empty() {
        Span::styled("Type to search...", Style::default().fg(Color::DarkGray))
    } else {
        Span::styled(modal.query.clone(), Style::default().fg(Color::White))
    };
    lines.push(Line::from(vec![
        Span::styled(" > ", Style::default().fg(Color::Cyan)),
        query_display,
        Span::styled("▍", Style::default().fg(Color::Cyan)),
    ]));
    lines.push(Line::from(Span::styled(
        " ─────────────────────────────────",
        Style::default().fg(Color::DarkGray),
    )));

    // Filtered commands.
    let visible_count = (area.height.saturating_sub(5)) as usize;
    for (display_idx, &cmd_idx) in modal.filtered.iter().enumerate().take(visible_count) {
        if let Some(cmd) = modal.all_commands.get(cmd_idx) {
            let is_selected = display_idx == modal.selected;
            let marker = if is_selected { "▸ " } else { "  " };
            let style = if is_selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            lines.push(Line::from(Span::styled(
                format!(" {marker}{}", cmd.label),
                style,
            )));
        }
    }

    if modal.filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No matching commands",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(" Command Palette "),
    );

    frame.render_widget(para, area);
}
