//! User-input reduction: keyboard handlers for every mode
//! (`reduce_key_*`), mouse clicks (`reduce_click`), command-palette
//! execution (`run_command`), and the detail-navigation helpers they call.

use crossterm::event::{KeyCode, KeyEvent};

use bitrouter_substrate::translate::PermissionOutcome;

use super::diff::Line;
use super::layout::ClickTarget;
use super::overlay::{
    Command, LeaderAction, ManagerState, Mode, PaletteState, PickerPurpose, PickerState,
    leader_action,
};
use super::pane::{Ownership, PaneKind};
use super::{AppState, REJECT_NOTE, mark_shown_seen};
use crate::tui::event::Effect;

/// NORMAL-mode keys. Permission keys take priority when a prompt is pending.
pub(super) fn reduce_key_normal(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    // The one-shot leader (TUI_SPEC_V3 §3): intercepted before PTY
    // passthrough so it never reaches the orchestrator child. It opens the
    // which-key overlay; the next key runs one leaf and returns to NORMAL.
    if (key.code, key.modifiers) == state.leader {
        state.mode = Mode::Leader;
        return Vec::new();
    }
    // A focused PTY pane is locked-mode passthrough (TUI_SPEC §9): every key
    // except the leader (handled above) routes to the child — that includes
    // `Ctrl-A`/`Ctrl-B` (readline) and the arrows the inner app drives its
    // menus with. The exception is PgUp/PgDn: the host owns pane scrollback
    // (the agent relies on the terminal to hold history), so those page the
    // emulator instead of reaching the child. Typing snaps back to the live
    // bottom loop-side — you cannot type into history.
    if let Some(pane) = state.focused()
        && pane.kind == PaneKind::Pty
    {
        if pane.exited {
            return Vec::new(); // dead child — nothing to type into
        }
        let record_id = pane.record_id.clone();
        return match key.code {
            KeyCode::PageUp => vec![Effect::PtyScroll {
                record_id,
                up: true,
                page: true,
            }],
            KeyCode::PageDown => vec![Effect::PtyScroll {
                record_id,
                up: false,
                page: true,
            }],
            _ => vec![Effect::PtyKey {
                record_id,
                key: *key,
            }],
        };
    }
    let focus_id = match state.focused() {
        Some(p) => p.record_id.clone(),
        None => return Vec::new(),
    };
    // Scrollback paging works whether or not a permission is pending, so the
    // user can read history before answering y/a/n.
    match key.code {
        KeyCode::PageUp => {
            if let Some(pane) = state.focused_mut() {
                pane.scroll_page_up();
            }
            return Vec::new();
        }
        KeyCode::PageDown => {
            if let Some(pane) = state.focused_mut() {
                pane.scroll_page_down();
            }
            return Vec::new();
        }
        _ => {}
    }
    // ── Inline decisions: `y/a/n` resolve the TOP pending decision — the
    // fleet head's, risk-sorted then oldest-first — and advance focus to the
    // next pending item (batch clear, no mode).
    //
    // NOTE this arm is unreachable while a PTY pane is focused (the
    // passthrough above returns first), which is the default screen. That is
    // exactly why the manager view owns the same verbs: it is the only
    // surface on which a blocked subagent can be unblocked while the
    // orchestrator holds the keyboard.
    // A pending decision being up does not swallow other keys: scroll is
    // handled above and the review/notice arms below still run.
    if let Some(top_id) = top_pending(state)
        && let Some(outcome) = decide_outcome(key.code)
    {
        return decide(state, &top_id, outcome, true);
    }

    // ── Inline review verbs on the focused Monitor: no mode to enter — `D`
    // loads the diff, `m` merges, `p` applies, `r` rejects. Only live when
    // the focused pane has a ready-to-review diff, so they never shadow
    // anything else. Shared verbatim with the manager view.
    if state.focused().is_some_and(|p| p.review.is_some())
        && let Some(effects) = review_verb(state, &focus_id, key.code)
    {
        return effects;
    }

    // Monitors are read-only (TUI_SPEC_V3 I2): there is no composer and no
    // human prompt path. `:` opens the command palette; anything else that
    // would have typed lands on a notice pointing at the owner.
    match key.code {
        KeyCode::Char(':') => {
            state.palette = Some(PaletteState::default());
            state.mode = Mode::Command;
            Vec::new()
        }
        KeyCode::Char(_) | KeyCode::Enter | KeyCode::Backspace => {
            state.notice = Some(match state.focused().map(|p| p.owner) {
                Some(Ownership::Orchestrator) => {
                    "orchestrator-managed subagent — steer it from the orchestrator".to_string()
                }
                _ => format!(
                    "read-only monitor — {} t attaches to drive it directly",
                    state.leader_label()
                ),
            });
            Vec::new()
        }
        _ => Vec::new(),
    }
}

