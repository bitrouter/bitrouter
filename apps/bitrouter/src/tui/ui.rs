//! ratatui rendering of `AppState`. M2a: a tiled grid of agent panes with a
//! focus highlight, an agent-picker overlay, and a mode bar — plus the M1 input
//! box and permission popup.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line as TuiLine, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::tui::state::{AppState, Line, Mode, PaneState, PendingView, PickerState};

/// Render the whole app for one frame.
pub fn render(state: &AppState, frame: &mut Frame) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);
    let grid_area = chunks[0];

    let (panes, focus): (&[PaneState], usize) = match state.active() {
        Some(t) => (t.panes.as_slice(), t.focus),
        None => (&[], 0),
    };

    if state.zoom {
        if let Some(pane) = state.focused() {
            render_grid_pane(pane, true, focus, frame, grid_area);
        }
    } else {
        let rects = grid_rects(grid_area, panes.len());
        for (i, (pane, rect)) in panes.iter().zip(rects.iter()).enumerate() {
            render_grid_pane(pane, i == focus, i, frame, *rect);
        }
    }

    render_input(state, frame, chunks[1]);
    render_modebar(state, frame, chunks[2]);

    if state.mode == Mode::Picker
        && let Some(picker) = &state.picker
    {
        render_picker(picker, frame, area);
    }

    if let Some(pane) = state.focused()
        && let Some(pending) = &pane.pending
    {
        render_permission(pending, frame, area);
    }
}

/// Row-major tiled layout of `n` rects within `area`. `cols = ceil(sqrt(n))`,
/// `rows = ceil(n/cols)`; the final row's cells widen to fill. `n == 0` → empty.
fn grid_rects(area: Rect, n: usize) -> Vec<Rect> {
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![area];
    }
    let cols = (n as f64).sqrt().ceil() as usize;
    let rows = n.div_ceil(cols);
    let row_constraints: Vec<Constraint> = (0..rows)
        .map(|_| Constraint::Ratio(1, rows as u32))
        .collect();
    let row_rects = Layout::default()
        .direction(Direction::Vertical)
        .constraints(row_constraints)
        .split(area);

    let mut rects = Vec::with_capacity(n);
    for (r, row_rect) in row_rects.iter().enumerate() {
        let cells_in_row = (n - r * cols).min(cols);
        if cells_in_row == 0 {
            break;
        }
        let col_constraints: Vec<Constraint> = (0..cells_in_row)
            .map(|_| Constraint::Ratio(1, cells_in_row as u32))
            .collect();
        let col_rects = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(col_constraints)
            .split(*row_rect);
        for cr in col_rects.iter() {
            rects.push(*cr);
            if rects.len() == n {
                return rects;
            }
        }
    }
    rects
}

/// Render one agent pane: bordered block titled `[i] agent · shortid [⚠]`, with
/// the focused pane's border highlighted, showing the scrollback tail.
fn render_grid_pane(pane: &PaneState, focused: bool, index: usize, frame: &mut Frame, area: Rect) {
    let short = pane.record_id.get(..8).unwrap_or(pane.record_id.as_str());
    let warn = if pane.pending.is_some() { " ⚠" } else { "" };
    let title = format!(" [{}] {} · {}{} ", index + 1, pane.agent_id, short, warn);
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);
    let inner_height = area.height.saturating_sub(2) as usize;
    let start = pane.lines.len().saturating_sub(inner_height);
    let lines: Vec<TuiLine> = pane.lines[start..].iter().map(render_line).collect();
    let para = Paragraph::new(lines)
        .block(block)
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

fn render_modebar(state: &AppState, frame: &mut Frame, area: Rect) {
    let hints = match state.mode {
        Mode::Normal => "NORMAL  ^a agent · ^c quit",
        Mode::Agent => "AGENT  n new · x close · Tab focus · 1-9 · f zoom · Esc",
        Mode::Picker => "PICKER  up/down select · Enter spawn · Esc",
    };
    let text = match &state.notice {
        Some(n) => format!("{hints}   ! {n}"),
        None => hints.to_string(),
    };
    frame.render_widget(Paragraph::new(text), area);
}

fn render_picker(picker: &PickerState, frame: &mut Frame, area: Rect) {
    let popup = centered(area, 50, 50);
    frame.render_widget(Clear, popup);
    let items: Vec<TuiLine> = if picker.agents.is_empty() {
        vec![TuiLine::raw("(no agents configured)")]
    } else {
        picker
            .agents
            .iter()
            .enumerate()
            .map(|(i, a)| {
                if i == picker.selected {
                    TuiLine::styled(format!("> {a}"), Style::default().fg(Color::Cyan))
                } else {
                    TuiLine::raw(format!("  {a}"))
                }
            })
            .collect()
    };
    let para =
        Paragraph::new(items).block(Block::default().borders(Borders::ALL).title(" pick agent "));
    frame.render_widget(para, popup);
}

fn render_permission(pending: &PendingView, frame: &mut Frame, area: Rect) {
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

/// Single-key hint per option label (y/a/n), matching `reduce_key_normal`.
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
    use crate::tui::state::{AppState, Line, Mode, PaneState, PickerState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    fn draw(state: &AppState, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| render(state, f)).expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn three_panes() -> AppState {
        let mut st = AppState::new(PaneState::new("r0".into(), "a0".into()));
        st.tabs[0]
            .panes
            .push(PaneState::new("r1".into(), "a1".into()));
        st.tabs[0]
            .panes
            .push(PaneState::new("r2".into(), "a2".into()));
        st
    }

    #[test]
    fn renders_all_panes_with_indices() {
        let text = draw(&three_panes(), 80, 24);
        assert!(text.contains("a0") && text.contains("a1") && text.contains("a2"));
        assert!(text.contains("[1]") && text.contains("[2]") && text.contains("[3]"));
    }

    #[test]
    fn zoom_shows_only_focused_pane() {
        let mut st = three_panes();
        st.tabs[0].panes[1]
            .lines
            .push(Line::Message("SECOND_PANE_UNIQUE".into()));
        st.tabs[0].focus = 0;
        st.zoom = true;
        let text = draw(&st, 80, 24);
        assert!(text.contains("a0"), "focused agent present");
        assert!(
            !text.contains("SECOND_PANE_UNIQUE"),
            "non-focused content hidden when zoomed"
        );
    }

    #[test]
    fn picker_overlay_lists_agents() {
        let mut st = AppState::new(PaneState::new("r0".into(), "a0".into()));
        st.mode = Mode::Picker;
        st.picker = Some(PickerState {
            agents: vec!["alpha".into(), "beta".into()],
            selected: 0,
        });
        let text = draw(&st, 80, 24);
        assert!(text.contains("alpha") && text.contains("beta"));
    }

    #[test]
    fn single_message_line_renders_with_agent_title() {
        let mut pane = PaneState::new("rec-1".into(), "claude".into());
        pane.lines.push(Line::Message("hello world".into()));
        let text = draw(&AppState::new(pane), 60, 12);
        assert!(text.contains("hello world"));
        assert!(text.contains("claude"));
    }

    #[test]
    fn grid_rects_counts_and_non_overlap() {
        let area = Rect::new(0, 0, 80, 24);
        for n in 1..=6usize {
            let rects = grid_rects(area, n);
            assert_eq!(rects.len(), n, "n={n} rect count");
            for i in 0..rects.len() {
                for j in (i + 1)..rects.len() {
                    assert!(!overlaps(rects[i], rects[j]), "n={n} rects {i},{j} overlap");
                }
            }
        }
    }

    fn overlaps(a: Rect, b: Rect) -> bool {
        a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
    }
}
