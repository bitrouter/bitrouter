//! ratatui rendering of `AppState`: a fixed left rail (roster sorted by
//! actionability + radar strip) beside a splittable detail viewport, with the
//! input box, mode bar, and the picker/permission popups.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TuiLine, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::tui::state::{AppState, Line, Mode, PaneState, PendingView, PickerState, Split};

/// Preferred rail width; shrinks on narrow terminals. Wide enough for a row
/// plus its expanded `└ y·a·d risk · title` line to stay readable.
const RAIL_WIDTH: u16 = 28;

/// Render the whole app for one frame. Takes `&mut` so panes can record the
/// viewport height they were drawn at (ratatui stateful-render idiom) — the
/// reducer uses it to page the scrollback by exactly one screen.
pub fn render(state: &mut AppState, frame: &mut Frame) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),    // rail + detail
            Constraint::Length(3), // input
            Constraint::Length(1), // mode bar
        ])
        .split(area);
    // Narrow terminals get a proportional rail instead of a fixed one.
    let rail_w = RAIL_WIDTH.min(rows[0].width / 3);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(rail_w), Constraint::Min(1)])
        .split(rows[0]);

    render_rail(state, frame, cols[0]);
    render_detail(state, frame, cols[1]);
    render_input(state, frame, rows[1]);
    render_modebar(state, frame, rows[2]);

    if state.mode == Mode::Picker
        && let Some(picker) = &state.picker
    {
        render_picker(picker, frame, area);
    }

    if state.mode == Mode::Command
        && let Some(palette) = &state.palette
    {
        render_palette(palette, frame, area);
    }

    if state.keys_help {
        render_keys_help(state.mode, frame, area);
    }

    if let Some(pane) = state.focused()
        && let Some(pending) = &pane.pending
    {
        render_permission(pending, frame, area);
    }
}

/// Command palette: a filter line over the fuzzy-matched command list.
fn render_palette(palette: &crate::tui::state::PaletteState, frame: &mut Frame, area: Rect) {
    let popup = centered(area, 50, 50);
    frame.render_widget(Clear, popup);
    let mut lines: Vec<TuiLine> = vec![TuiLine::from(vec![
        Span::styled(": ", Style::default().fg(Color::Cyan)),
        Span::raw(palette.input.clone()),
        Span::styled("▏", Style::default().fg(Color::Cyan)),
    ])];
    let matches = palette.matches();
    if matches.is_empty() {
        lines.push(TuiLine::styled(
            "(no matching command)",
            Style::default().fg(Color::DarkGray),
        ));
    }
    for (i, (name, _)) in matches.iter().enumerate() {
        if i == palette.selected.min(matches.len() - 1) {
            lines.push(TuiLine::styled(
                format!("> {name}"),
                Style::default().fg(Color::Cyan),
            ));
        } else {
            lines.push(TuiLine::raw(format!("  {name}")));
        }
    }
    let para =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" command "));
    frame.render_widget(para, popup);
}

/// Which-key overlay: every binding for the current mode. Any key dismisses.
fn render_keys_help(mode: Mode, frame: &mut Frame, area: Rect) {
    let bindings: &[(&str, &str)] = match mode {
        Mode::Normal | Mode::Command => &[
            ("type + Enter", "prompt the focused agent"),
            ("y / a / n", "resolve its pending permission"),
            ("PgUp / PgDn", "scroll its scrollback"),
            (": (empty line)", "command palette"),
            ("Ctrl-A", "manager mode"),
            ("Ctrl-B", "broadcast mode"),
            ("Ctrl-C", "quit"),
        ],
        Mode::Agent => &[
            ("j / k / ↑ / ↓", "move the rail cursor"),
            ("Enter", "open cursor agent solo"),
            ("s / v", "split cursor agent in (h/v)"),
            ("u", "drop the focused slot"),
            ("Tab / ← / → / 1-4", "switch detail slot"),
            ("q", "queue focus (needs-you only)"),
            ("y / a / d", "resolve cursor pending"),
            ("A", "cycle autonomy tier"),
            ("n", "new agent"),
            ("x", "close cursor agent"),
            (":", "command palette"),
            ("Esc", "back to normal"),
        ],
        Mode::Picker => &[("↑ / ↓", "select"), ("Enter", "spawn"), ("Esc", "cancel")],
        Mode::Broadcast => &[
            ("Space", "toggle cursor row"),
            ("1-9", "toggle roster row"),
            ("a", "select all"),
            ("type + Enter", "send to selection"),
            ("Esc", "cancel"),
        ],
    };
    let popup = centered(area, 60, 60);
    frame.render_widget(Clear, popup);
    let lines: Vec<TuiLine> = bindings
        .iter()
        .map(|(key, what)| {
            TuiLine::from(vec![
                Span::styled(format!("{key:>18}  "), Style::default().fg(Color::Cyan)),
                Span::raw(*what),
            ])
        })
        .collect();
    let para = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" keys "));
    frame.render_widget(para, popup);
}