// ── Supervision verbs, shared by NORMAL (against the focused pane) and
// MANAGER (against the cursor row). One implementation, so a decision made
// from the manager and one made from a focused monitor cannot diverge.

/// The record id of the fleet's top pending decision — highest risk, then
/// oldest — or `None` when nothing is blocked.
fn top_pending(state: &AppState) -> Option<String> {
    state
        .fleet()
        .into_iter()
        .find(|&i| state.agents[i].pending.is_some())
        .map(|i| state.agents[i].record_id.clone())
}

/// Map a decision key to its ACP outcome. `y` allow once, `a` allow always,
/// `n` deny.
fn decide_outcome(code: KeyCode) -> Option<PermissionOutcome> {
    match code {
        KeyCode::Char('y') => Some(PermissionOutcome::AllowOnce),
        KeyCode::Char('a') => Some(PermissionOutcome::AllowAlways),
        KeyCode::Char('n') => Some(PermissionOutcome::Deny),
        _ => None,
    }
}

/// Resolve `record_id`'s pending decision. With `advance`, focus moves to the
/// next blocked agent so `y y y` batch-clears the queue without navigating by
/// hand — the manager passes `false` because its cursor already tracks the
/// re-sorted list.
fn decide(
    state: &mut AppState,
    record_id: &str,
    outcome: PermissionOutcome,
    advance: bool,
) -> Vec<Effect> {
    if let Some(pane) = state.pane_by_id_mut(record_id) {
        pane.pending = None;
        // Decided — nothing left to look at here.
        pane.attention = false;
        pane.done = false;
    }
    if advance && let Some(next) = top_pending(state) {
        state.focus_on(next);
    }
    vec![Effect::ResolvePermission {
        record_id: record_id.to_string(),
        outcome,
    }]
}

