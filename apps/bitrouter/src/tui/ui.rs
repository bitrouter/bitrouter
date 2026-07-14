//! ratatui rendering of `AppState`: a fixed left rail (roster sorted by
//! actionability + radar strip) beside a splittable detail viewport, with the
//! input box, mode bar, and the picker/permission popups.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TuiLine, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::tui::state::{
    AppState, DiffLine, Line, Mode, PaneState, Panel, PendingView, PickerPurpose, PickerState,
    Split, TailKind, diff_lines,
};

/// Preferred rail width; shrinks on narrow terminals. Wide enough for a row
/// plus its expanded `└ y·a·d risk · title` line to stay readable.
const RAIL_WIDTH: u16 = 28;

/// Sessions-panel (left sidebar) width: a binary name + a dim model line.
const SESSIONS_WIDTH: u16 = 24;

/// Below this terminal width the sessions sidebar auto-collapses so the
/// center PTY keeps a usable column count; below `RAIL_MIN_WIDTH` the
/// subagents sidebar folds too (the title badge + mode bar still signal).
const SESSIONS_MIN_WIDTH: u16 = 110;
const RAIL_MIN_WIDTH: u16 = 70;

/// A PTY pane's rendered grid for this frame, produced loop-side from its
/// terminal backend (state stays pure — the emulator lives with the loop).
pub struct PtyView {
    pub record_id: String,
    pub lines: Vec<TuiLine<'static>>,
}

/// Render the whole app for one frame. Takes `&mut` so panes can record the
/// viewport height they were drawn at (ratatui stateful-render idiom) — the
/// reducer uses it to page the scrollback by exactly one screen.
pub fn render(state: &mut AppState, pty: &[PtyView], frame: &mut Frame) {
    let area = frame.area();
    // The composer grows with its (Shift-Enter) newlines, up to 5 rows.
    let input_lines = if state.mode == Mode::Broadcast {
        state.broadcast_input.split('\n').count()
    } else {
        state.input.split('\n').count()
    }
    .clamp(1, 5) as u16;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),                  // rail + detail
            Constraint::Length(input_lines + 2), // composer (+ borders)
            Constraint::Length(1),               // mode bar
        ])
        .split(area);
    // Sidebars (sessions left, subagents right) are collapsible — by the
    // user (palette toggles) and automatically on narrow terminals, so the
    // center PTY keeps a usable column count.
    let show_sessions = !state.sessions_collapsed && area.width >= SESSIONS_MIN_WIDTH;
    let show_rail = !state.subagents_collapsed && area.width >= RAIL_MIN_WIDTH;
    // Narrow terminals get a proportional rail instead of a fixed one.
    let rail_w = RAIL_WIDTH.min(rows[0].width / 3);
    let mut constraints = Vec::new();
    if show_sessions {
        constraints.push(Constraint::Length(SESSIONS_WIDTH));
    }
    constraints.push(Constraint::Min(1));
    if show_rail {
        constraints.push(Constraint::Length(rail_w));
    }
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(rows[0]);

    let mut col = 0;
    if show_sessions {
        render_sessions(state, frame, cols[col]);
        col += 1;
    }
    render_detail(state, pty, frame, cols[col]);
    if show_rail {
        render_rail(state, frame, cols[col + 1]);
    }
    render_input(state, frame, rows[1]);
    render_modebar(state, frame, rows[2]);

    if state.mode == Mode::Picker
        && let Some(picker) = &state.picker
    {
        render_picker(picker, state.no_color, frame, area);
    }

    if state.mode == Mode::Command
        && let Some(palette) = &state.palette
    {
        render_palette(palette, state.no_color, frame, area);
    }

    if state.mode == Mode::Confirm {
        render_confirm(state, frame, area);
    }

    if state.keys_help {
        render_keys_help(state.mode, state.no_color, frame, area);
    }

    if let Some(pane) = state.focused()
        && let Some(pending) = &pane.pending
    {
        render_permission(pending, state.no_color, frame, area);
    }
}

/// Command palette: a filter line over the fuzzy-matched command list.
fn render_palette(
    palette: &crate::tui::state::PaletteState,
    nc: bool,
    frame: &mut Frame,
    area: Rect,
) {
    let popup = centered(area, 50, 50);
    frame.render_widget(Clear, popup);
    let mut lines: Vec<TuiLine> = vec![TuiLine::from(vec![
        Span::styled(": ", tint(nc, Color::Cyan)),
        Span::raw(palette.input.clone()),
        Span::styled("▏", tint(nc, Color::Cyan)),
    ])];
    let matches = palette.matches();
    if matches.is_empty() {
        lines.push(TuiLine::styled(
            "(no matching command)",
            tint(nc, Color::DarkGray),
        ));
    }
    for (i, (name, _)) in matches.iter().enumerate() {
        if i == palette.selected.min(matches.len() - 1) {
            lines.push(TuiLine::styled(format!("> {name}"), tint(nc, Color::Cyan)));
        } else {
            lines.push(TuiLine::raw(format!("  {name}")));
        }
    }
    let para =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" command "));
    frame.render_widget(para, popup);
}