/// State glyph + color for one agent, shared by the roster and the radar.
/// Never color-alone: each state has a distinct glyph.
fn state_glyph(pane: &PaneState) -> (&'static str, Color) {
    if pane.pending.is_some() {
        ("⚠", Color::Red) // needs you
    } else if pane.attention {
        ("●", Color::Yellow) // happened in the background
    } else if !pane.exited {
        ("⣷", Color::Cyan) // running
    } else {
        ("✗", Color::DarkGray) // dead
    }
}

/// Left rail: the roster (every agent, sorted by actionability) over a radar
/// strip. The rail cursor (`▸`) is shown in AGENT and BROADCAST modes.
fn render_rail(state: &AppState, frame: &mut Frame, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let order = state.roster();
    let cursor_active = matches!(state.mode, Mode::Agent | Mode::Broadcast);
    let mut lines: Vec<TuiLine> = Vec::with_capacity(order.len());
    for (row, &idx) in order.iter().enumerate() {
        let pane = &state.agents[idx];
        let (glyph, color) = state_glyph(pane);
        let at_cursor = cursor_active && row == state.rail_cursor;
        let cursor = if at_cursor { "▸" } else { " " };
        let shown = state.detail.shown.iter().any(|r| r == &pane.record_id);
        let mut name_style = Style::default();
        if shown {
            name_style = name_style.add_modifier(Modifier::BOLD);
        }
        let mut spans = vec![
            Span::raw(cursor.to_string()),
            Span::styled(glyph.to_string(), Style::default().fg(color)),
            Span::raw(" "),
            Span::styled(pane.agent_id.clone(), name_style),
        ];
        if pane.selected {
            spans.push(Span::styled(" ✓", Style::default().fg(Color::Green)));
        }
        // Non-default autonomy is worth knowing at a glance.
        match pane.autonomy {
            crate::tui::state::Autonomy::Manual => {}
            crate::tui::state::Autonomy::Assisted => {
                spans.push(Span::styled(" [a]", Style::default().fg(Color::DarkGray)))
            }
            crate::tui::state::Autonomy::Auto => {
                spans.push(Span::styled(" [A]", Style::default().fg(Color::DarkGray)))
            }
        }
        lines.push(TuiLine::from(spans));
        // Actionable rows expand inline: risk + what the agent wants, and (on
        // the cursor row in AGENT mode) the resolve keys.
        if let Some(pending) = &pane.pending {
            let risk_span = match pending.risk {
                crate::tui::event::Risk::High => Span::styled(
                    "high · ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                crate::tui::event::Risk::Low => {
                    Span::styled("low · ", Style::default().fg(Color::DarkGray))
                }
            };
            // Keys first, then risk, then the (clippable) title — on a narrow
            // rail the actionable part must survive truncation.
            let mut detail = vec![Span::raw(" └ ")];
            if at_cursor && state.mode == Mode::Agent {
                detail.push(Span::styled(
                    "y·a·d ",
                    Style::default().add_modifier(Modifier::BOLD),
                ));
            }
            detail.push(risk_span);
            detail.push(Span::styled(
                pending.title.clone(),
                Style::default().fg(Color::Red),
            ));
            lines.push(TuiLine::from(detail));
        }
    }
    if lines.is_empty() {
        if state.queue_only {
            lines.push(TuiLine::styled(
                "✓ all clear",
                Style::default().fg(Color::Green),
            ));
        } else {
            lines.push(TuiLine::styled(
                "(no agents)",
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    let title = if state.queue_only {
        format!(" needs you · {} ", order.len())
    } else {
        format!(" agents · {} ", state.agents.len())
    };
    let roster = Paragraph::new(lines).block(Block::default().borders(Borders::RIGHT).title(title));
    frame.render_widget(roster, chunks[0]);

    // Radar: one glyph per agent in roster order — peripheral vision of every
    // agent's state even while the detail is zoomed into one.
    let radar: Vec<Span> = order
        .iter()
        .map(|&idx| {
            let (glyph, color) = state_glyph(&state.agents[idx]);
            Span::styled(glyph.to_string(), Style::default().fg(color))
        })
        .collect();
    frame.render_widget(Paragraph::new(TuiLine::from(radar)), chunks[1]);
}

/// Detail viewport: the shown agents in a horizontal or vertical split.
fn render_detail(state: &mut AppState, frame: &mut Frame, area: Rect) {
    let shown = state.detail.shown.clone();
    let focus = state.detail.focus;
    let split = state.detail.split;
    if shown.is_empty() {
        let placeholder = Paragraph::new("no agent shown — Ctrl-A then n to spawn")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL));
        frame.render_widget(placeholder, area);
        return;
    }
    let rects = split_rects(area, shown.len(), split);
    for (slot, (rid, rect)) in shown.iter().zip(rects.iter()).enumerate() {
        if let Some(pane) = state.agents.iter_mut().find(|p| &p.record_id == rid) {
            render_pane(pane, slot, slot == focus, frame, *rect);
        }
    }
}

/// Equal division of `area` into `n` slots: columns for [`Split::H`], rows for
/// [`Split::V`]. `n == 0` → empty.
fn split_rects(area: Rect, n: usize, split: Split) -> Vec<Rect> {
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![area];
    }
    let constraints: Vec<Constraint> = (0..n).map(|_| Constraint::Ratio(1, n as u32)).collect();
    let direction = match split {
        Split::H => Direction::Horizontal,
        Split::V => Direction::Vertical,
    };
    Layout::default()
        .direction(direction)
        .constraints(constraints)
        .split(area)
        .to_vec()
}

/// Render one detail pane: bordered block titled
/// `[slot] agent · harness · shortid [markers]`, focused slot highlighted.
/// Shows the scrollback tail unless the pane is pinned (`scroll`), and records
/// the drawn viewport height for paging.
fn render_pane(pane: &mut PaneState, slot: usize, focused: bool, frame: &mut Frame, area: Rect) {
    let short = pane.record_id.get(..8).unwrap_or(pane.record_id.as_str());
    let inner_height = area.height.saturating_sub(2) as usize;
    pane.viewport = inner_height;
    let tail_start = pane.lines.len().saturating_sub(inner_height);
    // A pin never scrolls past the tail view (no blank space below the tail).
    let start = pane.scroll.map(|s| s.min(tail_start)).unwrap_or(tail_start);
    let hidden_below = pane.lines.len() - (start + inner_height).min(pane.lines.len());

    let mut markers = String::new();
    if pane.pending.is_some() {
        markers.push_str(" ⚠");
    }
    if pane.attention {
        markers.push_str(" ●");
    }
    if pane.selected {
        markers.push_str(" ✓");
    }
    if pane.exited {
        markers.push_str(" ✗");
    }
    if hidden_below > 0 {
        // Off-tail indicator: how many newer lines are below the pinned view.
        markers.push_str(&format!(" ⇣{hidden_below}"));
    }
    let harness = if pane.harness.is_empty() {
        String::new()
    } else {
        format!(" · {}", pane.harness)
    };
    let title = format!(
        " [{}] {}{} · {}{} ",
        slot + 1,
        pane.agent_id,
        harness,
        short,
        markers
    );
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else if pane.selected {
        Style::default().fg(Color::Green)
    } else {
        Style::default()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);
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
        Line::Error(t) => TuiLine::from(vec![
            Span::styled("✗ ", Style::default().fg(Color::Red)),
            Span::styled(t.clone(), Style::default().fg(Color::Red)),
        ]),
        Line::AutoResolved(t) => TuiLine::from(vec![
            Span::styled("· ", Style::default().fg(Color::DarkGray)),
            Span::styled(t.clone(), Style::default().fg(Color::DarkGray)),
        ]),
    }
}

