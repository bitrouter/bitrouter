//! ratatui rendering of `AppState`.
//!
//! Two screens. The DEFAULT screen is the focused agent at full terminal
//! width over a one-line status bar — no sidebars, no rails, no borrowed
//! columns. Harness TUIs draw their own chrome and wrap against the grid they
//! are given, so every column BitRouter takes for itself is a column the
//! agent's own layout degrades by.
//!
//! The MANAGER screen (leader `m`) is a full-screen overlay holding the one
//! thing the sidebars were genuinely for: the fleet, and the verbs that
//! unblock it.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TuiLine, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

use crossterm::event::KeyCode;

use crate::tui::state::AppState;
use crate::tui::state::diff::{DiffLine, Line, diff_lines};
use crate::tui::state::layout::{ClickTarget, ClickZone};
use crate::tui::state::overlay::{LEADER_LEAVES, Mode, PickerPurpose, PickerState};
use crate::tui::state::pane::{PaneState, PendingView, TailKind};

/// A PTY pane's rendered grid for this frame, produced loop-side from its
/// terminal backend (state stays pure — the emulator lives with the loop).
pub struct PtyView {
    pub record_id: String,
    pub lines: Vec<TuiLine<'static>>,
    /// The emulator view is pinned above the live tail — surface a hint so a
    /// stalled-looking pane reads as "scrolled", not "hung".
    pub scrolled: bool,
}

/// Render the whole app for one frame. Takes `&mut` so panes can record the
/// viewport height they were drawn at (ratatui stateful-render idiom) — the
/// reducer uses it to page the scrollback by exactly one screen.
pub fn render(state: &mut AppState, pty: &[PtyView], frame: &mut Frame) {
    let area = frame.area();
    // The whole screen is the agent, minus one row for the status bar. There
    // is no composer — monitors are read-only and a focused PTY pane owns the
    // keyboard (locked-mode passthrough).
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    // Clickable regions for this frame, filled by the manager renderer and
    // handed to state for the `Click` reducer to hit-test (mirrors `pty_areas`).
    let mut zones: Vec<ClickZone> = Vec::new();
    render_detail(state, pty, frame, rows[0]);
    render_statusbar(state, frame, rows[1]);

    // The manager sits over everything: it is a screen, not a panel, so the
    // fleet gets the full width for names, state, and the expanded action
    // line — the thing a 28-column rail could never show without clipping.
    if state.mode == Mode::Manager {
        render_manager(state, &mut zones, frame, area);
    }
    state.click_zones = zones;

    if state.mode == Mode::Picker
        && let Some(picker) = &state.picker
    {
        render_picker(picker, frame, area);
    }

    if state.mode == Mode::Command
        && let Some(palette) = &state.palette
    {
        render_palette(palette, state.no_color, frame, area);
    }

    if state.mode == Mode::Confirm {
        render_confirm(state, frame, area);
    }

    // The which-key overlay: up while the one-shot leader prefix is armed
    // (TUI_SPEC_V3 §3), or when `?` asked for the current mode's bindings.
    if state.keys_help || state.mode == Mode::Leader {
        render_keys_help(state.mode, &state.leader_label(), frame, area);
    }

    // The focused agent's own pending decision, as a modal. Suppressed under
    // the manager, which already shows every pending inline — two renderings
    // of the same request, one of them behind the other, is worse than one.
    if state.mode != Mode::Manager
        && let Some(pane) = state.focused()
        && let Some(pending) = &pane.pending
    {
        render_permission(pending, state.no_color, frame, area);
    }
}