/// Bootstrap-approval overlay: the hook executes shell on every new worktree,
/// so it is shown verbatim before the first isolated spawn each session.
fn render_confirm(state: &AppState, frame: &mut Frame, area: Rect) {
    let nc = state.no_color;
    let popup = centered(area, 70, 40);
    frame.render_widget(Clear, popup);
    let cmd = state.bootstrap_cmd.as_deref().unwrap_or_default();
    let agent = state.confirm_agent.as_deref().unwrap_or_default();
    let lines: Vec<TuiLine> = vec![
        TuiLine::raw(format!(
            "spawning {agent} into an isolated worktree — run the configured"
        )),
        TuiLine::raw("bootstrap hook in each new worktree? It executes shell:"),
        TuiLine::raw(""),
        TuiLine::styled(format!("  {cmd}"), tint(nc, Color::Yellow)),
        TuiLine::raw(""),
        TuiLine::from(vec![
            Span::styled("[y]", tint(nc, Color::Green)),
            Span::raw(" run for this session   "),
            Span::styled("[n]", tint(nc, Color::Red)),
            Span::raw(" skip this session   "),
            Span::styled("[Esc]", tint(nc, Color::DarkGray)),
            Span::raw(" cancel spawn"),
        ]),
    ];
    let para = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" worktree bootstrap "),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(para, popup);
}

/// Which-key overlay: every binding for the current mode. Any key dismisses.
fn render_keys_help(mode: Mode, nc: bool, frame: &mut Frame, area: Rect) {
    let bindings: &[(&str, &str)] = match mode {
        Mode::Normal | Mode::Command => &[
            ("type + Enter", "prompt the focused agent"),
            ("Shift-Enter", "newline in the composer"),
            ("y / a / n", "resolve its pending permission"),
            ("PgUp / PgDn", "scroll its scrollback"),
            (": (empty line)", "command palette"),
            ("Ctrl-A", "manager mode"),
            ("Ctrl-B", "broadcast mode"),
            ("Ctrl-C", "interrupt the focused agent"),
        ],
        Mode::Agent => &[
            ("[ / ]", "cursor to sessions / subagents panel"),
            ("j / k / ↑ / ↓", "move the panel cursor"),
            ("g", "jump to the most actionable agent"),
            ("Enter", "open cursor pane solo"),
            ("s / v", "split cursor pane in (h/v)"),
            ("u", "drop the focused slot"),
            ("Tab / ← / → / 1-4", "switch detail slot"),
            ("q", "queue focus (needs-you only)"),
            ("y / a / d", "resolve cursor pending"),
            ("D / m / p / r", "review: diff · merge · apply · reject"),
            ("t", "attach: drive the agent's native TUI"),
            ("A", "cycle autonomy tier"),
            ("n / N", "new subagent / new session"),
            ("x", "close cursor pane"),
            (":", "command palette"),
            ("Esc", "back to normal"),
        ],
        Mode::Picker => &[("↑ / ↓", "select"), ("Enter", "spawn"), ("Esc", "cancel")],
        Mode::Confirm => &[
            ("y", "run bootstrap this session"),
            ("n", "skip bootstrap this session"),
            ("Esc", "cancel the spawn"),
        ],
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
                Span::styled(format!("{key:>18}  "), tint(nc, Color::Cyan)),
                Span::raw(*what),
            ])
        })
        .collect();
    let para = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" keys "));
    frame.render_widget(para, popup);
}

/// Braille spinner frames for running agents, advanced by the UI tick.
const SPINNER: [&str; 8] = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];

/// Foreground style honoring NO_COLOR (glyphs carry the meaning either way).
fn tint(no_color: bool, color: Color) -> Style {
    if no_color {
        Style::default()
    } else {
        Style::default().fg(color)
    }
}

/// State glyph + color for one agent, shared by the roster and the radar.
/// Never color-alone: each state has a distinct glyph.
fn state_glyph(pane: &PaneState, tick: u64) -> (&'static str, Color) {
    if pane.pending.is_some() {
        ("⚠", Color::Red) // needs you
    } else if pane.review.is_some() && !pane.exited {
        ("◆", Color::Blue) // ready to review
    } else if pane.attention {
        ("●", Color::Yellow) // went wrong in the background
    } else if pane.done && !pane.exited {
        ("◉", Color::Magenta) // finished, unseen — decays to ○ on view
    } else if !pane.exited && pane.turn_active {
        (SPINNER[(tick % 8) as usize], Color::Cyan) // working (turn in flight)
    } else if !pane.exited {
        ("○", Color::Green) // idle
    } else {
        ("✗", Color::DarkGray) // dead
    }
}

/// Human word for a pane's current state — the dim metadata line under each
/// panel entry (mirrors `state_glyph`'s order; never color-alone).
fn state_word(pane: &PaneState) -> &'static str {
    if pane.pending.is_some() {
        "needs you"
    } else if pane.review.is_some() && !pane.exited {
        "review"
    } else if pane.attention {
        "attention"
    } else if pane.done && !pane.exited {
        "done"
    } else if !pane.exited && pane.turn_active {
        "working"
    } else if !pane.exited {
        "idle"
    } else {
        "exited"
    }
}

