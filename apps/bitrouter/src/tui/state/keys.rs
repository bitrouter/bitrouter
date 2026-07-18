//! User-input reduction: keyboard handlers for every mode
//! (`reduce_key_*`), mouse clicks (`reduce_click`), command-palette
//! execution (`run_command`), and the detail-navigation helpers they call.

use crossterm::event::{KeyCode, KeyEvent};

use bitrouter_substrate::translate::PermissionOutcome;

use super::diff::Line;
use super::layout::{ClickTarget, DetailLayout, Split};
use super::overlay::{
    Command, LeaderAction, Mode, PaletteState, PickerPurpose, PickerState, leader_action,
};
use super::pane::{Ownership, PaneKind};
use super::{AppState, MAX_SHOWN, REJECT_NOTE, mark_shown_seen};
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
    // ── Inline decisions (TUI_SPEC_V3 §5): `y/a/n` resolve the TOP pending
    // decision — the roster head's, risk-sorted then oldest-first — and
    // advance focus to the next pending item (batch clear, no mode).
    let top_pending = state
        .roster()
        .into_iter()
        .find(|&i| state.agents[i].pending.is_some())
        .map(|i| state.agents[i].record_id.clone());
    if let Some(top_id) = top_pending {
        let outcome = match key.code {
            KeyCode::Char('y') => Some(PermissionOutcome::AllowOnce),
            KeyCode::Char('a') => Some(PermissionOutcome::AllowAlways),
            KeyCode::Char('n') => Some(PermissionOutcome::Deny),
            _ => None,
        };
        if let Some(outcome) = outcome {
            if let Some(pane) = state.pane_by_id_mut(&top_id) {
                pane.pending = None;
                // Decided — nothing left to look at here.
                pane.attention = false;
                pane.done = false;
            }
            // Advance to the next decision so `y y y` batch-clears the
            // queue without refocusing by hand.
            if let Some(next) = state
                .roster()
                .into_iter()
                .find(|&i| state.agents[i].pending.is_some())
                .map(|i| state.agents[i].record_id.clone())
            {
                state.detail = DetailLayout::solo(next);
                mark_shown_seen(state);
            }
            return vec![Effect::ResolvePermission {
                record_id: top_id,
                outcome,
            }];
        }
        // A pending decision is up: other keys still work (scroll handled
        // above; review/notice arms below), but never leak into it.
    }

    // ── Inline review verbs on the focused Monitor (TUI_SPEC_V3 §5): no
    // mode to enter — `D` loads the diff, `m` merges, `p` applies, `r`
    // rejects. Only live when the focused pane has a ready-to-review diff,
    // so they never shadow anything else.
    let review_ready = state.focused().is_some_and(|p| p.review.is_some());
    if review_ready {
        match key.code {
            KeyCode::Char('D') => {
                mark_shown_seen(state);
                return vec![Effect::LoadDiff {
                    record_id: focus_id,
                }];
            }
            KeyCode::Char('m') => {
                if let Some(pane) = state.focused_mut() {
                    // Integrations queue one at a time in the background;
                    // the outcome lands as an OpDone line.
                    pane.push_external(Line::Note("merging in the background…".into()));
                }
                return vec![Effect::Merge {
                    record_id: focus_id,
                }];
            }
            KeyCode::Char('p') => {
                if let Some(pane) = state.focused_mut() {
                    pane.push_external(Line::Note("applying in the background…".into()));
                }
                return vec![Effect::Apply {
                    record_id: focus_id,
                }];
            }
            KeyCode::Char('r') => {
                let owner = state.focused().map(|p| p.owner);
                let exited = state.focused().is_some_and(|p| p.exited);
                if let Some(pane) = state.focused_mut() {
                    pane.review = None;
                }
                mark_shown_seen(state);
                return match owner {
                    // Orchestrator-owned but the bridge is gone: there is no
                    // consumer for the verdict — dismiss the review honestly
                    // instead of claiming it was routed.
                    Some(Ownership::Orchestrator) if exited => {
                        state.notice = Some(
                            "orchestrator disconnected — review dismissed, no verdict sent".into(),
                        );
                        Vec::new()
                    }
                    // Orchestrator-owned: the verdict is the subagent's task
                    // outcome, consumed by the owning orchestrator — nothing
                    // is injected into any PTY or prompt (TUI_SPEC_V3 §5).
                    Some(Ownership::Orchestrator) => {
                        state.notice = Some(
                            "rejected — routed to the orchestrator (changes_requested)".into(),
                        );
                        vec![Effect::ReviewVerdict {
                            record_id: focus_id,
                            note: REJECT_NOTE.into(),
                        }]
                    }
                    // Human-owned (the palette hatch): the human IS the
                    // owner, so direct steering is correct here — and only
                    // here. The rejection re-prompts the agent.
                    _ => {
                        if let Some(pane) = state.focused_mut() {
                            pane.push_external(Line::Note("rejected — asked to revise".into()));
                            // New work supersedes the finished turn's state.
                            pane.turn_active = true;
                            pane.done = false;
                            pane.check_retries = 0;
                        }
                        state.notice = Some("rejected — agent asked to revise".into());
                        vec![Effect::Prompt {
                            record_id: focus_id,
                            text: REJECT_NOTE.into(),
                        }]
                    }
                };
            }
            _ => {}
        }
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
        // Focus session N (switch orchestrator conversation).
        LeaderAction::FocusSession(idx) => {
            if let Some(&i) = state.sessions_list().get(idx) {
                state.detail = DetailLayout::solo(state.agents[i].record_id.clone());
                mark_shown_seen(state);
            } else {
                state.notice = Some(format!("no session {}", idx + 1));
            }
            Vec::new()
        }
        // Focus the next actionable subagent (needs-you → review), cycling
        // past the currently focused one.
        LeaderAction::NextActionable => {
            let actionable: Vec<usize> = state
                .roster()
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
            let focused = state.detail.focused_id().map(str::to_string);
            let next = actionable
                .iter()
                .position(|&i| Some(state.agents[i].record_id.as_str()) == focused.as_deref())
                .map(|pos| actionable[(pos + 1) % actionable.len()])
                .unwrap_or(actionable[0]);
            state.detail = DetailLayout::solo(state.agents[next].record_id.clone());
            mark_shown_seen(state);
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

/// Execute one palette command. Every action maps onto an existing reducer
/// path — the palette is a discoverable front door, not a second behavior set.
/// Hit-test a left-click against the zones the renderer recorded this frame.
/// Later-pushed zones sit on top, so the topmost match wins (`rev()`). Sidebar
/// buttons toggle their panel; a row click focuses that pane — its split slot
/// when already shown, else solo (TUI_SPEC_V3 §3: click is the pointer half
/// of navigation; no mode involved). Overlays (picker / palette / confirm /
/// which-key) swallow clicks: their zones sit behind the popup, so acting on
/// them would be a click-through.
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
        ClickTarget::ToggleSessions => run_command(state, Command::ToggleSessions),
        ClickTarget::ToggleSubagents => run_command(state, Command::ToggleSubagents),
        ClickTarget::SessionRow(i) => {
            let Some(&idx) = state.sessions_list().get(i) else {
                return Vec::new();
            };
            let id = state.agents[idx].record_id.clone();
            focus_or_solo(state, id);
            Vec::new()
        }
        ClickTarget::RailRow(i) => {
            let Some(&idx) = state.roster().get(i) else {
                return Vec::new();
            };
            let id = state.agents[idx].record_id.clone();
            focus_or_solo(state, id);
            Vec::new()
        }
        ClickTarget::NewSession => run_command(state, Command::NewSession),
    }
}

