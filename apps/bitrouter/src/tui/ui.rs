//! ratatui rendering of `AppState`. M1: single full-height pane + input line,
//! with a permission overlay when the focused pane has a pending request.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line as TuiLine, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::tui::state::{AppState, Line};

/// Render the whole app for one frame.
pub fn render(state: &AppState, frame: &mut Frame) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(area);

    render_pane(state, frame, chunks[0]);
    render_input(state, frame, chunks[1]);

    if let Some(pane) = state.focused()
        && let Some(pending) = &pane.pending
    {
        render_permission(pending, frame, area);
    }
}

fn render_pane(state: &AppState, frame: &mut Frame, area: Rect) {
    let title = match state.focused() {
        Some(p) => format!(" {} · {} ", p.record_id, p.agent_id),
        None => " bitrouter tui ".to_string(),
    };
    let lines: Vec<TuiLine> = state
        .focused()
        .map(|p| p.lines.iter().map(render_line).collect())
        .unwrap_or_default();
    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

fn render_line(line: &Line) -> TuiLine<'static> {
    match line {
        Line::UserPrompt(t) => TuiLine::from(vec![
            Span::styled("› ", Style::default().fg(Color::Cyan)),
            Span::raw(t.clone()),
        ]),
        Line::Message(t) => TuiLine::raw(t.clone()),
        Line::Thought(t) => TuiLine::styled(t.clone(), Style::default().fg(Color::DarkGray)),
        Line::Tool { title, status, .. } => TuiLine::from(vec![
            Span::styled("⚒ ", Style::default().fg(Color::Yellow)),
            Span::raw(title.clone()),
            Span::raw(format!(" [{status:?}]")),
        ]),
    }
}

fn render_input(state: &AppState, frame: &mut Frame, area: Rect) {
    let para =
        Paragraph::new(format!("› {}", state.input)).block(Block::default().borders(Borders::ALL));
    frame.render_widget(para, area);
}

fn render_permission(pending: &crate::tui::state::PendingView, frame: &mut Frame, area: Rect) {
    let popup = centered(area, 70, 40);
    frame.render_widget(Clear, popup);
    let mut lines: Vec<TuiLine> = vec![TuiLine::raw(pending.title.clone())];
    if let Some(diff) = &pending.diff {
        for l in diff.lines() {
            lines.push(TuiLine::raw(l.to_string()));
        }
    }
    let keys: Vec<String> = pending
        .options
        .iter()
        .map(|o| format!("[{}] {}", key_for(&o.label), o.label))
        .collect();
    lines.push(TuiLine::raw(keys.join("   ")));
    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" permission "))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, popup);
}

/// Single-key hint per option label (y/a/n), matching `reduce_key`.
fn key_for(label: &str) -> char {
    match label {
        "allow" => 'y',
        "allow always" => 'a',
        _ => 'n',
    }
}

fn centered(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(v[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::{AppState, Line, PaneState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn renders_a_message_line_in_the_pane() {
        let mut pane = PaneState::new("rec-1".into(), "claude".into());
        pane.lines.push(Line::Message("hello world".into()));
        let state = AppState::new(pane);

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| render(&state, f)).expect("draw");

        let buffer = terminal.backend().buffer().clone();
        let text: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("hello world"), "pane should show the message");
        assert!(text.contains("claude"), "title should show the agent id");
    }
}