/// Dim lowercase section header — the herdr-minimal panel chrome.
fn header_line(text: String, nc: bool) -> TuiLine<'static> {
    TuiLine::styled(text, tint(nc, Color::DarkGray).add_modifier(Modifier::BOLD))
}

/// Left sidebar: orchestrator sessions (PTY panes), herdr-spaces style — a
/// name line over a dim `state · model` line per entry. The cursor (`▸`)
/// shows when AGENT-mode panel focus is here (`[`).
fn render_sessions(state: &AppState, frame: &mut Frame, area: Rect) {
    let nc = state.no_color;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let order = state.sessions_list();
    let cursor_active = state.mode == Mode::Agent && state.panel == Panel::Sessions;
    let mut lines: Vec<TuiLine> = vec![header_line("sessions".to_string(), nc)];
    for (row, &idx) in order.iter().enumerate() {
        let pane = &state.agents[idx];
        let (glyph, color) = state_glyph(pane, state.tick);
        let at_cursor = cursor_active && row == state.session_cursor;
        let cursor = if at_cursor { "▸" } else { " " };
        let shown = state.detail.shown.iter().any(|r| r == &pane.record_id);
        let mut name_style = Style::default();
        if shown {
            name_style = name_style.add_modifier(Modifier::BOLD);
        }
        lines.push(TuiLine::raw(""));
        lines.push(TuiLine::from(vec![
            Span::raw(cursor.to_string()),
            Span::styled(glyph.to_string(), tint(nc, color)),
            Span::raw(" "),
            Span::styled(pane.agent_id.clone(), name_style),
        ]));
        // Which model this session's traffic is pinned to (the glyph already
        // carries its state; a long `provider:model` id needs every column).
        let meta = if pane.harness == "attach" {
            "attach".to_string()
        } else {
            pane.model.as_deref().unwrap_or("default model").to_string()
        };
        lines.push(TuiLine::from(vec![
            Span::raw("   "),
            Span::styled(meta, tint(nc, Color::DarkGray)),
        ]));
    }
    if order.is_empty() {
        lines.push(TuiLine::raw(""));
        lines.push(TuiLine::styled("(no sessions)", tint(nc, Color::DarkGray)));
    }
    let para = Paragraph::new(lines).block(Block::default().borders(Borders::RIGHT));
    frame.render_widget(para, chunks[0]);
    // The `new` affordance, keyboard-flavored.
    let footer = Paragraph::new(TuiLine::styled("N new session", tint(nc, Color::DarkGray)))
        .block(Block::default().borders(Borders::RIGHT));
    frame.render_widget(footer, chunks[1]);
}