pub(super) fn run_command(state: &mut AppState, cmd: Command) -> Vec<Effect> {
    match cmd {
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
        Command::SplitH | Command::SplitV => {
            let split = if cmd == Command::SplitH {
                Split::H
            } else {
                Split::V
            };
            split_detail(state, split);
            Vec::new()
        }
        Command::Unsplit => {
            state.detail.remove_focused();
            Vec::new()
        }
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
        Command::ToggleSessions => {
            state.sessions_collapsed = !state.sessions_collapsed;
            Vec::new()
        }
        Command::ToggleSubagents => {
            state.subagents_collapsed = !state.subagents_collapsed;
            Vec::new()
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

/// Split the detail in `split` direction (palette-only in v3). Adds the
/// roster's most actionable agent not yet shown. A notice explains the
/// no-op cases (all shown / full).
fn split_detail(state: &mut AppState, split: Split) {
    if state.detail.shown.len() >= MAX_SHOWN {
        state.notice = Some(format!(
            "detail is full ({MAX_SHOWN} panes) — unsplit drops a slot"
        ));
        return;
    }
    let target = state
        .roster()
        .into_iter()
        .map(|i| state.agents[i].record_id.clone())
        .find(|id| !state.detail.shown.contains(id));
    match target {
        Some(id) => {
            state.detail.add(id, split);
            mark_shown_seen(state);
        }
        None => {
            state.notice = Some("nothing to split with — every agent is already shown".into());
        }
    }
}

/// Focus `id` in the detail: when it is already shown (a split slot), move
/// focus to its slot — the only way to switch slots, so clicking must never
/// collapse a split; otherwise open it solo.
fn focus_or_solo(state: &mut AppState, id: String) {
    match state.detail.shown.iter().position(|r| r == &id) {
        Some(slot) => state.detail.focus = slot,
        None => state.detail = DetailLayout::solo(id),
    }
    mark_shown_seen(state);
}

/// Close the focused pane (leader `c` / palette `close agent`; attach close
/// = detach). A *live* orchestrator-owned monitor stays: another process
/// owns that session, and removing the pane would orphan its future
/// permission requests — every close surface shares this guard.
fn close_focused(state: &mut AppState) -> Vec<Effect> {
    match state
        .focused()
        .map(|p| (p.record_id.clone(), p.owner, p.exited))
    {
        Some((_, Ownership::Orchestrator, false)) => {
            state.notice =
                Some("orchestrator-managed subagent — close it there (close_subagent)".into());
            Vec::new()
        }
        Some((id, _, _)) => close_agent_by_id(state, &id),
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

/// Close one agent by id: remove it, prune the detail layout (refilling it
/// with the most actionable agent if it empties), emit `CloseAgent`. Closing
/// the last agent quits.
fn close_agent_by_id(state: &mut AppState, record_id: &str) -> Vec<Effect> {
    if !state.agents.iter().any(|p| p.record_id == record_id) {
        return Vec::new();
    }
    state.agents.retain(|p| p.record_id != record_id);
    state.detail.prune(record_id);
    if state.agents.is_empty() {
        state.should_quit = true;
    } else if state.detail.shown.is_empty() {
        // Refill with the roster head (most actionable agent), falling back
        // to the first session when no ACP agents remain.
        let head = state
            .roster()
            .into_iter()
            .next()
            .or_else(|| state.sessions_list().into_iter().next());
        if let Some(head) = head {
            state.detail = DetailLayout::solo(state.agents[head].record_id.clone());
        }
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