fn render_input(state: &AppState, frame: &mut Frame, area: Rect) {
    let (prefix, text) = if state.mode == Mode::Broadcast {
        ("⇉ ", state.broadcast_input.as_str())
    } else {
        ("› ", state.input.as_str())
    };
    let para =
        Paragraph::new(format!("{prefix}{text}")).block(Block::default().borders(Borders::ALL));
    frame.render_widget(para, area);
}

fn render_modebar(state: &AppState, frame: &mut Frame, area: Rect) {
    let hints = match state.mode {
        Mode::Normal => "NORMAL  ^a manage · ^b broadcast · : cmd · PgUp/PgDn scroll · ^c quit",
        Mode::Agent => {
            "AGENT  j/k · Enter open · s/v split · q queue · y/a/d · A tier · n new · x close · ? keys · Esc"
        }
        Mode::Picker => "PICKER  up/down select · Enter spawn · Esc",
        Mode::Broadcast => "BROADCAST  Space/1-9 select · a all · Enter send · Esc",
        Mode::Command => "COMMAND  type to filter · up/down select · Enter run · Esc",
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

/// Single-key hint per option label (y/a/n), matching `reduce_key_normal`'s
/// y/a/n handling (allow-once / allow-always / deny).
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
    use crate::tui::state::{AppState, DetailLayout, Line, Mode, PaneState, PickerState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    fn draw(state: &mut AppState, w: u16, h: u16) -> String {
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

    fn agents3() -> AppState {
        let mut st = AppState::new(PaneState::new("r0".into(), "a0".into()));
        st.agents.push(PaneState::new("r1".into(), "a1".into()));
        st.agents.push(PaneState::new("r2".into(), "a2".into()));
        st
    }

    #[test]
    fn rail_lists_every_agent_even_when_detail_is_solo() {
        let text = draw(&mut agents3(), 80, 24);
        assert!(text.contains("a0") && text.contains("a1") && text.contains("a2"));
        assert!(text.contains("agents · 3"), "rail header with count");
    }

    #[test]
    fn rail_sorts_actionable_agent_to_the_top() {
        let mut st = agents3();
        st.agents[2].pending = Some(crate::tui::state::PendingView {
            title: "WRITE".into(),
            diff: None,
            options: vec![],
            risk: crate::tui::event::Risk::High,
        });
        // Show r2's pane so the permission popup doesn't cover the rail.
        st.detail = DetailLayout {
            shown: vec!["r2".into()],
            split: crate::tui::state::Split::H,
            focus: 0,
        };
        let text = draw(&mut st, 80, 24);
        let (a2, a0) = (text.find("a2"), text.find("a0"));
        assert!(
            a2 < a0,
            "needs-you row renders above running rows: a2={a2:?} a0={a0:?}"
        );
        assert!(text.contains('⚠'), "needs-you glyph shown");
    }

    #[test]
    fn detail_shows_only_shown_agents() {
        let mut st = agents3();
        st.agents[1]
            .lines
            .push(Line::Message("SECOND_PANE_UNIQUE".into()));
        let text = draw(&mut st, 80, 24);
        assert!(
            !text.contains("SECOND_PANE_UNIQUE"),
            "non-shown agent content hidden"
        );
        assert!(text.contains("[1]"), "solo pane has slot header");
    }

    #[test]
    fn split_shows_two_panes_with_slots() {
        let mut st = agents3();
        st.detail = DetailLayout {
            shown: vec!["r0".into(), "r1".into()],
            split: crate::tui::state::Split::H,
            focus: 1,
        };
        st.agents[0]
            .lines
            .push(Line::Message("LEFT_CONTENT".into()));
        st.agents[1]
            .lines
            .push(Line::Message("RIGHT_CONTENT".into()));
        let text = draw(&mut st, 100, 24);
        assert!(text.contains("LEFT_CONTENT") && text.contains("RIGHT_CONTENT"));
        assert!(text.contains("[1]") && text.contains("[2]"));
    }

    #[test]
    fn pane_header_includes_harness_tag() {
        let mut st = AppState::new(PaneState::new("r0".into(), "api-1".into()));
        st.agents[0].harness = "codex".into();
        let text = draw(&mut st, 80, 24);
        assert!(
            text.contains("api-1 · codex"),
            "agent · harness header: {text:?}"
        );
    }

    #[test]
    fn split_rects_h_columns_v_rows_no_overlap() {
        let area = Rect::new(0, 0, 80, 24);
        for n in 1..=4usize {
            for split in [crate::tui::state::Split::H, crate::tui::state::Split::V] {
                let rects = split_rects(area, n, split);
                assert_eq!(rects.len(), n, "n={n} rect count");
                for i in 0..rects.len() {
                    for j in (i + 1)..rects.len() {
                        assert!(!overlaps(rects[i], rects[j]), "n={n} rects {i},{j} overlap");
                    }
                }
            }
        }
        // Direction: H splits along x, V along y.
        let h = split_rects(area, 2, crate::tui::state::Split::H);
        assert_eq!(h[0].y, h[1].y, "H = side-by-side");
        let v = split_rects(area, 2, crate::tui::state::Split::V);
        assert_eq!(v[0].x, v[1].x, "V = stacked");
    }

    fn overlaps(a: Rect, b: Rect) -> bool {
        a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
    }

    #[test]
    fn rail_shows_attention_glyph_for_background_agent() {
        let mut st = agents3();
        st.agents[1].attention = true;
        let text = draw(&mut st, 80, 24);
        assert!(text.contains('●'), "attention glyph rendered in rail/radar");
    }

    #[test]
    fn rail_shows_selection_marks_in_broadcast() {
        let mut st = agents3();
        st.mode = Mode::Broadcast;
        st.agents[0].selected = true;
        let text = draw(&mut st, 80, 24);
        assert!(text.contains('✓'), "selection marker rendered");
    }

    #[test]
    fn broadcast_input_renders_in_broadcast_mode() {
        let mut st = agents3();
        st.mode = Mode::Broadcast;
        st.broadcast_input = "BROADCAST_TEXT".into();
        let text = draw(&mut st, 80, 24);
        assert!(text.contains("BROADCAST_TEXT"), "broadcast input shown");
    }

    #[test]
    fn picker_overlay_lists_agents() {
        let mut st = AppState::new(PaneState::new("r0".into(), "a0".into()));
        st.mode = Mode::Picker;
        st.picker = Some(PickerState {
            agents: vec!["alpha".into(), "beta".into()],
            selected: 0,
        });
        let text = draw(&mut st, 80, 24);
        assert!(text.contains("alpha") && text.contains("beta"));
    }

    #[test]
    fn single_message_line_renders_with_agent_title() {
        let mut pane = PaneState::new("rec-1".into(), "claude".into());
        pane.lines.push(Line::Message("hello world".into()));
        let text = draw(&mut AppState::new(pane), 60, 12);
        assert!(text.contains("hello world"));
        assert!(text.contains("claude"));
    }

    #[test]
    fn pinned_pane_shows_off_tail_indicator_and_history() {
        let mut st = AppState::new(PaneState::new("r0".into(), "a0".into()));
        for i in 0..40 {
            st.agents[0]
                .lines
                .push(Line::Message(format!("hist{i}end")));
        }
        st.agents[0].scroll = Some(0);
        let text = draw(&mut st, 60, 12);
        assert!(text.contains('⇣'), "off-tail indicator visible: {text:?}");
        assert!(text.contains("hist0end"), "pinned view shows history top");
        assert!(!text.contains("hist39end"), "tail hidden while pinned");

        st.agents[0].scroll = None;
        let text = draw(&mut st, 60, 12);
        assert!(!text.contains('⇣'), "no indicator when following the tail");
        assert!(text.contains("hist39end"), "tail visible when following");
    }

    #[test]
    fn rail_expands_pending_row_with_title_and_resolve_hint() {
        let mut st = agents3();
        st.agents[1].pending = Some(crate::tui::state::PendingView {
            title: "rm -rf".into(),
            diff: None,
            options: vec![],
            risk: crate::tui::event::Risk::High,
        });
        st.mode = Mode::Agent;
        st.rail_cursor = 0; // r1 tops the roster
        let text = draw(&mut st, 100, 24);
        assert!(text.contains("rm -rf"), "pending title inline");
        assert!(text.contains('└'), "expanded row marker");
        assert!(text.contains("y·a·d"), "resolve hint on the cursor row");
    }

    #[test]
    fn rail_shows_risk_label_and_autonomy_tag() {
        let mut st = agents3();
        st.agents[1].pending = Some(crate::tui::state::PendingView {
            title: "wants".into(),
            diff: None,
            options: vec![],
            risk: crate::tui::event::Risk::High,
        });
        st.agents[2].autonomy = crate::tui::state::Autonomy::Auto;
        let text = draw(&mut st, 80, 24);
        assert!(text.contains("high ·"), "risk label on the expanded row");
        assert!(text.contains("[A]"), "auto tier tagged on the row");
    }

    #[test]
    fn queue_only_rail_shows_all_clear_when_empty() {
        let mut st = agents3();
        st.queue_only = true;
        let text = draw(&mut st, 80, 24);
        assert!(text.contains("all clear"), "empty queue reads calm");
        assert!(text.contains("needs you · 0"), "queue header with count");
    }

    #[test]
    fn palette_popup_renders_filter_and_matches() {
        let mut st = agents3();
        st.mode = Mode::Command;
        st.palette = Some(crate::tui::state::PaletteState {
            input: "sp".into(),
            selected: 0,
        });
        let text = draw(&mut st, 80, 24);
        assert!(text.contains("spawn agent"), "match listed");
        assert!(text.contains("> spawn agent"), "selection marked");
    }

    #[test]
    fn palette_popup_handles_no_matches() {
        let mut st = agents3();
        st.mode = Mode::Command;
        st.palette = Some(crate::tui::state::PaletteState {
            input: "zzz".into(),
            selected: 3,
        });
        let text = draw(&mut st, 80, 24);
        assert!(text.contains("no matching command"), "empty state shown");
    }

    #[test]
    fn keys_help_popup_lists_mode_bindings() {
        let mut st = agents3();
        st.mode = Mode::Agent;
        st.keys_help = true;
        let text = draw(&mut st, 90, 30);
        assert!(
            text.contains("cycle autonomy tier"),
            "agent bindings listed"
        );
        assert!(text.contains("command palette"));
    }

    #[test]
    fn empty_agent_list_renders_placeholders() {
        let mut st = agents3();
        st.agents.clear();
        st.detail = DetailLayout {
            shown: vec![],
            split: crate::tui::state::Split::H,
            focus: 0,
        };
        let text = draw(&mut st, 80, 24);
        assert!(text.contains("(no agents)"), "rail placeholder");
        assert!(text.contains("no agent shown"), "detail placeholder");
    }

    #[test]
    fn tiny_terminals_render_every_surface_without_panic() {
        use crate::tui::event::PermOption;
        use crate::tui::state::PendingView;
        use bitrouter_substrate::translate::PermissionOutcome;

        // Every render surface active at once: rail, split detail, input,
        // mode bar, picker overlay, permission popup, notice.
        let mut st = agents3();
        st.detail = DetailLayout {
            shown: vec!["r0".into(), "r1".into()],
            split: crate::tui::state::Split::V,
            focus: 0,
        };
        st.agents[0].pending = Some(PendingView {
            title: "write file".into(),
            diff: Some("+added\n-removed".into()),
            options: vec![PermOption {
                outcome: PermissionOutcome::AllowOnce,
                label: "allow".into(),
            }],
            risk: crate::tui::event::Risk::High,
        });
        st.agents[1].attention = true;
        st.mode = Mode::Picker;
        st.picker = Some(PickerState {
            agents: vec!["alpha".into()],
            selected: 0,
        });
        st.notice = Some("spawn failed".into());

        // Degenerate sizes: the spec's 20x5, plus 1-cell and 1-row/1-col
        // extremes. Passing = no panic; ratatui clamps layout.
        for (w, h) in [(1, 1), (2, 2), (5, 3), (10, 2), (20, 5), (80, 1), (1, 24)] {
            let _ = draw(&mut st, w, h);
        }
    }
}