/// Right rail: the subagents roster (every ACP agent, sorted by
/// actionability) over a radar strip — a name line over a dim
/// `state · harness` line per entry, herdr-minimal. The rail cursor (`▸`)
/// shows in AGENT (panel focus here) and BROADCAST modes.
fn render_rail(state: &AppState, frame: &mut Frame, area: Rect) {
    let nc = state.no_color;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let order = state.roster();
    let cursor_active = (state.mode == Mode::Agent && state.panel == Panel::Subagents)
        || state.mode == Mode::Broadcast;
    let header = if state.queue_only {
        format!("needs you · {}", order.len())
    } else {
        "subagents".to_string()
    };
    let mut lines: Vec<TuiLine> = vec![header_line(header, nc)];
    for (row, &idx) in order.iter().enumerate() {
        let pane = &state.agents[idx];
        let (glyph, color) = state_glyph(pane, state.tick);
        let at_cursor = cursor_active && row == state.rail_cursor;
        let cursor = if at_cursor { "▸" } else { " " };
        let shown = state.detail.shown.iter().any(|r| r == &pane.record_id);
        let mut name_style = Style::default();
        if shown {
            name_style = name_style.add_modifier(Modifier::BOLD);
        }
        let mut spans = vec![
            Span::raw(cursor.to_string()),
            Span::styled(glyph.to_string(), tint(nc, color)),
            Span::raw(" "),
            Span::styled(pane.agent_id.clone(), name_style),
        ];
        if pane.selected {
            spans.push(Span::styled(" ✓", tint(nc, Color::Green)));
        }
        lines.push(TuiLine::raw(""));
        lines.push(TuiLine::from(spans));
        // Dim metadata line: state word, harness tag, then the quantitative
        // extras (time-in-state, dev-server port, metered cost, autonomy).
        let mut meta = vec![Span::raw("   ")];
        let mut words = vec![state_word(pane).to_string()];
        if !pane.harness.is_empty() {
            words.push(pane.harness.clone());
        }
        // Time-in-state: how long it has been working/blocked/done — the
        // "is it stuck?" signal. Idle and dead rows stay calm.
        if let Some(elapsed) = pane.elapsed_label(state.tick) {
            words.push(elapsed);
        }
        // The fleet-allocated dev-server port, so N servers stay tellable apart.
        if let Some(port) = pane.port {
            words.push(format!(":{port}"));
        }
        // Cumulative cost, when the upstream meters it.
        if let Some(cost) = &pane.cost {
            words.push(fmt_cost(cost).trim_start().to_string());
        }
        // Non-default autonomy is worth knowing at a glance.
        match pane.autonomy {
            crate::tui::state::Autonomy::Manual => {}
            crate::tui::state::Autonomy::Assisted => words.push("[a]".to_string()),
            crate::tui::state::Autonomy::Auto => words.push("[A]".to_string()),
        }
        meta.push(Span::styled(words.join(" · "), tint(nc, Color::DarkGray)));
        lines.push(TuiLine::from(meta));
        // Actionable rows expand inline: risk + what the agent wants, and (on
        // the cursor row in AGENT mode) the resolve keys.
        if let Some(pending) = &pane.pending {
            let risk_span = match pending.risk {
                crate::risk::Risk::High => {
                    Span::styled("high · ", tint(nc, Color::Red).add_modifier(Modifier::BOLD))
                }
                crate::risk::Risk::Low => Span::styled("low · ", tint(nc, Color::DarkGray)),
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
            detail.push(Span::styled(pending.title.clone(), tint(nc, Color::Red)));
            lines.push(TuiLine::from(detail));
        } else if let Some((files, adds, dels)) = pane.review {
            // Ready-to-review rows expand with the diff stat and (on the
            // cursor row in AGENT mode) the integration keys.
            let mut detail = vec![Span::raw(" └ ")];
            if at_cursor && state.mode == Mode::Agent {
                detail.push(Span::styled(
                    "m·p·D·r ",
                    Style::default().add_modifier(Modifier::BOLD),
                ));
            }
            detail.push(Span::styled(
                format!("review · {files}f "),
                tint(nc, Color::Blue),
            ));
            detail.push(Span::styled(format!("+{adds}"), tint(nc, Color::Green)));
            detail.push(Span::raw("/"));
            detail.push(Span::styled(format!("-{dels}"), tint(nc, Color::Red)));
            lines.push(TuiLine::from(detail));
        }
    }
    if order.is_empty() {
        lines.push(TuiLine::raw(""));
        if state.queue_only {
            lines.push(TuiLine::styled("✓ all clear", tint(nc, Color::Green)));
        } else {
            lines.push(TuiLine::styled("(no subagents)", tint(nc, Color::DarkGray)));
        }
    }
    let roster = Paragraph::new(lines).block(Block::default().borders(Borders::LEFT));
    frame.render_widget(roster, chunks[0]);

    // Radar: one glyph per agent in roster order — peripheral vision of every
    // agent's state even while the detail is zoomed into one.
    let radar: Vec<Span> = order
        .iter()
        .map(|&idx| {
            let (glyph, color) = state_glyph(&state.agents[idx], state.tick);
            Span::styled(glyph.to_string(), tint(nc, color))
        })
        .collect();
    frame.render_widget(
        Paragraph::new(TuiLine::from(radar)).block(Block::default().borders(Borders::LEFT)),
        chunks[1],
    );
}

/// Detail viewport: the shown agents in a horizontal or vertical split.
fn render_detail(state: &mut AppState, pty: &[PtyView], frame: &mut Frame, area: Rect) {
    let nc = state.no_color;
    let shown = state.detail.shown.clone();
    let focus = state.detail.focus;
    let split = state.detail.split;
    if shown.is_empty() {
        let placeholder = Paragraph::new("no agent shown — Ctrl-A then n to spawn")
            .style(tint(nc, Color::DarkGray))
            .block(Block::default().borders(Borders::ALL));
        frame.render_widget(placeholder, area);
        return;
    }
    let rects = split_rects(area, shown.len(), split);
    let mut pty_areas = Vec::new();
    for (slot, (rid, rect)) in shown.iter().zip(rects.iter()).enumerate() {
        if let Some(pane) = state.agents.iter_mut().find(|p| &p.record_id == rid) {
            match pane.kind {
                crate::tui::state::PaneKind::Pty => {
                    // Record the drawn inner size so the loop can resize the
                    // emulator + PTY (SIGWINCH) when the layout changes.
                    pty_areas.push((
                        rid.clone(),
                        (rect.width.saturating_sub(2), rect.height.saturating_sub(2)),
                    ));
                    let view = pty.iter().find(|v| &v.record_id == rid);
                    render_pty_pane(pane, view, slot, slot == focus, nc, frame, *rect);
                }
                crate::tui::state::PaneKind::Acp => {
                    render_pane(pane, slot, slot == focus, nc, frame, *rect)
                }
            }
        }
    }
    state.pty_areas = pty_areas;
}

/// Render a PTY pane: the emulator's grid verbatim inside the pane border —
/// the harness renders itself; bitrouter draws no lines of its own here.
fn render_pty_pane(
    pane: &mut PaneState,
    view: Option<&PtyView>,
    slot: usize,
    focused: bool,
    nc: bool,
    frame: &mut Frame,
    area: Rect,
) {
    pane.viewport = area.height.saturating_sub(2) as usize;
    let mut markers = String::new();
    if pane.exited {
        markers.push_str(" ✗");
    }
    let title = format!(
        " [{}] {} · {}{} ",
        slot + 1,
        pane.agent_id,
        pane.harness,
        markers
    );
    let border_style = if focused {
        tint(nc, Color::Cyan)
    } else {
        Style::default()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);
    let lines: Vec<TuiLine> = match view {
        Some(v) => v.lines.clone(),
        None => vec![TuiLine::styled(
            "(starting…)",
            tint(nc, Color::DarkGray).add_modifier(Modifier::ITALIC),
        )],
    };
    // No wrap: the emulator already fits its grid to the pane.
    let para = Paragraph::new(lines).block(block);
    frame.render_widget(para, area);
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
fn render_pane(
    pane: &mut PaneState,
    slot: usize,
    focused: bool,
    nc: bool,
    frame: &mut Frame,
    area: Rect,
) {
    let short = pane.record_id.get(..8).unwrap_or(pane.record_id.as_str());
    let inner_height = area.height.saturating_sub(2) as usize;
    let inner_width = area.width.saturating_sub(2) as usize;
    pane.viewport = inner_height;
    // The mutable streaming tail counts as one display line after the
    // committed region.
    let extra = usize::from(pane.tail.is_some());
    let total = pane.lines.len() + extra;
    let tail_start = total.saturating_sub(inner_height);
    // A pin never scrolls past the tail view (no blank space below the tail).
    let start = pane.scroll.map(|s| s.min(tail_start)).unwrap_or(tail_start);
    let hidden_below = total - (start + inner_height).min(total);

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
    // Context-window occupancy + cumulative cost, when the upstream reports them.
    let usage = match pane.usage {
        Some((used, size)) => format!(" · {}/{}", fmt_tokens(used), fmt_tokens(size)),
        None => String::new(),
    };
    let cost = match &pane.cost {
        Some(c) => format!(" ·{}", fmt_cost(c)),
        None => String::new(),
    };
    let title = format!(
        " [{}] {}{} · {}{}{}{} ",
        slot + 1,
        pane.agent_id,
        harness,
        short,
        usage,
        cost,
        markers
    );
    let border_style = if focused {
        tint(nc, Color::Cyan)
    } else if pane.selected {
        tint(nc, Color::Green)
    } else {
        Style::default()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);
    let committed_end = pane.lines.len().min(start + inner_height);
    let mut lines: Vec<TuiLine> = pane.lines[start.min(pane.lines.len())..committed_end]
        .iter()
        .map(|l| render_line(l, nc, inner_width))
        .collect();
    // The mutable tail renders after the committed region while following.
    if let Some((kind, buf)) = &pane.tail
        && start + inner_height > pane.lines.len()
    {
        lines.push(match kind {
            TailKind::Message => TuiLine::raw(buf.clone()),
            TailKind::Thought => TuiLine::styled(buf.clone(), tint(nc, Color::DarkGray)),
        });
    }
    if lines.is_empty() && !pane.exited {
        // Calm pre-first-output placeholder, not a blank pane.
        lines.push(TuiLine::styled(
            "thinking…",
            tint(nc, Color::DarkGray).add_modifier(Modifier::ITALIC),
        ));
    }
    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

/// Compact token count for the pane header (`182300` → `182k`).
fn fmt_tokens(n: u64) -> String {
    if n >= 1000 {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
}

/// Compact cumulative cost (` $0.25`, or ` 0.25 EUR` off-dollar).
fn fmt_cost(cost: &bitrouter_substrate::translate::UsageCost) -> String {
    if cost.currency == "USD" {
        format!(" ${:.2}", cost.amount)
    } else {
        format!(" {:.2} {}", cost.amount, cost.currency)
    }
}

fn render_line(line: &Line, nc: bool, width: usize) -> TuiLine<'static> {
    use bitrouter_substrate::translate::ToolStatus;
    match line {
        Line::UserPrompt(t) => TuiLine::from(vec![
            Span::styled("› ", tint(nc, Color::Cyan)),
            Span::raw(t.clone()),
        ]),
        Line::Message(t) => TuiLine::raw(t.clone()),
        Line::Thought(t) => TuiLine::styled(t.clone(), tint(nc, Color::DarkGray)),
        Line::Code { text, lang } => TuiLine::from(crate::tui::highlight::spans(lang, text, nc)),
        Line::Tool { title, status, .. } => {
            // Status glyph, not a Debug dump — glyphs carry meaning without color.
            let (glyph, color) = match status {
                ToolStatus::Pending => ("· ", Color::DarkGray),
                ToolStatus::Running => ("⚒ ", Color::Yellow),
                ToolStatus::Ok => ("✓ ", Color::Green),
                ToolStatus::Failed => ("✗ ", Color::Red),
            };
            TuiLine::from(vec![
                Span::styled(glyph, tint(nc, color)),
                Span::raw(title.clone()),
            ])
        }
        Line::Diff(d) => render_diff_line(d, nc, width),
        Line::Error(t) => TuiLine::from(vec![
            Span::styled("✗ ", tint(nc, Color::Red)),
            Span::styled(t.clone(), tint(nc, Color::Red)),
        ]),
        Line::AutoResolved(t) => TuiLine::from(vec![
            Span::styled("· ", tint(nc, Color::DarkGray)),
            Span::styled(t.clone(), tint(nc, Color::DarkGray)),
        ]),
        Line::Note(t) => TuiLine::from(vec![
            Span::styled("· ", tint(nc, Color::DarkGray)),
            Span::styled(t.clone(), tint(nc, Color::DarkGray)),
        ]),
    }
}

/// Background tint for added lines (kept dark so syntax fg stays readable).
const ADD_BG: Color = Color::Rgb(16, 48, 16);
/// Background tint for deleted lines.
const DEL_BG: Color = Color::Rgb(48, 16, 16);

/// The `diff_render` treatment: `+`/`-` prefixed lines with a full-width
/// background tint (padded to `width`), dimmed deletions, a `⋮` gap between
/// hunks, and a `path +N/-M` header with count chips.
fn render_diff_line(d: &DiffLine, nc: bool, width: usize) -> TuiLine<'static> {
    let pad = |s: &str| {
        let mut out = s.to_string();
        while out.chars().count() < width {
            out.push(' ');
        }
        out
    };
    match d {
        DiffLine::Header { path, adds, dels } => TuiLine::from(vec![
            Span::styled(path.clone(), Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled(format!("+{adds}"), tint(nc, Color::Green)),
            Span::raw("/"),
            Span::styled(format!("-{dels}"), tint(nc, Color::Red)),
        ]),
        DiffLine::Add(t) => {
            let style = if nc {
                Style::default()
            } else {
                Style::default().fg(Color::Green).bg(ADD_BG)
            };
            TuiLine::styled(pad(&format!("+{t}")), style)
        }
        DiffLine::Del(t) => {
            let style = if nc {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
                    .fg(Color::Red)
                    .bg(DEL_BG)
                    .add_modifier(Modifier::DIM)
            };
            TuiLine::styled(pad(&format!("-{t}")), style)
        }
        DiffLine::Ctx(t) => TuiLine::styled(format!(" {t}"), tint(nc, Color::DarkGray)),
        DiffLine::Gap => TuiLine::styled("⋮", tint(nc, Color::DarkGray)),
    }
}

fn render_input(state: &AppState, frame: &mut Frame, area: Rect) {
    // A focused PTY pane owns the keyboard (locked-mode passthrough) — the
    // manager's prompt line would be a lie, so show the routing instead.
    if state.mode == Mode::Normal
        && state
            .focused()
            .is_some_and(|p| p.kind == crate::tui::state::PaneKind::Pty)
    {
        let para = Paragraph::new("⇢ keys go to the orchestrator · Ctrl-A for the manager")
            .style(tint(state.no_color, Color::DarkGray))
            .block(Block::default().borders(Borders::ALL));
        frame.render_widget(para, area);
        return;
    }
    let (prefix, text) = if state.mode == Mode::Broadcast {
        ("⇉ ", state.broadcast_input.as_str())
    } else {
        ("› ", state.input.as_str())
    };
    // Multiline composer: the prefix marks the first line; continuation
    // lines indent under it.
    let lines: Vec<TuiLine> = text
        .split('\n')
        .enumerate()
        .map(|(i, l)| {
            if i == 0 {
                TuiLine::raw(format!("{prefix}{l}"))
            } else {
                TuiLine::raw(format!("  {l}"))
            }
        })
        .collect();
    let para = Paragraph::new(lines).block(Block::default().borders(Borders::ALL));
    frame.render_widget(para, area);
}

fn render_modebar(state: &AppState, frame: &mut Frame, area: Rect) {
    let hints = match state.mode {
        Mode::Normal => {
            "NORMAL  ^a manage · ^b broadcast · : cmd · PgUp/PgDn scroll · ^c interrupt agent"
        }
        Mode::Agent => {
            "AGENT  j/k · Enter open · s/v split · q queue · y/a/d · D/m/p/r review · t attach · A tier · n new · x close · ? keys · Esc"
        }
        Mode::Picker => "PICKER  up/down select · Enter spawn · Esc",
        Mode::Broadcast => "BROADCAST  Space/1-9 select · a all · Enter send · Esc",
        Mode::Command => "COMMAND  type to filter · up/down select · Enter run · Esc",
        Mode::Confirm => "CONFIRM  y run bootstrap · n skip · Esc cancel spawn",
    };
    let text = match &state.notice {
        Some(n) => format!("{hints}   ! {n}"),
        None => hints.to_string(),
    };
    frame.render_widget(Paragraph::new(text), area);
}

fn render_picker(picker: &PickerState, nc: bool, frame: &mut Frame, area: Rect) {
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
                    TuiLine::styled(format!("> {a}"), tint(nc, Color::Cyan))
                } else {
                    TuiLine::raw(format!("  {a}"))
                }
            })
            .collect()
    };
    let title = match picker.purpose {
        PickerPurpose::Subagent => " pick agent ",
        PickerPurpose::Session => " new session ",
    };
    let para = Paragraph::new(items).block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(para, popup);
}