/// Command palette: a filter line over the fuzzy-matched command list.
fn render_palette(
    palette: &crate::tui::state::overlay::PaletteState,
    nc: bool,
    frame: &mut Frame,
    area: Rect,
) {
    let popup = centered(area, 50, 50);
    frame.render_widget(Clear, popup);
    let mut lines: Vec<TuiLine> = vec![TuiLine::from(vec![
        Span::raw(": "),
        Span::raw(palette.input.clone()),
        Span::styled("▏", Style::default().add_modifier(Modifier::BOLD)),
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
            // Monochrome: the `>` marker + bold carries selection, not hue.
            lines.push(TuiLine::styled(
                format!("> {name}"),
                Style::default().add_modifier(Modifier::BOLD),
            ));
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
        // Monochrome: the command to vet reads in bold; the y/n/Esc letters
        // carry the choice without green/red.
        TuiLine::styled(
            format!("  {cmd}"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        TuiLine::raw(""),
        TuiLine::from(vec![
            Span::styled("[y]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" run for this session   "),
            Span::styled("[n]", Style::default().add_modifier(Modifier::BOLD)),
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
fn render_keys_help(mode: Mode, leader: &str, frame: &mut Frame, area: Rect) {
    let static_rows = |rows: &[(&str, &str)]| -> Vec<(String, String)> {
        rows.iter()
            .map(|&(k, w)| (k.to_string(), w.to_string()))
            .collect()
    };
    let bindings: Vec<(String, String)> = match mode {
        Mode::Normal | Mode::Command => static_rows(&[
            (
                "y / a / n",
                "resolve the top pending decision (batch-clears)",
            ),
            ("D / m / p / r", "review: diff · merge · apply · reject"),
            ("PgUp / PgDn", "scroll the focused scrollback"),
            (":", "command palette"),
            (leader, "leader (one-shot menu)"),
            ("Ctrl-C", "interrupt the focused agent"),
        ]),
        // The manager's verbs act on the CURSOR row, not the focused pane —
        // that is the whole point of the screen.
        Mode::Manager => static_rows(&[
            ("↑ ↓ / j k", "move the cursor"),
            ("g / G", "first / last agent"),
            ("Enter", "give this agent the viewport"),
            ("y / a / n", "decide: allow once · always · deny"),
            ("D / m / p / r", "review: diff · merge · apply · reject"),
            ("c", "close this agent"),
            ("N / S", "new session · spawn subagent"),
            ("Esc", "back to the agent"),
        ]),
        // Rendered from LEADER_LEAVES — the same table the reducer
        // dispatches from — so a leaf and its help line cannot drift apart
        // (TUI_SPEC_V3 §9). Only the digit range and Esc are hand rows.
        Mode::Leader => {
            let mut rows = vec![("1-9".to_string(), "focus session N".to_string())];
            rows.extend(LEADER_LEAVES.iter().map(|(key, what, _)| {
                let label = match key {
                    KeyCode::Tab => "Tab".to_string(),
                    KeyCode::Char(c) => c.to_string(),
                    other => format!("{other:?}"),
                };
                (label, (*what).to_string())
            }));
            rows.push(("Esc".to_string(), "cancel".to_string()));
            rows
        }
        Mode::Picker => static_rows(&[("↑ / ↓", "select"), ("Enter", "spawn"), ("Esc", "cancel")]),
        Mode::Confirm => static_rows(&[
            ("y", "run bootstrap this session"),
            ("n", "skip bootstrap this session"),
            ("Esc", "cancel the spawn"),
        ]),
    };
    let popup = centered(area, 60, 60);
    frame.render_widget(Clear, popup);
    let lines: Vec<TuiLine> = bindings
        .into_iter()
        .map(|(key, what)| {
            TuiLine::from(vec![
                Span::styled(
                    format!("{key:>18}  "),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(what),
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

/// State glyph for one agent, shared by the roster and the radar. The wrapper
/// is monochrome, so shape alone carries the state — each glyph is distinct.
fn state_glyph(pane: &PaneState, tick: u64) -> &'static str {
    if pane.pending.is_some() {
        "⚠" // needs you
    } else if pane.review.is_some() && !pane.exited {
        "◆" // ready to review
    } else if pane.attention {
        "●" // went wrong in the background
    } else if pane.done && !pane.exited {
        "◉" // finished, unseen — decays to ○ on view
    } else if !pane.exited && pane.turn_active {
        SPINNER[(tick % 8) as usize] // working (turn in flight)
    } else if !pane.exited {
        "○" // idle
    } else {
        "✗" // dead
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

/// The manager screen: one list of the WHOLE fleet — orchestrator sessions
/// and ACP subagents together, sorted by who needs you — over a footer of the
/// verbs that apply to the cursor row.
///
/// This is the supervision surface. Because a focused PTY pane swallows every
/// key but the leader, an agent that is not on screen can only be seen and
/// unblocked from here.
fn render_manager(state: &AppState, zones: &mut Vec<ClickZone>, frame: &mut Frame, area: Rect) {
    let nc = state.no_color;
    frame.render_widget(Clear, area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let order = state.fleet();
    let cursor = state
        .manager
        .as_ref()
        .map_or(0, |m| m.cursor)
        .min(order.len().saturating_sub(1));
    let focused_id = state.focus.clone();
    let inner_width = chunks[0].width.saturating_sub(2) as usize;

    let needs_you = state
        .agents
        .iter()
        .filter(|p| p.pending.is_some() || p.review.is_some())
        .count();
    let summary = if needs_you > 0 {
        format!("{} agents · {needs_you} need you", state.agents.len())
    } else {
        format!("{} agents · all clear", state.agents.len())
    };

    let mut cursor_end = 0usize;
    let mut row_spans: Vec<(usize, usize)> = Vec::new();
    let mut lines: Vec<TuiLine> = Vec::new();
    for (row, &idx) in order.iter().enumerate() {
        let pane = &state.agents[idx];
        let at_cursor = row == cursor;
        let on_screen = focused_id.as_deref() == Some(pane.record_id.as_str());
        // Focus is carried by weight (the agent you are looking at reads
        // bold); the cursor by the `▸` marker. Neither leans on hue.
        let name_style = if on_screen {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let start = lines.len();
        lines.push(TuiLine::from(vec![
            Span::raw(if at_cursor { "▸" } else { " " }.to_string()),
            Span::raw(state_glyph(pane, state.tick).to_string()),
            Span::raw(" "),
            Span::styled(pane.agent_id.clone(), name_style),
            Span::styled(
                if on_screen { "  ·  on screen" } else { "" }.to_string(),
                tint(nc, Color::DarkGray),
            ),
        ]));

        // Dim metadata line: state word, how it was launched, then the
        // quantitative extras (time-in-state, dev-server port, cost,
        // autonomy). The `pty`/`acp` tag is the ONLY place the old
        // sessions-vs-subagents split survives — as a fact about the row,
        // not as a reason to keep two lists.
        let mut words = vec![state_word(pane).to_string()];
        let kind = match pane.kind {
            crate::tui::state::pane::PaneKind::Pty => "pty",
            crate::tui::state::pane::PaneKind::Monitor => "acp",
        };
        words.push(kind.to_string());
        if !pane.harness.is_empty() && pane.harness != kind {
            words.push(pane.harness.clone());
        }
        if let Some(model) = &pane.model {
            words.push(model.clone());
        }
        if let Some(elapsed) = pane.elapsed_label(state.tick) {
            words.push(elapsed);
        }
        if let Some(port) = pane.port {
            words.push(format!(":{port}"));
        }
        if let Some(cost) = &pane.cost {
            words.push(fmt_cost(cost).trim_start().to_string());
        }
        match pane.autonomy {
            crate::tui::state::pane::Autonomy::Manual => {}
            crate::tui::state::pane::Autonomy::Assisted => words.push("[a]".to_string()),
            crate::tui::state::pane::Autonomy::Auto => words.push("[A]".to_string()),
        }
        lines.push(TuiLine::from(vec![
            Span::raw("   "),
            Span::styled(words.join(" · "), tint(nc, Color::DarkGray)),
        ]));

        // Actionable rows expand: what the agent wants and the keys that
        // answer it. Full terminal width, so the request title survives
        // intact — clipping the one line that says what you are approving
        // was the rail's worst failure.
        if let Some(pending) = &pane.pending {
            let risk = match pending.risk {
                crate::risk::Risk::High => {
                    Span::styled("high", Style::default().add_modifier(Modifier::BOLD))
                }
                crate::risk::Risk::Low => Span::styled("low", tint(nc, Color::DarkGray)),
            };
            let mut detail = vec![Span::raw("   └ ")];
            if at_cursor {
                detail.push(Span::styled(
                    "y·a·n  ",
                    Style::default().add_modifier(Modifier::BOLD),
                ));
            }
            detail.push(risk);
            detail.push(Span::raw(" · "));
            detail.push(Span::raw(clip(
                &pending.title,
                inner_width.saturating_sub(24),
            )));
            lines.push(TuiLine::from(detail));
        } else if let Some((files, adds, dels)) = pane.review {
            let mut detail = vec![Span::raw("   └ ")];
            if at_cursor {
                detail.push(Span::styled(
                    "D·m·p·r  ",
                    Style::default().add_modifier(Modifier::BOLD),
                ));
            }
            detail.push(Span::raw(format!("review · {files}f +{adds}/-{dels}")));
            lines.push(TuiLine::from(detail));
        }
        lines.push(TuiLine::raw(""));
        row_spans.push((start, lines.len()));
        if at_cursor {
            cursor_end = lines.len();
        }
    }
    if order.is_empty() {
        lines.push(TuiLine::styled("(no agents)", tint(nc, Color::DarkGray)));
    }

    let scroll = scroll_to_cursor(cursor_end, chunks[0].height.saturating_sub(2));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .title(TuiLine::from(Span::styled(
            " fleet ",
            Style::default().add_modifier(Modifier::BOLD),
        )))
        .title_bottom(
            TuiLine::from(Span::styled(
                format!(" {summary} "),
                tint(nc, Color::DarkGray),
            ))
            .right_aligned(),
        );
    frame.render_widget(
        Paragraph::new(lines).block(block).scroll((scroll, 0)),
        chunks[0],
    );

    // Row click zones, offset past the block border.
    let list = Rect {
        x: chunks[0].x + 1,
        y: chunks[0].y + 1,
        width: chunks[0].width.saturating_sub(2),
        height: chunks[0].height.saturating_sub(2),
    };
    for (row, (lo, hi)) in row_spans.into_iter().enumerate() {
        if let Some((y, h)) = screen_span(list, scroll as usize, lo, hi) {
            zones.push(ClickZone {
                x: list.x,
                y,
                w: list.width,
                h,
                target: ClickTarget::AgentRow(row),
            });
        }
    }

    // The footer names the verbs. `n` is deny here, so new-session moves to
    // `N` — spelled out rather than left to be discovered.
    let footer =
        "↑↓ move · ⏎ open · y/a/n decide · D/m/p/r review · c close · N new · S spawn · esc back";
    frame.render_widget(
        Paragraph::new(TuiLine::styled(
            clip(footer, chunks[1].width as usize),
            tint(nc, Color::DarkGray),
        )),
        chunks[1],
    );
    zones.push(ClickZone {
        x: chunks[1].x,
        y: chunks[1].y,
        w: chunks[1].width,
        h: chunks[1].height,
        target: ClickTarget::NewSession,
    });
}

/// Truncate to `width` display cells with an ellipsis.
fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The viewport: the ONE focused agent, full width.
fn render_detail(state: &mut AppState, pty: &[PtyView], frame: &mut Frame, area: Rect) {
    let nc = state.no_color;
    let Some(rid) = state.focus.clone() else {
        let placeholder = Paragraph::new(format!(
            "no agent shown — {} n for a session · {} m for the fleet",
            state.leader_label(),
            state.leader_label()
        ))
        .style(tint(nc, Color::DarkGray))
        .block(Block::default().borders(Borders::ALL));
        frame.render_widget(placeholder, area);
        state.pty_areas = Vec::new();
        return;
    };
    let mut pty_areas = Vec::new();
    if let Some(pane) = state.agents.iter_mut().find(|p| p.record_id == rid) {
        match pane.kind {
            crate::tui::state::pane::PaneKind::Pty => {
                // Record the drawn content rect (inside the border) so the
                // loop can resize the emulator + PTY (SIGWINCH) on layout
                // changes and hit-test the pointer for mouse forwarding.
                pty_areas.push(crate::tui::state::layout::PtyArea {
                    record_id: rid.clone(),
                    x: area.x.saturating_add(1),
                    y: area.y.saturating_add(1),
                    cols: area.width.saturating_sub(2),
                    rows: area.height.saturating_sub(2),
                });
                let view = pty.iter().find(|v| v.record_id == rid);
                render_pty_pane(pane, view, nc, frame, area);
            }
            // Monitor panes: bitrouter-drawn lines, no PTY grid.
            crate::tui::state::pane::PaneKind::Monitor => render_pane(pane, nc, frame, area),
        }
    }
    state.pty_areas = pty_areas;
}

/// Monochrome pane frame for the agent in the viewport. There is only ever
/// one, so the border carries identity rather than focus.
fn pane_block(title: String, nc: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(tint(nc, Color::DarkGray))
        .title(TuiLine::from(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        )))
}

/// Render a PTY pane: the emulator's grid verbatim inside the pane border —
/// the harness renders itself; bitrouter draws no lines of its own here.
fn render_pty_pane(
    pane: &mut PaneState,
    view: Option<&PtyView>,
    nc: bool,
    frame: &mut Frame,
    area: Rect,
) {
    pane.viewport = area.height.saturating_sub(2) as usize;
    let mut markers = String::new();
    if pane.exited {
        markers.push_str(" ✗");
    }
    let title = format!(" {} · {}{} ", pane.agent_id, pane.harness, markers);
    let block = pane_block(title, nc);
    // Pinned into history: a right-aligned bottom hint so the frozen tail
    // reads as "scrolled" (with the way back), not "hung".
    let block = if view.is_some_and(|v| v.scrolled) {
        block.title_bottom(
            TuiLine::from(Span::styled(
                " ↑ SCROLLBACK · PgDn or type → live ",
                tint(nc, Color::Yellow),
            ))
            .right_aligned(),
        )
    } else {
        block
    };
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

/// Sidebar scroll offset keeping the cursor entry (whose last line index is
/// `cursor_end`) inside a viewport `height` rows tall — with many agents on
/// a short terminal the `▸` cursor used to walk below the fold, making j/k
/// look dead. Zero (top-anchored) while the cursor fits.
fn scroll_to_cursor(cursor_end: usize, height: u16) -> u16 {
    u16::try_from(cursor_end.saturating_sub(height as usize)).unwrap_or(u16::MAX)
}

/// Map a paragraph's logical line range `[lo, hi)` to an on-screen `(y, h)`
/// inside `area`, given the vertical `scroll` offset — so a roster row's lines
/// become a click zone. `None` when the range is fully scrolled out of view.
fn screen_span(area: Rect, scroll: usize, lo: usize, hi: usize) -> Option<(u16, u16)> {
    let top = area.y as usize;
    let vis_lo = lo.max(scroll);
    let vis_hi = hi.min(scroll + area.height as usize);
    if vis_lo >= vis_hi {
        return None;
    }
    let y = top + (vis_lo - scroll);
    Some((y as u16, (vis_hi - vis_lo) as u16))
}

/// Render one detail pane: bordered block titled
/// `[slot] agent · harness · shortid [markers]`, focused slot highlighted.
/// Shows the scrollback tail unless the pane is pinned (`scroll`), and records
/// the drawn viewport height for paging.
fn render_pane(pane: &mut PaneState, nc: bool, frame: &mut Frame, area: Rect) {
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
    // Context occupancy + cost live in the status bar's left zone
    // (TUI_SPEC_V3 §6) — the header stays identity + attention only.
    let title = format!(" {}{} · {}{} ", pane.agent_id, harness, short, markers);
    let block = pane_block(title, nc);
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
    // Wrap-aware tail-follow: long logical lines wrap to multiple rows, so
    // the slice above can overflow the viewport — which used to clip the
    // NEWEST output (the streaming tail) off the bottom while following.
    // Scroll the overflow off the top instead. Pinned views stay
    // top-anchored (paging moves in logical lines).
    let para = if pane.scroll.is_none() {
        let rows = para.line_count(area.width.saturating_sub(2));
        let overflow = rows.saturating_sub(inner_height);
        para.scroll((u16::try_from(overflow).unwrap_or(u16::MAX), 0))
    } else {
        para
    };
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

/// The global status bar — the ONLY chrome BitRouter draws on the default
/// screen, so it carries the entire peripheral-awareness budget.
///
/// Three zones on one line: the focused agent's numbers plus mode hints on
/// the left (a notice temporarily claims that zone; it decays reducer-side),
/// and global fleet state on the right — attention counts by glyph,
/// cumulative cost, the live `serve` dot. When something needs a human, the
/// left zone says which key gets you to it; with the sidebars gone, that
/// pointer is the whole discovery path to the manager.
fn render_statusbar(state: &AppState, frame: &mut Frame, area: Rect) {
    let nc = state.no_color;
    let pty_focused = state
        .focused()
        .is_some_and(|p| p.kind == crate::tui::state::pane::PaneKind::Pty);
    let leader = state.leader_label();
    // How many agents are actually waiting on a human — the number that
    // decides whether the bar nags or stays quiet.
    let waiting = state
        .agents
        .iter()
        .filter(|p| p.pending.is_some() || p.review.is_some())
        .count();
    let hints = match state.mode {
        // Something is blocked: the hint's job is to name the way out, not
        // to describe the mode. This is the only nudge toward the manager
        // the default screen gets.
        Mode::Normal if waiting > 0 => {
            let noun = if waiting == 1 { "agent" } else { "agents" };
            format!("{waiting} {noun} waiting · {leader} m to decide")
        }
        Mode::Normal if pty_focused => format!("⇢ keys go to the agent · {leader} m fleet"),
        Mode::Normal => format!("{leader} m fleet · {leader} menu"),
        Mode::Manager => "MANAGER".to_string(),
        // Overlay modes render their own affordances; the bar just names them.
        Mode::Leader => "LEADER".to_string(),
        Mode::Picker => "PICKER".to_string(),
        Mode::Command => "COMMAND".to_string(),
        Mode::Confirm => "CONFIRM".to_string(),
    };
    // The left zone follows the focused pane: context gauge + model + cost,
    // when the upstream reports them — the numbers you actually watch. A live
    // notice takes the whole zone (full width, never clipped at the edge
    // mid-word); mode hints trail the gauge.
    let left = match &state.notice {
        Some(n) => format!("! {n}"),
        None => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(pane) = state.focused() {
                match pane.usage {
                    Some((used, size)) if size > 0 => {
                        parts.push(format!("ctx {}%", used * 100 / size));
                    }
                    Some((used, _)) if used > 0 => {
                        parts.push(format!("ctx {}", fmt_tokens(used)));
                    }
                    _ => {}
                }
                if let Some(model) = &pane.model {
                    parts.push(model.clone());
                }
                if let Some(cost) = &pane.cost {
                    parts.push(fmt_cost(cost).trim_start().to_string());
                }
            }
            if parts.is_empty() {
                hints.clone()
            } else {
                format!("{}  {}", parts.join(" · "), hints)
            }
        }
    };

    // Right zone: only segments that carry information right now.
    let mut segments: Vec<String> = Vec::new();
    let badge: Vec<String> = state
        .badge_counts()
        .into_iter()
        .map(|(glyph, n)| format!("{glyph}{n}"))
        .collect();
    if !badge.is_empty() {
        segments.push(badge.join(" "));
    }
    if let Some(total) = state.total_cost() {
        segments.push(fmt_cost(&total).trim_start().to_string());
    }
    // The `serve` dot: monochrome, so the ✗ vs ● glyph carries daemon-down
    // (never color-alone) instead of the old red accent.
    let serve = match state.serve_ok {
        Some(true) => Some("serve ●"),
        Some(false) => Some("serve ✗"),
        None => None,
    };
    let mut right = segments.join(" · ");
    if serve.is_some() && !right.is_empty() {
        right.push_str(" · ");
    }
    let right_chars = right.chars().count() + serve.map_or(0, |s| s.chars().count());

    let width = area.width as usize;
    // Right-align the state zone. When both don't fit, hints yield to the
    // global state (truncated with an ellipsis); a notice keeps the whole
    // line — it's transient, and clipping it is exactly the old failure.
    let mut left = left;
    if state.notice.is_none() && right_chars > 0 && left.chars().count() + right_chars + 2 > width {
        let keep = width.saturating_sub(right_chars + 3);
        left = left.chars().take(keep).collect();
        if !left.is_empty() {
            left.push('…');
        }
    }
    let left_chars = left.chars().count();
    let mut spans: Vec<Span> = Vec::new();
    if right_chars > 0 && left_chars + right_chars + 2 <= width {
        let pad = width - left_chars - right_chars;
        spans.push(Span::raw(left));
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(right, tint(nc, Color::DarkGray)));
        if let Some(text) = serve {
            spans.push(Span::styled(text, tint(nc, Color::DarkGray)));
        }
    } else {
        // Lone-left (or an over-long notice): keep it within the bar's cells.
        if left.chars().count() > width {
            left = left.chars().take(width).collect();
        }
        spans.push(Span::raw(left));
    }
    frame.render_widget(Paragraph::new(TuiLine::from(spans)), area);
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
                    // Monochrome: `>` + bold marks the selection, not hue.
                    TuiLine::styled(
                        format!("> {a}"),
                        Style::default().add_modifier(Modifier::BOLD),
                    )
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
    use crate::tui::state::AppState;
    use crate::tui::state::diff::Line;
    use crate::tui::state::overlay::{ManagerState, Mode, PickerPurpose, PickerState};
    use crate::tui::state::pane::PaneState;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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

    /// Open the manager with the cursor at the head, then render.
    fn draw_manager(state: &mut AppState, w: u16, h: u16) -> String {
        state.mode = Mode::Manager;
        state.manager = Some(ManagerState { cursor: 0 });
        draw(state, w, h)
    }

    #[test]
    fn default_screen_is_the_agent_plus_one_status_row() {
        // The premise of the whole layout: nothing but the focused agent and
        // the bar. No sidebar headers, no roster, no second list.
        let mut st = agents3();
        let text = draw(&mut st, 80, 24);
        assert!(!text.contains("sessions"), "no sessions sidebar: {text:?}");
        assert!(!text.contains("subagents"), "no subagents rail");
        // No agent but the focused one is named: the fleet list is a screen
        // you open, not chrome you carry.
        assert!(!text.contains("a1") && !text.contains("a2"), "no roster");
        // Only the focused agent's pane is drawn — one box.
        assert_eq!(text.matches('┌').count(), 1, "exactly one pane frame");
    }

    #[test]
    fn manager_lists_every_agent_in_one_list() {
        // The merge: PTY sessions and ACP monitors in a single list, each
        // tagged by kind rather than split across two panels.
        let text = draw_manager(&mut with_session(), 100, 30);
        for name in ["a0", "a1", "a2", "claude"] {
            assert!(text.contains(name), "manager lists {name}: {text:?}");
        }
        assert!(text.contains("fleet"), "manager title");
        assert!(text.contains("pty"), "session kind tag");
        assert!(text.contains("acp"), "subagent kind tag");
    }

    #[test]
    fn manager_footer_names_the_verbs_including_shifted_new_session() {
        let text = draw_manager(&mut agents3(), 110, 24);
        assert!(text.contains("y/a/n decide"), "decision keys advertised");
        assert!(text.contains("D/m/p/r review"), "review keys advertised");
        assert!(
            text.contains("N new"),
            "new-session moved to N because n is deny: {text:?}"
        );
    }

    #[test]
    fn manager_suppresses_the_permission_modal_it_already_shows_inline() {
        let mut st = agents3();
        st.agents[0].pending = Some(crate::tui::state::pane::PendingView {
            title: "MODAL_ONLY_TITLE".into(),
            diff: None,
            options: vec![],
            risk: crate::risk::Risk::High,
        });
        st.focus = Some("r0".into());
        // Normal: the focused pane's pending is a modal.
        let text = draw(&mut st, 100, 24);
        assert!(text.contains("MODAL_ONLY_TITLE"), "modal up in NORMAL");
        let modal_boxes = text.matches('┌').count();
        // Manager: the same request appears once, in the list.
        let text = draw_manager(&mut st, 100, 24);
        assert!(text.contains("MODAL_ONLY_TITLE"), "still visible inline");
        assert!(
            text.matches('┌').count() < modal_boxes,
            "the modal is not stacked behind the manager: {text:?}"
        );
    }

    /// agents3 plus a PTY orchestrator session pinned to a model.
    fn with_session() -> AppState {
        let mut st = agents3();
        let mut orch = PaneState::new("orchestrator".into(), "claude".into());
        orch.kind = crate::tui::state::pane::PaneKind::Pty;
        orch.harness = "pty".into();
        orch.model = Some("supergrok:grok-4.5".into());
        st.agents.push(orch);
        st
    }

    /// Render `state` at `w`×`h`, keeping the state so its recorded click zones
    /// (and everything else the renderer wrote back) can be inspected.
    fn render_keeping_state(state: &mut AppState, w: u16, h: u16) {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| render(state, &[], f)).expect("draw");
    }

    #[test]
    fn the_default_screen_records_no_click_zones() {
        // Nothing to click when the whole screen is the agent — the pointer
        // belongs to the inner app.
        let mut st = with_session();
        render_keeping_state(&mut st, 130, 30);
        assert!(st.click_zones.is_empty(), "{:?}", st.click_zones);
    }

    #[test]
    fn manager_records_a_zone_per_agent_row() {
        let mut st = with_session();
        st.mode = Mode::Manager;
        st.manager = Some(ManagerState { cursor: 0 });
        render_keeping_state(&mut st, 130, 30);
        let rows = st
            .click_zones
            .iter()
            .filter(|z| matches!(z.target, ClickTarget::AgentRow(_)))
            .count();
        assert_eq!(
            rows,
            st.agents.len(),
            "one zone per agent: {:?}",
            st.click_zones
        );
    }

    #[test]
    fn clicking_a_manager_row_aims_then_a_second_click_opens_it() {
        // The renderer's zone coordinates and the reducer's hit-test must
        // agree — and a first click must only move the cursor, so a misclick
        // near a decision row costs nothing.
        let mut st = with_session();
        st.mode = Mode::Manager;
        st.manager = Some(ManagerState { cursor: 0 });
        render_keeping_state(&mut st, 130, 30);
        let target_id = st.agents[st.fleet()[1]].record_id.clone();
        let zone = st
            .click_zones
            .iter()
            .find(|z| z.target == ClickTarget::AgentRow(1))
            .copied()
            .expect("an AgentRow(1) zone");
        let (col, row) = (zone.x + zone.w / 2, zone.y + zone.h / 2);
        let click = crate::tui::event::AppEvent::Click { col, row };
        crate::tui::state::reduce(&mut st, &click);
        assert_eq!(st.manager.as_ref().map(|m| m.cursor), Some(1), "aimed");
        assert_eq!(st.mode, Mode::Manager, "still in the manager");
        crate::tui::state::reduce(&mut st, &click);
        assert_eq!(st.focus.as_deref(), Some(target_id.as_str()), "committed");
        assert_eq!(st.mode, Mode::Normal, "and got out of the way");
    }

    #[test]
    fn manager_rows_carry_the_model_and_a_new_session_footer() {
        let text = draw_manager(&mut with_session(), 130, 30);
        assert!(text.contains("claude"), "session binary");
        assert!(text.contains("supergrok:grok-4.5"), "session model");
        assert!(text.contains("N new"), "new-session affordance");
    }

    #[test]
    fn composer_never_renders() {
        let mut st = with_session();
        // A focused PTY session owns the keyboard: no composer, and the
        // routing hint lives in the status bar instead.
        st.focus = Some("orchestrator".into());
        let text = draw(&mut st, 140, 24);
        assert!(!text.contains("› "), "no idle prompt under a PTY pane");
        assert!(
            text.contains("keys go to the agent"),
            "hint moved to the status bar"
        );
        let boxes_under_pty = text.matches('┌').count();
        // A focused Monitor is read-only (TUI_SPEC_V3 I2): no composer, no
        // input-border row — the same chrome as a PTY focus, which never
        // had one.
        st.focus = Some("r0".into());
        let text = draw(&mut st, 140, 24);
        assert!(!text.contains("› "), "no composer for a Monitor pane");
        assert_eq!(
            text.matches('┌').count(),
            boxes_under_pty,
            "no extra input box appears when a Monitor takes focus"
        );
    }

    #[test]
    fn status_bar_left_zone_follows_the_focused_pane() {
        let mut st = with_session();
        // Focus the ACP monitor and report upstream numbers on it.
        st.focus = Some("r0".into());
        st.agents[0].usage = Some((62_000, 100_000));
        st.agents[0].model = Some("claude-opus-4-8".into());
        st.agents[0].cost = Some(bitrouter_substrate::translate::UsageCost {
            amount: 0.41,
            currency: "USD".into(),
        });
        let text = draw(&mut st, 140, 24);
        assert!(
            text.contains("ctx 62%"),
            "context gauge in the bar: {text:?}"
        );
        assert!(text.contains("claude-opus-4-8"), "model tag in the bar");
        assert!(text.contains("$0.41"), "pane cost in the bar");
        // A transient notice still claims the whole zone.
        st.notice = Some("previous fleet remembered".into());
        let text = draw(&mut st, 140, 24);
        assert!(text.contains("! previous fleet remembered"));
        assert!(!text.contains("ctx 62%"), "notice preempts the gauge");
    }

    #[test]
    fn status_bar_right_zone_reports_global_state() {
        let mut st = with_session();
        st.serve_ok = Some(true);
        st.agents[0].cost = Some(bitrouter_substrate::translate::UsageCost {
            amount: 0.30,
            currency: "USD".into(),
        });
        st.agents[1].cost = Some(bitrouter_substrate::translate::UsageCost {
            amount: 0.12,
            currency: "USD".into(),
        });
        st.agents[2].attention = true;
        let (w, h) = (130u16, 30u16);
        let text = draw(&mut st, w, h);
        // The bar is the frame's last row between its << / >> buttons —
        // the full-height sidebars legitimately say "session" beside it,
        // so the fold is asserted on the bar's own cells.
        let row: String = text.chars().skip((w as usize) * (h as usize - 1)).collect();
        let bar: String = match (row.find("<<"), row.rfind(">>")) {
            (Some(lo), Some(hi)) if lo < hi => row[lo..hi].to_string(),
            _ => row,
        };
        assert!(bar.contains("serve ●"), "live daemon dot: {bar}");
        assert!(!bar.contains("session"), "bare session count folded: {bar}");
        assert!(bar.contains("$0.42"), "summed fleet cost");
        assert!(bar.contains("●1"), "attention count");
        // A down daemon flips the glyph.
        st.serve_ok = Some(false);
        let text = draw(&mut st, w, h);
        assert!(text.contains("serve ✗"));
    }

    #[test]
    fn manager_rows_carry_a_state_and_harness_meta_line() {
        let mut st = agents3();
        st.agents[1].harness = "claude".into();
        st.agents[1].turn_active = true;
        let text = draw_manager(&mut st, 100, 24);
        assert!(text.contains("working · acp · claude"), "meta line: {text}");
        assert!(text.contains("idle"), "calm rows say idle");
    }

    #[test]
    fn manager_sorts_actionable_agent_to_the_top() {
        let mut st = agents3();
        st.agents[2].pending = Some(crate::tui::state::pane::PendingView {
            title: "WRITE".into(),
            diff: None,
            options: vec![],
            risk: crate::risk::Risk::High,
        });
        // Show r2's pane so the permission popup doesn't cover the rail.
        st.focus = Some("r2".into());
        let text = draw_manager(&mut st, 100, 24);
        let (a2, a0) = (text.find("a2"), text.find("a0"));
        assert!(
            a2 < a0,
            "needs-you row renders above running rows: a2={a2:?} a0={a0:?}"
        );
        assert!(text.contains('⚠'), "needs-you glyph shown");
    }

    #[test]
    fn the_viewport_shows_only_the_focused_agent() {
        let mut st = agents3();
        st.agents[1]
            .lines
            .push(Line::Message("SECOND_PANE_UNIQUE".into()));
        let text = draw(&mut st, 80, 24);
        assert!(
            !text.contains("SECOND_PANE_UNIQUE"),
            "unfocused agent content hidden"
        );
        // No slot numbers: with one pane there is no slot to name.
        assert!(!text.contains("[1]"), "no slot header: {text:?}");
        assert!(text.contains("a0"), "the focused agent is titled");
    }

    #[test]
    fn the_viewport_holds_exactly_one_agent() {
        // No splits: switching focus replaces the viewport rather than
        // subdividing it. The multiplexer the user already runs does panes.
        let mut st = agents3();
        st.agents[0]
            .lines
            .push(Line::Message("FIRST_CONTENT".into()));
        st.agents[1]
            .lines
            .push(Line::Message("SECOND_CONTENT".into()));
        st.focus = Some("r0".into());
        let text = draw(&mut st, 100, 24);
        assert!(text.contains("FIRST_CONTENT"), "focused agent drawn");
        assert!(!text.contains("SECOND_CONTENT"), "no second slot");
        st.focus = Some("r1".into());
        let text = draw(&mut st, 100, 24);
        assert!(
            text.contains("SECOND_CONTENT"),
            "focus swapped the viewport"
        );
        assert!(!text.contains("FIRST_CONTENT"), "the old agent is gone");
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
    fn manager_shows_attention_glyph_for_background_agent() {
        let mut st = agents3();
        st.agents[1].attention = true;
        let text = draw(&mut st, 80, 24);
        assert!(
            text.contains('●'),
            "attention glyph rendered in the fleet list"
        );
    }

    #[test]
    fn manager_shows_done_unseen_glyph() {
        let mut st = agents3();
        st.agents[1].done = true;
        let text = draw(&mut st, 80, 24);
        assert!(text.contains('◉'), "done-unseen glyph rendered: {text:?}");
    }

    #[test]
    fn manager_shows_time_in_state_for_working_rows() {
        let mut st = agents3();
        st.agents[0].turn_active = true;
        // Stamp the bucket, then advance 42s of ticks (5/sec).
        crate::tui::state::reduce(&mut st, &crate::tui::event::AppEvent::Tick);
        st.tick += 42 * 5;
        let text = draw_manager(&mut st, 100, 24);
        assert!(text.contains("42s"), "elapsed column rendered: {text:?}");
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
    fn manager_expands_pending_row_with_title_and_resolve_hint() {
        let mut st = agents3();
        st.agents[1].pending = Some(crate::tui::state::pane::PendingView {
            title: "rm -rf".into(),
            diff: None,
            options: vec![],
            risk: crate::risk::Risk::High,
        });
        let text = draw_manager(&mut st, 100, 24);
        assert!(text.contains("rm -rf"), "pending title inline");
        assert!(text.contains('└'), "expanded row marker");
        assert!(text.contains("y·a·n"), "resolve hint on the queue-top row");
    }

    #[test]
    fn manager_shows_risk_label_and_autonomy_tag() {
        let mut st = agents3();
        st.agents[1].pending = Some(crate::tui::state::pane::PendingView {
            title: "wants".into(),
            diff: None,
            options: vec![],
            risk: crate::risk::Risk::High,
        });
        st.agents[2].autonomy = crate::tui::state::pane::Autonomy::Auto;
        let text = draw_manager(&mut st, 100, 24);
        assert!(text.contains("high ·"), "risk label on the expanded row");
        assert!(text.contains("[A]"), "auto tier tagged on the row");
    }

    #[test]
    fn palette_popup_renders_filter_and_matches() {
        let mut st = agents3();
        st.mode = Mode::Command;
        st.palette = Some(crate::tui::state::overlay::PaletteState {
            input: "sp".into(),
            selected: 0,
        });
        let text = draw(&mut st, 80, 24);
        assert!(text.contains("spawn subagent"), "match listed");
        assert!(text.contains("> spawn subagent"), "selection marked");
    }

    #[test]
    fn palette_popup_handles_no_matches() {
        let mut st = agents3();
        st.mode = Mode::Command;
        st.palette = Some(crate::tui::state::overlay::PaletteState {
            input: "zzz".into(),
            selected: 3,
        });
        let text = draw(&mut st, 80, 24);
        assert!(text.contains("no matching command"), "empty state shown");
    }

    #[test]
    fn keys_help_popup_lists_mode_bindings() {
        let mut st = agents3();
        st.mode = Mode::Leader;
        st.keys_help = true;
        let text = draw(&mut st, 90, 30);
        // Every leader leaf's help line comes from LEADER_LEAVES (TUI_SPEC_V3
        // §9 keyboard parity) — assert the whole table renders, plus the two
        // hand rows.
        for (_, what, _) in LEADER_LEAVES {
            assert!(text.contains(what), "leader overlay lists {what:?}");
        }
        assert!(text.contains("focus session N"));
        assert!(text.contains("cancel"));
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
        for l in crate::tui::state::diff::diff_lines(&crate::tui::event::DiffData {
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
    fn cost_shows_in_the_status_bar() {
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
        pane.kind = crate::tui::state::pane::PaneKind::Pty;
        pane.harness = "pty".into();
        let mut st = AppState::new(pane);
        let view = PtyView {
            record_id: "orchestrator".into(),
            lines: vec![
                ratatui::text::Line::raw("NATIVE_TUI_ROW_1"),
                ratatui::text::Line::raw("NATIVE_TUI_ROW_2"),
            ],
            scrolled: false,
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
            text.contains("keys go to the agent"),
            "passthrough hint replaces the prompt line"
        );
        let area = st.pty_areas.first().expect("drawn size recorded");
        assert_eq!(area.record_id, "orchestrator");
        assert!(area.cols > 0 && area.rows > 0);
        assert!(
            area.x > 0 && area.y > 0,
            "content origin sits inside the border"
        );
    }

    #[test]
    fn pty_pane_shows_a_scrollback_hint_when_pinned() {
        let mut pane = PaneState::new("orchestrator".into(), "claude".into());
        pane.kind = crate::tui::state::pane::PaneKind::Pty;
        pane.harness = "pty".into();
        let mut st = AppState::new(pane);
        let view = PtyView {
            record_id: "orchestrator".into(),
            lines: vec![ratatui::text::Line::raw("OLD_HISTORY_ROW")],
            scrolled: true,
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
        assert!(
            text.contains("SCROLLBACK"),
            "a pinned view surfaces the scrollback hint: {text:?}"
        );
    }

    #[test]
    fn pty_pane_without_a_view_shows_a_calm_placeholder() {
        let mut pane = PaneState::new("orchestrator".into(), "claude".into());
        pane.kind = crate::tui::state::pane::PaneKind::Pty;
        let mut st = AppState::new(pane);
        let text = draw(&mut st, 60, 12);
        assert!(text.contains("starting…"), "{text:?}");
    }

    #[test]
    fn manager_shows_allocated_port() {
        let mut st = AppState::new(PaneState::new("r0".into(), "a0".into()));
        st.agents[0].port = Some(3101);
        let text = draw_manager(&mut st, 80, 24);
        assert!(text.contains(":3101"), "port column rendered: {text:?}");
    }

    #[test]
    fn idle_agent_shows_idle_glyph_not_spinner() {
        let mut st = agents3(); // no turn in flight anywhere
        let text = draw_manager(&mut st, 80, 24);
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
        let t0 = draw_manager(&mut st, 80, 24);
        st.tick = 1;
        let t1 = draw_manager(&mut st, 80, 24);
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
        st.focus = None;
        let text = draw(&mut st, 80, 24);
        assert!(text.contains("no agent shown"), "viewport placeholder");
        // The manager over an empty fleet says so rather than drawing blank.
        let text = draw_manager(&mut st, 80, 24);
        assert!(
            text.contains("(no agents)"),
            "manager placeholder: {text:?}"
        );
    }

    #[test]
    fn tiny_terminals_render_every_surface_without_panic() {
        use crate::tui::event::PermOption;
        use crate::tui::state::pane::PendingView;
        use bitrouter_substrate::translate::PermissionOutcome;

        // Every render surface active at once: viewport, status bar, picker
        // overlay, permission popup, notice.
        let mut st = agents3();
        st.focus = Some("r0".into());
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
