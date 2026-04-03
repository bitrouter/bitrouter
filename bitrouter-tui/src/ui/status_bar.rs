use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{AppState, InputMode};
use crate::model::AgentStatus;

pub fn render(frame: &mut Frame, state: &AppState, area: Rect) {
    let (mode_label, mode_color, hints) = mode_hints(state);

    let connected_count = state
        .agents
        .iter()
        .filter(|a| matches!(a.status, AgentStatus::Connected | AgentStatus::Busy))
        .count();

    let right_info = format!(
        "  {connected_count} connected │ {}",
        state.config.listen_addr
    );

    // Build hint spans: each key in bg(mode_color), description in fg(mode_color).
    let mut spans = Vec::new();

    // Mode label.
    spans.push(Span::styled(
        format!(" {mode_label} "),
        Style::default()
            .fg(Color::Black)
            .bg(mode_color)
            .add_modifier(Modifier::BOLD),
    ));

    // Hint pairs.
    spans.push(Span::styled(
        format!(" {hints}"),
        Style::default().fg(mode_color),
    ));

    // Right info.
    let left_width: usize = spans.iter().map(|s| s.width()).sum();
    let padding = (area.width as usize).saturating_sub(left_width + right_info.len());
    if padding > 0 {
        spans.push(Span::raw(" ".repeat(padding)));
    }
    spans.push(Span::styled(
        right_info,
        Style::default().fg(Color::DarkGray),
    ));

    let line = Line::from(spans);
    frame.render_widget(Paragraph::new(line), area);
}

fn mode_hints(state: &AppState) -> (&'static str, Color, String) {
    match &state.mode {
        InputMode::Normal => (
            "NORMAL",
            Color::Cyan,
            "Enter: send │ @agent: mention │ Esc: scroll │ ^T: tabs │ ^A: agents".to_string(),
        ),
        InputMode::Scroll => (
            "SCROLL",
            Color::Yellow,
            "j/k: scroll │ G: bottom │ /: search │ i: input │ Esc: back".to_string(),
        ),
        InputMode::Tab => (
            "TAB",
            Color::Magenta,
            "h/l: switch │ 1-9: jump │ n: new │ x: close │ Esc: back".to_string(),
        ),
        InputMode::Agent => (
            "AGENT",
            Color::Green,
            "j/k: select │ Enter/c: connect │ d: disconnect │ r: rediscover │ Esc: back"
                .to_string(),
        ),
        InputMode::Search => {
            let query = state
                .search
                .as_ref()
                .map(|s| s.query.as_str())
                .unwrap_or("");
            let match_info = state
                .search
                .as_ref()
                .map(|s| {
                    if s.matches.is_empty() {
                        "no matches".to_string()
                    } else {
                        format!("{}/{}", s.current_match + 1, s.matches.len())
                    }
                })
                .unwrap_or_default();
            (
                "SEARCH",
                Color::Blue,
                format!("/{query}  {match_info} │ Enter: next │ Esc: cancel"),
            )
        }
        InputMode::Permission => (
            "PERMISSION",
            Color::Yellow,
            "y: allow │ n: deny │ a: always".to_string(),
        ),
    }
}