/// Run a review verb (`D` diff · `m` merge · `p` apply · `r` reject) against
/// `record_id`. `None` when `code` is not a review verb, so callers can fall
/// through to their own bindings.
fn review_verb(state: &mut AppState, record_id: &str, code: KeyCode) -> Option<Vec<Effect>> {
    let record_id = record_id.to_string();
    match code {
        KeyCode::Char('D') => {
            mark_shown_seen(state);
            Some(vec![Effect::LoadDiff { record_id }])
        }
        KeyCode::Char('m') => {
            if let Some(pane) = state.pane_by_id_mut(&record_id) {
                // Integrations queue one at a time in the background; the
                // outcome lands as an OpDone line.
                pane.push_external(Line::Note("merging in the background…".into()));
            }
            Some(vec![Effect::Merge { record_id }])
        }
        KeyCode::Char('p') => {
            if let Some(pane) = state.pane_by_id_mut(&record_id) {
                pane.push_external(Line::Note("applying in the background…".into()));
            }
            Some(vec![Effect::Apply { record_id }])
        }
        KeyCode::Char('r') => {
            let pane = state.pane_by_id_mut(&record_id)?;
            let (owner, exited) = (pane.owner, pane.exited);
            pane.review = None;
            mark_shown_seen(state);
            Some(match owner {
                // Orchestrator-owned but the bridge is gone: there is no
                // consumer for the verdict — dismiss the review honestly
                // instead of claiming it was routed.
                Ownership::Orchestrator if exited => {
                    state.notice = Some(
                        "orchestrator disconnected — review dismissed, no verdict sent".into(),
                    );
                    Vec::new()
                }
                // Orchestrator-owned: the verdict is the subagent's task
                // outcome, consumed by the owning orchestrator — nothing is
                // injected into any PTY or prompt.
                Ownership::Orchestrator => {
                    state.notice =
                        Some("rejected — routed to the orchestrator (changes_requested)".into());
                    vec![Effect::ReviewVerdict {
                        record_id,
                        note: REJECT_NOTE.into(),
                    }]
                }
                // Human-owned (the palette hatch): the human IS the owner, so
                // direct steering is correct here — and only here. The
                // rejection re-prompts the agent.
                Ownership::Human => {
                    if let Some(pane) = state.pane_by_id_mut(&record_id) {
                        pane.push_external(Line::Note("rejected — asked to revise".into()));
                        // New work supersedes the finished turn's state.
                        pane.turn_active = true;
                        pane.done = false;
                        pane.check_retries = 0;
                    }
                    state.notice = Some("rejected — agent asked to revise".into());
                    vec![Effect::Prompt {
                        record_id,
                        text: REJECT_NOTE.into(),
                    }]
                }
            })
        }
        _ => None,
    }
}