fn render_permission(pending: &PendingView, nc: bool, frame: &mut Frame, area: Rect) {
    let popup = centered(area, 70, 40);
    frame.render_widget(Clear, popup);
    let width = popup.width.saturating_sub(2) as usize;
    let mut lines: Vec<TuiLine> = vec![TuiLine::raw(pending.title.clone())];
    if let Some(diff) = &pending.diff {
        // Same diff_render treatment as the scrollback, not raw text.
        for l in diff_lines(diff) {
            lines.push(render_line(&l, nc, width));
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
    use crate::tui::state::{
        AppState, DetailLayout, Line, Mode, PaneState, PickerPurpose, PickerState,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    fn draw(state: &mut AppState, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| render(state, &[], f)).expect("draw");
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
        assert!(text.contains("subagents"), "rail header");
    }

    /// agents3 plus a PTY orchestrator session pinned to a model.
    fn with_session() -> AppState {
        let mut st = agents3();
        let mut orch = PaneState::new("orchestrator".into(), "claude".into());
        orch.kind = crate::tui::state::PaneKind::Pty;
        orch.harness = "pty".into();
        orch.model = Some("supergrok:grok-4.5".into());
        st.agents.push(orch);
        st
    }

    #[test]
    fn sessions_panel_shows_binary_model_and_footer_on_wide_terminals() {
        let text = draw(&mut with_session(), 130, 30);
        assert!(text.contains("sessions"), "left header");
        assert!(text.contains("claude"), "session binary");
        assert!(text.contains("supergrok:grok-4.5"), "session model");
        assert!(text.contains("N new session"), "footer affordance");
    }

    #[test]
    fn sessions_panel_folds_on_narrow_terminals_and_on_collapse() {
        let mut st = with_session();
        let text = draw(&mut st, 80, 24);
        assert!(!text.contains("N new session"), "auto-collapsed at 80 cols");
        // Wide but user-collapsed: stays hidden.
        st.sessions_collapsed = true;
        let text = draw(&mut st, 130, 30);
        assert!(!text.contains("N new session"), "user collapse wins");
        // Both sidebars collapsed: the detail still renders.
        st.subagents_collapsed = true;
        let text = draw(&mut st, 130, 30);
        assert!(!text.contains("subagents"), "rail hidden");
    }

    #[test]
    fn subagent_rows_carry_a_state_and_harness_meta_line() {
        let mut st = agents3();
        st.agents[1].harness = "claude".into();
        st.agents[1].turn_active = true;
        let text = draw(&mut st, 80, 24);
        assert!(text.contains("working · claude"), "meta line: {text}");
        assert!(text.contains("idle"), "calm rows say idle");
    }

    #[test]
    fn rail_sorts_actionable_agent_to_the_top() {
        let mut st = agents3();
        st.agents[2].pending = Some(crate::tui::state::PendingView {
            title: "WRITE".into(),
            diff: None,
            options: vec![],
            risk: crate::risk::Risk::High,
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
    fn rail_shows_done_unseen_glyph() {
        let mut st = agents3();
        st.agents[1].done = true;
        let text = draw(&mut st, 80, 24);
        assert!(text.contains('◉'), "done-unseen glyph rendered: {text:?}");
    }

    #[test]
    fn rail_shows_time_in_state_for_working_rows() {
        let mut st = agents3();
        st.agents[0].turn_active = true;
        // Stamp the bucket, then advance 42s of ticks (5/sec).
        crate::tui::state::reduce(&mut st, &crate::tui::event::AppEvent::Tick);
        st.tick += 42 * 5;
        let text = draw(&mut st, 80, 24);
        assert!(text.contains("42s"), "elapsed column rendered: {text:?}");
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
            purpose: PickerPurpose::Subagent,
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
            risk: crate::risk::Risk::High,
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
            risk: crate::risk::Risk::High,
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
    fn streaming_tail_renders_after_committed_lines() {
        let mut st = AppState::new(PaneState::new("r0".into(), "a0".into()));
        st.agents[0].lines.push(Line::Message("committed".into()));
        st.agents[0].tail = Some((TailKind::Message, "half-formed".into()));
        let text = draw(&mut st, 60, 12);
        assert!(text.contains("committed"));
        assert!(text.contains("half-formed"), "mutable tail visible");
    }

    #[test]
    fn diff_lines_render_with_prefixes_and_chips() {
        let mut st = AppState::new(PaneState::new("r0".into(), "a0".into()));
        for l in crate::tui::state::diff_lines(&crate::tui::event::DiffData {
            path: "src/x.rs".into(),
            old: "old line\n".into(),
            new: "new line\n".into(),
        }) {
            st.agents[0].lines.push(l);
        }
        let text = draw(&mut st, 60, 12);
        assert!(text.contains("src/x.rs +1/-1"), "header chips: {text:?}");
        assert!(text.contains("-old line"), "deletion prefixed");
        assert!(text.contains("+new line"), "addition prefixed");
    }

    #[test]
    fn code_lines_render_their_text() {
        let mut st = AppState::new(PaneState::new("r0".into(), "a0".into()));
        st.agents[0].lines.push(Line::Code {
            text: "fn main() {}".into(),
            lang: "rust".into(),
        });
        let text = draw(&mut st, 60, 12);
        assert!(text.contains("fn main() {}"));
    }

    #[test]
    fn cost_shows_in_rail_and_pane_header() {
        let mut st = AppState::new(PaneState::new("r0".into(), "a0".into()));
        st.agents[0].cost = Some(bitrouter_substrate::translate::UsageCost {
            amount: 0.25,
            currency: "USD".into(),
        });
        let text = draw(&mut st, 80, 24);
        assert!(text.contains("$0.25"), "cost column rendered: {text:?}");
    }

    #[test]
    fn confirm_overlay_shows_the_bootstrap_command() {
        let mut st = AppState::new(PaneState::new("r0".into(), "a0".into()));
        st.mode = Mode::Confirm;
        st.bootstrap_cmd = Some("npm ci".into());
        st.confirm_agent = Some("codex".into());
        let text = draw(&mut st, 90, 24);
        assert!(text.contains("npm ci"), "the shell it will run is visible");
        assert!(text.contains("codex"), "which spawn is waiting");
        assert!(
            text.contains("[y]") && text.contains("[Esc]"),
            "resolve keys"
        );
    }

    #[test]
    fn pty_pane_renders_the_grid_and_records_its_size() {
        let mut pane = PaneState::new("orchestrator".into(), "claude".into());
        pane.kind = crate::tui::state::PaneKind::Pty;
        pane.harness = "pty".into();
        let mut st = AppState::new(pane);
        let view = PtyView {
            record_id: "orchestrator".into(),
            lines: vec![
                ratatui::text::Line::raw("NATIVE_TUI_ROW_1"),
                ratatui::text::Line::raw("NATIVE_TUI_ROW_2"),
            ],
        };
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render(&mut st, std::slice::from_ref(&view), f))
            .expect("draw");
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("NATIVE_TUI_ROW_1"), "grid rendered: {text:?}");
        assert!(text.contains("claude · pty"), "pane header");
        assert!(
            text.contains("keys go to the orchestrator"),
            "passthrough hint replaces the prompt line"
        );
        let (rid, (cols, rows)) = st.pty_areas.first().expect("drawn size recorded");
        assert_eq!(rid, "orchestrator");
        assert!(*cols > 0 && *rows > 0);
    }

    #[test]
    fn pty_pane_without_a_view_shows_a_calm_placeholder() {
        let mut pane = PaneState::new("orchestrator".into(), "claude".into());
        pane.kind = crate::tui::state::PaneKind::Pty;
        let mut st = AppState::new(pane);
        let text = draw(&mut st, 60, 12);
        assert!(text.contains("starting…"), "{text:?}");
    }

    #[test]
    fn rail_shows_allocated_port() {
        let mut st = AppState::new(PaneState::new("r0".into(), "a0".into()));
        st.agents[0].port = Some(3101);
        let text = draw(&mut st, 80, 24);
        assert!(text.contains(":3101"), "port column rendered: {text:?}");
    }

    #[test]
    fn idle_agent_shows_idle_glyph_not_spinner() {
        let mut st = agents3(); // no turn in flight anywhere
        let text = draw(&mut st, 80, 24);
        assert!(text.contains('○'), "idle glyph");
        assert!(!text.contains('⣾'), "no spinner without a turn");
    }

    #[test]
    fn pre_first_output_pane_shows_thinking_placeholder() {
        let mut st = AppState::new(PaneState::new("r0".into(), "a0".into()));
        let text = draw(&mut st, 60, 12);
        assert!(text.contains("thinking…"), "calm placeholder, not blank");

        st.agents[0].exited = true;
        let text = draw(&mut st, 60, 12);
        assert!(!text.contains("thinking…"), "dead pane doesn't pretend");
    }

    #[test]
    fn spinner_advances_with_tick() {
        let mut st = agents3();
        st.agents[0].turn_active = true; // spinner = a turn in flight
        st.tick = 0;
        let t0 = draw(&mut st, 80, 24);
        st.tick = 1;
        let t1 = draw(&mut st, 80, 24);
        assert!(t0.contains('⣾') && !t0.contains('⣽'), "frame 0");
        assert!(t1.contains('⣽') && !t1.contains('⣾'), "frame 1");
    }

    #[test]
    fn no_color_strips_foregrounds_but_keeps_glyphs() {
        use ratatui::style::Color;
        let mut st = agents3();
        st.agents[1].attention = true;
        st.no_color = true;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| render(&mut st, &[], f)).expect("draw");
        let buffer = terminal.backend().buffer();
        let colored = buffer
            .content()
            .iter()
            .filter(|c| c.fg != Color::Reset)
            .count();
        assert_eq!(colored, 0, "NO_COLOR leaves no foreground colors");
        let text: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains('●'), "state glyphs still carry the meaning");
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
        assert!(text.contains("(no subagents)"), "rail placeholder");
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
            diff: Some(crate::tui::event::DiffData {
                path: "src/x.rs".into(),
                old: "removed\n".into(),
                new: "added\n".into(),
            }),
            options: vec![PermOption {
                outcome: PermissionOutcome::AllowOnce,
                label: "allow".into(),
            }],
            risk: crate::risk::Risk::High,
        });
        st.agents[1].attention = true;
        st.mode = Mode::Picker;
        st.picker = Some(PickerState {
            agents: vec!["alpha".into()],
            selected: 0,
            purpose: PickerPurpose::Subagent,
        });
        st.notice = Some("spawn failed".into());

        // Degenerate sizes: the spec's 20x5, plus 1-cell and 1-row/1-col
        // extremes. Passing = no panic; ratatui clamps layout.
        for (w, h) in [(1, 1), (2, 2), (5, 3), (10, 2), (20, 5), (80, 1), (1, 24)] {
            let _ = draw(&mut st, w, h);
        }
    }
}
