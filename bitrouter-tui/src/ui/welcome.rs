use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::app::App;

const LOGO_LARGE: &str = "\
██████╗ ██╗████████╗██████╗  ██████╗ ██╗   ██╗████████╗███████╗██████╗
██╔══██╗██║╚══██╔══╝██╔══██╗██╔═══██╗██║   ██║╚══██╔══╝██╔════╝██╔══██╗
██████╔╝██║   ██║   ██████╔╝██║   ██║██║   ██║   ██║   █████╗  ██████╔╝
██╔══██╗██║   ██║   ██╔══██╗██║   ██║██║   ██║   ██║   ██╔══╝  ██╔══██╗
██████╔╝██║   ██║   ██║  ██║╚██████╔╝╚██████╔╝   ██║   ███████╗██║  ██║
╚═════╝ ╚═╝   ╚═╝   ╚═╝  ╚═╝ ╚═════╝  ╚═════╝   ╚═╝   ╚══════╝╚═╝  ╚═╝";

const LOGO_SMALL: &str = "\
 ___ _ _   ___          _
| _ |_) |_| _ \\___ _  _| |_ ___ _ _
| _ \\ |  _|   / _ \\ || |  _/ -_) '_|
|___/_|\\__|_|_\\___/\\_,_|\\__\\___|_|";

const LOGO_LARGE_WIDTH: u16 = 70;

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    let logo = if area.width >= LOGO_LARGE_WIDTH + 4 {
        LOGO_LARGE
    } else {
        LOGO_SMALL
    };

    let mut lines: Vec<Line> = Vec::new();

    // Logo
    let logo_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    for line in logo.lines() {
        lines.push(Line::from(Span::styled(line, logo_style)));
    }

    // Tagline
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Open Intelligence Router for LLM Agents",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )));

    // Separator
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "───────────────────────────────────────",
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(""));

    // Server info
    let info_style = Style::default().fg(Color::Gray);
    lines.push(Line::from(Span::styled(
        format!("Listening on {}", app.config.listen_addr),
        info_style,
    )));
    lines.push(Line::from(Span::styled(
        format!(
            "{} provider{} configured",
            app.config.providers.len(),
            if app.config.providers.len() == 1 { "" } else { "s" }
        ),
        info_style,
    )));
    lines.push(Line::from(Span::styled(
        format!(
            "{} route{} active",
            app.config.route_count,
            if app.config.route_count == 1 { "" } else { "s" }
        ),
        info_style,
    )));

    // Quit hint
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Press q to quit",
        Style::default().fg(Color::DarkGray),
    )));

    let content_height = lines.len() as u16;
    let centered = center_vertically(area, content_height);

    let paragraph = Paragraph::new(lines).alignment(Alignment::Center);
    frame.render_widget(paragraph, centered);
}

fn center_vertically(area: Rect, height: u16) -> Rect {
    let height = height.min(area.height);
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(area.x, y, area.width, height)
}