/// MANAGER-mode keys: the fleet list. Navigation (`↑`/`↓`, `j`/`k`, `g`/`G`),
/// `Enter` to give a row the viewport, and the supervision verbs against the
/// CURSOR row — `y`/`a`/`n` decide, `D`/`m`/`p`/`r` review, `c` close.
///
/// `n` is deny, not new-session: the decision keys are the reason this view
/// exists, and splitting `y`/`a` from `n` across two meanings would be worse
/// than moving new-session to `N`.
pub(super) fn reduce_key_manager(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    let order = state.fleet();
    if order.is_empty() {
        state.mode = Mode::Normal;
        state.manager = None;
        return Vec::new();
    }
    // Clamp rather than wrap: the list re-sorts as agents change state, and a
    // cursor left past the end must land on a real row.
    let cursor = state
        .manager
        .as_ref()
        .map_or(0, |m| m.cursor)
        .min(order.len() - 1);
    let record_id = state.agents[order[cursor]].record_id.clone();
    let set_cursor = |state: &mut AppState, next: usize| {
        state.manager = Some(ManagerState { cursor: next });
    };
    // The leader chord toggles: the key that opened the manager closes it, so
    // a glance at the fleet is one chord out and one chord back.
    if (key.code, key.modifiers) == state.leader {
        state.mode = Mode::Normal;
        state.manager = None;
        return Vec::new();
    }

    match key.code {
        KeyCode::Esc => {
            state.mode = Mode::Normal;
            state.manager = None;
            Vec::new()
        }
        KeyCode::Up | KeyCode::Char('k') => {
            set_cursor(state, cursor.saturating_sub(1));
            Vec::new()
        }
        KeyCode::Down | KeyCode::Char('j') => {
            set_cursor(state, (cursor + 1).min(order.len() - 1));
            Vec::new()
        }
        KeyCode::Home | KeyCode::Char('g') => {
            set_cursor(state, 0);
            Vec::new()
        }
        KeyCode::End | KeyCode::Char('G') => {
            set_cursor(state, order.len() - 1);
            Vec::new()
        }
        // Give the cursor row the viewport and get out of the way — the
        // manager is a place you pass through, not one you live in.
        KeyCode::Enter => {
            state.focus_on(record_id);
            state.mode = Mode::Normal;
            state.manager = None;
            Vec::new()
        }
        // Decide on the cursor row. Only when that row is actually blocked,
        // so `n` on an idle agent is inert rather than surprising.
        KeyCode::Char('y') | KeyCode::Char('a') | KeyCode::Char('n')
            if state
                .pane_by_id_mut(&record_id)
                .is_some_and(|p| p.pending.is_some()) =>
        {
            // Safe: the guard matched one of the three decision keys.
            match decide_outcome(key.code) {
                Some(outcome) => decide(state, &record_id, outcome, false),
                None => Vec::new(),
            }
        }
        // Review the cursor row. `D` opens the diff, which only makes sense
        // in the viewport — so it focuses that agent and closes the manager.
        KeyCode::Char('D')
            if state
                .pane_by_id_mut(&record_id)
                .is_some_and(|p| p.review.is_some()) =>
        {
            state.focus_on(record_id.clone());
            state.mode = Mode::Normal;
            state.manager = None;
            review_verb(state, &record_id, KeyCode::Char('D')).unwrap_or_default()
        }
        KeyCode::Char('m') | KeyCode::Char('p') | KeyCode::Char('r')
            if state
                .pane_by_id_mut(&record_id)
                .is_some_and(|p| p.review.is_some()) =>
        {
            review_verb(state, &record_id, key.code).unwrap_or_default()
        }
        // Close the cursor row (same guards as every other close surface).
        KeyCode::Char('c') => close_agent(state, &record_id),
        // New session / new subagent: shifted out of `n`'s way.
        KeyCode::Char('N') => run_command(state, Command::NewSession),
        KeyCode::Char('S') => run_command(state, Command::SpawnAgent),
        KeyCode::Char(':') => {
            state.palette = Some(PaletteState::default());
            state.mode = Mode::Command;
            state.manager = None;
            Vec::new()
        }
        KeyCode::Char('?') => {
            state.keys_help = true;
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// LEADER leaves (TUI_SPEC_V3 §3): one key, then back to NORMAL (or into
/// a `Command`/`Picker` leaf). Never sticky — every arm leaves `Leader`.
pub(super) fn reduce_key_leader(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    // One-shot: whatever happens below, the prefix is consumed.
    state.mode = Mode::Normal;
    let Some(action) = leader_action(key.code) else {
        // Esc / anything unmapped: cancel the prefix.
        return Vec::new();
    };
    match action {
        // The manager: the whole fleet, and the only place supervision verbs
        // reach an agent that is not the one on screen.
        LeaderAction::Manager => run_command(state, Command::Manager),
        // Focus session N (switch orchestrator conversation).
        LeaderAction::FocusSession(idx) => {
            match state.sessions_list().get(idx).copied() {
                Some(i) => {
                    let id = state.agents[i].record_id.clone();
                    state.focus_on(id);
                }
                None => state.notice = Some(format!("no session {}", idx + 1)),
            }
            Vec::new()
        }
        // Focus the next actionable agent (needs-you → review), cycling past
        // the currently focused one — the keyboard fast path that skips
        // opening the manager at all.
        LeaderAction::NextActionable => {
            let actionable: Vec<usize> = state
                .fleet()
                .into_iter()
                .filter(|&i| {
                    let p = &state.agents[i];
                    p.pending.is_some() || p.review.is_some()
                })
                .collect();
            if actionable.is_empty() {
                state.notice = Some("all clear — nothing actionable".into());
                return Vec::new();
            }
            let focused = state.focus.clone();
            let next = actionable
                .iter()
                .position(|&i| Some(state.agents[i].record_id.as_str()) == focused.as_deref())
                .map(|pos| actionable[(pos + 1) % actionable.len()])
                .unwrap_or(actionable[0]);
            let id = state.agents[next].record_id.clone();
            state.focus_on(id);
            Vec::new()
        }
        // New orchestrator session (harness picker).
        LeaderAction::NewSession => {
            state.picker = Some(PickerState {
                agents: state.available_sessions.clone(),
                selected: 0,
                purpose: PickerPurpose::Session,
            });
            state.mode = Mode::Picker;
            Vec::new()
        }
        // The command palette: the exhaustive rare-verb surface.
        LeaderAction::Palette => {
            state.palette = Some(PaletteState::default());
            state.mode = Mode::Command;
            Vec::new()
        }
        // Close the focused pane (attach close = detach). A *live*
        // orchestrator-owned monitor stays: another process owns that
        // session, and removing the pane would orphan its future
        // permission requests.
        LeaderAction::Close => close_focused(state),
        // Cycle the focused pane's autonomy tier. Orchestrator-owned
        // monitors keep their policy in the owning bridge — cycling here
        // would be a lie.
        LeaderAction::Autonomy => cycle_focused_autonomy(state),
        // Attach: drive the focused agent's harness natively (PTY in its
        // worktree) — the fidelity escape hatch (TUI_SPEC_V3 §2). Live
        // human-owned monitors only; sessions ARE native PTYs already.
        LeaderAction::Attach => {
            match state
                .focused()
                .filter(|p| p.kind == PaneKind::Monitor && p.owner == Ownership::Human && !p.exited)
                .map(|p| p.record_id.clone())
            {
                Some(record_id) => vec![Effect::Attach { record_id }],
                None => Vec::new(),
            }
        }
        // Keys help overlay (any key dismisses it).
        LeaderAction::KeysHelp => {
            state.keys_help = true;
            Vec::new()
        }
    }
}

/// COMMAND-mode keys: filter, select, and run a palette command.
pub(super) fn reduce_key_command(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    let palette = match state.palette.as_mut() {
        Some(p) => p,
        // Defensive: no palette → back to Normal.
        None => {
            state.mode = Mode::Normal;
            return Vec::new();
        }
    };
    match key.code {
        KeyCode::Esc => {
            state.palette = None;
            state.mode = Mode::Normal;
            Vec::new()
        }
        KeyCode::Up => {
            palette.selected = palette.selected.saturating_sub(1);
            Vec::new()
        }
        KeyCode::Down => {
            let max = palette.matches().len().saturating_sub(1);
            palette.selected = (palette.selected + 1).min(max);
            Vec::new()
        }
        KeyCode::Backspace => {
            palette.input.pop();
            palette.selected = 0;
            Vec::new()
        }
        KeyCode::Enter => {
            let cmd = palette
                .matches()
                .get(
                    palette
                        .selected
                        .min(palette.matches().len().saturating_sub(1)),
                )
                .map(|(_, c)| *c);
            state.palette = None;
            state.mode = Mode::Normal;
            match cmd {
                Some(cmd) => run_command(state, cmd),
                None => Vec::new(), // no match → just close, no panic
            }
        }
        KeyCode::Char(c) => {
            palette.input.push(c);
            palette.selected = 0;
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Hit-test a left-click against the zones the renderer recorded this frame.
/// Later-pushed zones sit on top, so the topmost match wins (`rev()`).
///
/// Every zone belongs to the manager view — the default screen is all agent,
/// so there is nothing there to click. A row click moves the cursor to it
/// rather than jumping straight to focus: one click to aim, `Enter` (or a
/// second click on the same row) to commit, so a misclick near a `y`/`n`
/// decision costs nothing. Overlays (picker / palette / confirm / which-key)
/// swallow clicks: their zones sit behind the popup, so acting on them would
/// be a click-through.
pub(super) fn reduce_click(state: &mut AppState, col: u16, row: u16) -> Vec<Effect> {
    if state.keys_help
        || matches!(
            state.mode,
            Mode::Leader | Mode::Picker | Mode::Command | Mode::Confirm
        )
    {
        return Vec::new();
    }
    let Some(target) = state
        .click_zones
        .iter()
        .rev()
        .find(|z| z.contains(col, row))
        .map(|z| z.target)
    else {
        return Vec::new();
    };
    match target {
        ClickTarget::AgentRow(i) => {
            let order = state.fleet();
            let Some(&idx) = order.get(i) else {
                return Vec::new();
            };
            let id = state.agents[idx].record_id.clone();
            let already_there = state.manager.as_ref().is_some_and(|m| m.cursor == i);
            if already_there {
                // Second click on the same row commits, like `Enter`.
                state.focus_on(id);
                state.mode = Mode::Normal;
                state.manager = None;
            } else {
                state.manager = Some(ManagerState { cursor: i });
            }
            Vec::new()
        }
        ClickTarget::NewSession => run_command(state, Command::NewSession),
    }
}

/// Execute one palette command. Every action maps onto an existing reducer
/// path — the palette is a discoverable front door, not a second behavior set.
pub(super) fn run_command(state: &mut AppState, cmd: Command) -> Vec<Effect> {
    match cmd {
        // Open the manager with the cursor on whatever most wants attention
        // (the fleet head) — the common case is "something needs me", and
        // landing on it saves the navigation.
        Command::Manager => {
            state.manager = Some(ManagerState { cursor: 0 });
            state.mode = Mode::Manager;
            Vec::new()
        }
        Command::SpawnAgent => {
            state.picker = Some(PickerState {
                agents: state.available_agents.clone(),
                selected: 0,
                purpose: PickerPurpose::Subagent,
            });
            state.mode = Mode::Picker;
            Vec::new()
        }
        Command::NewSession => {
            state.picker = Some(PickerState {
                agents: state.available_sessions.clone(),
                selected: 0,
                purpose: PickerPurpose::Session,
            });
            state.mode = Mode::Picker;
            Vec::new()
        }
        Command::CloseAgent => close_focused(state),
        Command::Autonomy => cycle_focused_autonomy(state),
        Command::KillDone => {
            let dead: Vec<String> = state
                .agents
                .iter()
                .filter(|p| p.exited)
                .map(|p| p.record_id.clone())
                .collect();
            let mut effects = Vec::new();
            for id in dead {
                effects.extend(close_agent_by_id(state, &id));
            }
            effects
        }
        Command::KeysHelp => {
            state.keys_help = true;
            Vec::new()
        }
        Command::Quit => {
            state.should_quit = true;
            vec![Effect::Quit]
        }
    }
}

/// Close the focused pane (leader `c` / palette `close agent`; attach close
/// = detach).
fn close_focused(state: &mut AppState) -> Vec<Effect> {
    match state.focus.clone() {
        Some(id) => close_agent(state, &id),
        None => Vec::new(),
    }
}

/// Close one agent, with the guard every close surface shares: a *live*
/// orchestrator-owned monitor stays, because another process owns that
/// session and removing the pane would orphan its future permission requests.
fn close_agent(state: &mut AppState, record_id: &str) -> Vec<Effect> {
    match state
        .agents
        .iter()
        .find(|p| p.record_id == record_id)
        .map(|p| (p.owner, p.exited))
    {
        Some((Ownership::Orchestrator, false)) => {
            state.notice =
                Some("orchestrator-managed subagent — close it there (close_subagent)".into());
            Vec::new()
        }
        Some(_) => close_agent_by_id(state, record_id),
        None => Vec::new(),
    }
}

/// Cycle the focused pane's autonomy tier (leader `a` / palette `autonomy
/// cycle`). Orchestrator-owned monitors keep their policy in the owning
/// bridge — cycling here would be a lie, so every surface refuses alike.
fn cycle_focused_autonomy(state: &mut AppState) -> Vec<Effect> {
    if let Some(pane) = state.focused_mut() {
        if pane.owner == Ownership::Orchestrator {
            state.notice =
                Some("orchestrator-managed subagent — its policy lives in the bridge".into());
            return Vec::new();
        }
        pane.autonomy = pane.autonomy.next();
        let label = pane.autonomy.label();
        pane.push(Line::AutoResolved(format!("autonomy set to {label}")));
    }
    Vec::new()
}

/// Close one agent by id: remove it, hand the viewport to the most actionable
/// survivor if it was the one on screen, emit `CloseAgent`. Closing the last
/// agent quits.
fn close_agent_by_id(state: &mut AppState, record_id: &str) -> Vec<Effect> {
    if !state.agents.iter().any(|p| p.record_id == record_id) {
        return Vec::new();
    }
    state.agents.retain(|p| p.record_id != record_id);
    if state.focus.as_deref() == Some(record_id) {
        state.focus = None;
    }
    if state.agents.is_empty() {
        state.should_quit = true;
    } else if state.focus.is_none() {
        // Refill with the fleet head — the most actionable survivor.
        if let Some(head) = state.fleet().into_iter().next() {
            let id = state.agents[head].record_id.clone();
            state.focus_on(id);
        }
    }
    // The manager's cursor indexes a list that just got shorter; clamping
    // here keeps a close from leaving it pointing past the end.
    if let Some(manager) = state.manager.as_mut() {
        manager.cursor = manager.cursor.min(state.agents.len().saturating_sub(1));
    }
    vec![Effect::CloseAgent {
        record_id: record_id.to_string(),
    }]
}

/// PICKER-mode keys: navigate + choose an agent to spawn.
pub(super) fn reduce_key_picker(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    let picker = match state.picker.as_mut() {
        Some(p) => p,
        // Defensive: no active picker → just return to Normal.
        None => {
            state.mode = Mode::Normal;
            return Vec::new();
        }
    };
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            picker.selected = picker.selected.saturating_sub(1);
            Vec::new()
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if !picker.agents.is_empty() {
                picker.selected = (picker.selected + 1).min(picker.agents.len() - 1);
            }
            Vec::new()
        }
        KeyCode::Enter => {
            let selected = picker.agents.get(picker.selected).cloned();
            let purpose = picker.purpose;
            state.picker = None;
            state.mode = Mode::Normal;
            match (purpose, selected) {
                (PickerPurpose::Subagent, Some(agent_id)) => request_spawn(state, agent_id),
                (PickerPurpose::Session, Some(binary)) => vec![Effect::SpawnSession { binary }],
                (_, None) => Vec::new(), // empty picker → just close, no spawn
            }
        }
        KeyCode::Esc => {
            state.picker = None;
            state.mode = Mode::Normal;
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Emit the spawn — unless a configured bootstrap hook hasn't been shown to
/// the human yet this session (it executes shell on worktree creation), in
/// which case the CONFIRM overlay asks first and the spawn waits.
pub(super) fn request_spawn(state: &mut AppState, agent_id: String) -> Vec<Effect> {
    if state.bootstrap_cmd.is_some() && state.bootstrap_decision.is_none() {
        state.confirm_agent = Some(agent_id);
        state.mode = Mode::Confirm;
        return Vec::new();
    }
    // The launch runs in the background (worktree + bootstrap can be slow);
    // the notice bridges the gap until AgentSpawned/AgentSpawnFailed lands.
    state.notice = Some(format!("spawning {agent_id}…"));
    vec![Effect::SpawnAgent { agent_id }]
}

/// CONFIRM-mode keys: decide the bootstrap hook's fate for this session,
/// then release the pending spawn. `y` = run it on every new worktree,
/// `n` = skip it this session, Esc = cancel the spawn (ask again next time).
pub(super) fn reduce_key_confirm(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Char(c @ ('y' | 'n')) => {
            state.bootstrap_decision = Some(c == 'y');
            state.mode = Mode::Normal;
            let mut effects = Vec::new();
            // The approval is fleet policy: connected MCP bridges gate their
            // own bootstrap runs on it too.
            #[cfg(unix)]
            if c == 'y' {
                effects.push(Effect::BridgeBootstrapApproved);
            }
            if let Some(agent_id) = state.confirm_agent.take() {
                state.notice = Some(format!("spawning {agent_id}…"));
                effects.push(Effect::SpawnAgent { agent_id });
            }
            effects
        }
        KeyCode::Esc => {
            state.confirm_agent = None;
            state.mode = Mode::Normal;
            Vec::new()
        }
        _ => Vec::new(),
    }
}
