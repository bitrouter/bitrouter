//! Event reduction: folds an external `AppEvent` into `AppState` and
//! returns the `Effect`s the loop must run. `reduce_inner` is the event
//! dispatcher; `apply_update` folds one translated session update into a
//! pane. Pure — no I/O, no session access.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use agent_client_protocol::schema::v1::StopReason;
use bitrouter_substrate::translate::{PermissionOutcome, SessionUpdateKind, ToolStatus};

use crate::risk::Risk;
use crate::tui::event::{AppEvent, DiffData, Effect, PermOption};

use super::diff::{Line, unified_to_lines};
use super::keys::{
    reduce_click, reduce_key_command, reduce_key_confirm, reduce_key_leader, reduce_key_normal,
    reduce_key_picker,
};
use super::layout::DetailLayout;
use super::overlay::Mode;
use super::pane::{Autonomy, Ownership, PaneKind, PaneState, PendingView, TailKind};
use super::{AppState, CHECK_RETRY_CAP, NOTICE_DECAY_TICKS, mark_shown_seen, stop_label};

pub(super) fn reduce_inner(state: &mut AppState, event: &AppEvent) -> Vec<Effect> {
    match event {
        AppEvent::Update { record_id, update } => on_update(state, record_id, update),
        AppEvent::TurnEnded {
            record_id,
            stop_reason,
        } => on_turn_ended(state, record_id, stop_reason),
        AppEvent::Exited { record_id } => on_exited(state, record_id),
        AppEvent::Permission {
            record_id,
            title,
            diff,
            options,
            risk,
        } => on_permission(state, record_id, title, diff, options, risk),
        AppEvent::AgentSpawned {
            record_id,
            agent_id,
            port,
        } => on_agent_spawned(state, record_id, agent_id, port),
        #[cfg(unix)]
        AppEvent::BridgeConnected { conn } => on_bridge_connected(state, conn),
        #[cfg(unix)]
        AppEvent::BridgeSpawned {
            record_id,
            agent_id,
            port,
        } => on_bridge_spawned(state, record_id, agent_id, port),
        #[cfg(unix)]
        AppEvent::BridgeState {
            record_id,
            state: s,
        } => on_bridge_state(state, record_id, s),
        #[cfg(unix)]
        AppEvent::BridgeGone { record_ids } => on_bridge_gone(state, record_ids),
        #[cfg(unix)]
        AppEvent::BridgeNotify { message } => on_bridge_notify(state, message),
        #[cfg(unix)]
        AppEvent::BridgeRequestAttach { record_id } => on_bridge_request_attach(state, record_id),
        #[cfg(unix)]
        AppEvent::BridgeRequestReview { record_id } => on_bridge_request_review(state, record_id),
        AppEvent::AgentSpawnFailed { agent_id, error } => {
            on_agent_spawn_failed(state, agent_id, error)
        }
        AppEvent::PtyAttached {
            record_id,
            agent_id,
        } => on_pty_attached(state, record_id, agent_id),
        AppEvent::SessionSpawned {
            record_id,
            binary,
            model,
        } => on_session_spawned(state, record_id, binary, model),
        AppEvent::PromptFailed { record_id, error } => on_prompt_failed(state, record_id, error),
        AppEvent::ReviewReady {
            record_id,
            files,
            adds,
            dels,
        } => on_review_ready(state, record_id, files, adds, dels),
        AppEvent::ChecksFailed { record_id, output } => on_checks_failed(state, record_id, output),
        AppEvent::DiffLoaded { record_id, text } => on_diff_loaded(state, record_id, text),
        AppEvent::OpDone {
            record_id,
            message,
            ok,
        } => on_op_done(state, record_id, message, ok),
        AppEvent::Paste(text) => on_paste(state, text),
        AppEvent::Scroll { up } => on_scroll(state, up),
        AppEvent::Click { col, row } => reduce_click(state, *col, *row),
        AppEvent::Focus(gained) => on_focus(state, gained),
        AppEvent::Tick => on_tick(state),
        AppEvent::ServeStatus { ok } => on_serve_status(state, ok),
        AppEvent::ForceQuit => on_force_quit(state),
        AppEvent::Key(key) => on_key(state, key),
    }
}

fn on_update(state: &mut AppState, record_id: &str, update: &SessionUpdateKind) -> Vec<Effect> {
    if let Some(pane) = state.pane_by_id_mut(record_id) {
        apply_update(pane, update);
    }
    Vec::new()
}

fn on_turn_ended(state: &mut AppState, record_id: &str, stop_reason: &StopReason) -> Vec<Effect> {
    // Seen = visible in the detail while the terminal has focus;
    // anything less makes the completion inbox material.
    let seen = state.is_shown(record_id) && state.term_focused;
    let notify = !state.term_focused;
    let mut effects = Vec::new();
    if let Some(pane) = state.pane_by_id_mut(record_id) {
        pane.flush_tail();
        pane.turn_active = false;
        pane.last_stop = Some(*stop_reason);
        // An abnormal end is worth a visible note; a clean end_turn
        // shows through the idle glyph alone.
        if *stop_reason != StopReason::EndTurn {
            pane.push(Line::Note(format!(
                "turn ended: {}",
                stop_label(stop_reason)
            )));
        } else {
            // Clean end: have the loop inspect the worktree (diff +
            // checks) — a non-empty diff feeds the review queue.
            effects.push(Effect::CheckReview {
                record_id: record_id.to_owned(),
            });
        }
        if !seen {
            // A finished turn is exactly what the tower should
            // surface — glyph only, no bell (completions are calm).
            // Cleared when the human views the pane.
            pane.done = true;
        }
        if notify {
            // The human is away — reach them through the terminal.
            effects.push(Effect::Notify {
                title: format!("{} finished", pane.agent_id),
                body: if *stop_reason == StopReason::EndTurn {
                    "turn complete".to_string()
                } else {
                    stop_label(stop_reason).to_string()
                },
            });
        }
    }
    effects
}

fn on_exited(state: &mut AppState, record_id: &str) -> Vec<Effect> {
    let shown = state.is_shown(record_id);
    let seen = shown && state.term_focused;
    let notify = !state.term_focused;
    let mut effects = Vec::new();
    if let Some(pane) = state.pane_by_id_mut(record_id) {
        pane.flush_tail();
        pane.exited = true;
        pane.turn_active = false;
        // A dead agent's decision is moot — drop it from the queue.
        // (The loop's teardown drops the resolvable handle → Deny.)
        pane.pending = None;
        if !seen {
            pane.attention = true;
        }
        if !shown {
            effects.push(Effect::Bell);
        }
        if notify {
            effects.push(Effect::Notify {
                title: format!("{} exited", pane.agent_id),
                body: "the agent process ended".to_string(),
            });
        }
    }
    effects
}

fn on_permission(
    state: &mut AppState,
    record_id: &str,
    title: &str,
    diff: &Option<DiffData>,
    options: &[PermOption],
    risk: &Risk,
) -> Vec<Effect> {
    let shown = state.is_shown(record_id);
    let seen = shown && state.term_focused;
    let notify = !state.term_focused;
    state.perm_seq += 1;
    let seq = state.perm_seq;
    let mut effects = Vec::new();
    if let Some(pane) = state.pane_by_id_mut(record_id) {
        // Autonomy policy: does this request reach the user?
        let auto_allow = match pane.autonomy {
            Autonomy::Manual => false,
            Autonomy::Assisted => *risk == Risk::Low,
            Autonomy::Auto => true,
        };
        if auto_allow {
            // Logged, never silent.
            let label = pane.autonomy.label();
            pane.push_external(Line::AutoResolved(format!(
                "auto-allowed ({label}): {title}"
            )));
            effects.push(Effect::ResolvePermission {
                record_id: record_id.to_owned(),
                outcome: PermissionOutcome::AllowOnce,
            });
        } else {
            pane.pending = Some(PendingView {
                title: title.to_owned(),
                diff: diff.clone(),
                options: options.to_vec(),
                risk: *risk,
            });
            pane.pending_seq = seq;
            if !seen {
                pane.attention = true;
            }
            if !shown {
                effects.push(Effect::Bell);
            }
            if notify {
                let risk_tag = match risk {
                    Risk::High => "high risk · ",
                    Risk::Low => "",
                };
                effects.push(Effect::Notify {
                    title: format!("{} needs approval", pane.agent_id),
                    body: format!("{risk_tag}{title}"),
                });
            }
        }
    } else {
        // No pane to surface it in (closed in the arrival race):
        // deny explicitly rather than strand the requester waiting
        // on an answer that can never come.
        effects.push(Effect::ResolvePermission {
            record_id: record_id.to_owned(),
            outcome: PermissionOutcome::Deny,
        });
    }
    effects
}

fn on_agent_spawned(
    state: &mut AppState,
    record_id: &str,
    agent_id: &str,
    port: &Option<u16>,
) -> Vec<Effect> {
    let mut pane = PaneState::new(record_id.to_owned(), agent_id.to_owned());
    if let Some(h) = state.harness_by_agent.get(agent_id) {
        pane.harness = h.clone();
    }
    pane.port = *port;
    state.agents.push(pane);
    // A just-spawned agent is what you want to look at: open it solo.
    state.detail = DetailLayout::solo(record_id.to_owned());
    state.notice = None;
    Vec::new()
}

#[cfg(unix)]
/// ── MCP fleet bridge (Unix): the orchestrator's subagents mirror
/// into the rail; their gated permissions ride the same decision
/// queue as TUI-spawned agents. ──
fn on_bridge_connected(state: &mut AppState, conn: &u64) -> Vec<Effect> {
    vec![Effect::BridgeHello {
        conn: *conn,
        bootstrap_approved: state.bootstrap_decision == Some(true),
    }]
}

#[cfg(unix)]
fn on_bridge_spawned(
    state: &mut AppState,
    record_id: &str,
    agent_id: &str,
    port: &Option<u16>,
) -> Vec<Effect> {
    let mut pane = PaneState::new(record_id.to_owned(), agent_id.to_owned());
    pane.owner = Ownership::Orchestrator;
    pane.harness = "mcp".to_string();
    pane.port = *port;
    // A bridge spawn blocks on its first turn — it starts working.
    pane.turn_active = true;
    pane.push(Line::Note(
        "spawned by the orchestrator (MCP) — monitor only; steer it there".into(),
    ));
    state.agents.push(pane);
    // Unlike a human-initiated spawn, don't steal the detail focus:
    // the human is mid-conversation with the orchestrator.
    Vec::new()
}

#[cfg(unix)]
fn on_bridge_state(state: &mut AppState, record_id: &str, s: &str) -> Vec<Effect> {
    if let Some(pane) = state.pane_by_id_mut(record_id) {
        match s {
            "working" => {
                pane.turn_active = true;
                pane.done = false;
            }
            "failed" => {
                pane.turn_active = false;
                pane.attention = true;
                pane.push_external(Line::Error("turn failed (see orchestrator)".into()));
            }
            // `completed` (or anything else): done-unseen, decaying
            // on view like any finished turn.
            _ => {
                pane.turn_active = false;
                pane.done = true;
            }
        }
    }
    Vec::new()
}

#[cfg(unix)]
fn on_bridge_gone(state: &mut AppState, record_ids: &[String]) -> Vec<Effect> {
    for id in record_ids {
        if let Some(pane) = state.pane_by_id_mut(id) {
            pane.exited = true;
            pane.turn_active = false;
            // The bridge side already denied its pendings when the
            // stream dropped.
            pane.pending = None;
            pane.push_external(Line::Note("bridge disconnected".into()));
        }
    }
    Vec::new()
}

#[cfg(unix)]
/// ── Human-bridge escalations from the orchestrator: reuse the existing
/// notice + attention + review-queue affordances (no new UI). ──
fn on_bridge_notify(state: &mut AppState, message: &str) -> Vec<Effect> {
    state.notice = Some(one_line(message));
    Vec::new()
}

#[cfg(unix)]
fn on_bridge_request_attach(state: &mut AppState, record_id: &str) -> Vec<Effect> {
    // Surface as an actionable rail item: mark the subagent for
    // attention (lifting it in the roster) and note the ask. The human
    // drives the attach — a mirror pane is monitor-only.
    let name = if let Some(pane) = state.pane_by_id_mut(record_id) {
        pane.attention = true;
        pane.push_external(Line::Note(
            "the orchestrator asks you to attach and drive this subagent".into(),
        ));
        pane.agent_id.clone()
    } else {
        record_id.to_owned()
    };
    state.notice = Some(one_line(&format!("attach requested: {name}")));
    Vec::new()
}

#[cfg(unix)]
fn on_bridge_request_review(state: &mut AppState, record_id: &str) -> Vec<Effect> {
    // Flag into the review queue via the same `review` affordance a
    // finished turn uses; the diff stat is unknown here, so 0/0/0.
    let name = if let Some(pane) = state.pane_by_id_mut(record_id) {
        pane.review = Some((0, 0, 0));
        pane.attention = true;
        pane.push_external(Line::Note(
            "the orchestrator flagged this subagent's work for your review".into(),
        ));
        pane.agent_id.clone()
    } else {
        record_id.to_owned()
    };
    state.notice = Some(one_line(&format!("review requested: {name}")));
    Vec::new()
}

fn on_agent_spawn_failed(state: &mut AppState, agent_id: &str, error: &str) -> Vec<Effect> {
    // The mode bar is one line: a multi-line upstream error (JSON-RPC
    // bodies…) must flatten or everything after the first newline is
    // silently lost.
    state.notice = Some(one_line(&format!("failed to spawn {agent_id}: {error}")));
    Vec::new()
}

fn on_pty_attached(state: &mut AppState, record_id: &str, agent_id: &str) -> Vec<Effect> {
    let mut pane = PaneState::new(record_id.to_owned(), agent_id.to_owned());
    pane.kind = PaneKind::Pty;
    pane.harness = "attach".to_string();
    state.agents.push(pane);
    // Attaching is for DRIVING this one agent — show it solo, keys
    // pass through; `leader c` on the attach pane detaches.
    state.detail = DetailLayout::solo(record_id.to_owned());
    state.mode = Mode::Normal;
    state.notice = Some(format!("attached — {} c detaches", state.leader_label()));
    Vec::new()
}

fn on_session_spawned(
    state: &mut AppState,
    record_id: &str,
    binary: &str,
    model: &Option<String>,
) -> Vec<Effect> {
    let mut pane = PaneState::new(record_id.to_owned(), binary.to_owned());
    pane.kind = PaneKind::Pty;
    pane.harness = "pty".to_string();
    pane.model = model.clone();
    state.agents.push(pane);
    // A fresh session is what you asked to talk to — show it solo.
    state.detail = DetailLayout::solo(record_id.to_owned());
    state.mode = Mode::Normal;
    state.notice = None;
    Vec::new()
}

fn on_prompt_failed(state: &mut AppState, record_id: &str, error: &str) -> Vec<Effect> {
    let shown = state.is_shown(record_id);
    let seen = shown && state.term_focused;
    let notify = !state.term_focused;
    let mut effects = Vec::new();
    if let Some(pane) = state.pane_by_id_mut(record_id) {
        pane.push_external(Line::Error(format!("prompt failed: {error}")));
        pane.turn_active = false;
        if !seen {
            pane.attention = true;
        }
        if !shown {
            effects.push(Effect::Bell);
        }
        if notify {
            effects.push(Effect::Notify {
                title: format!("{} prompt failed", pane.agent_id),
                body: error.to_owned(),
            });
        }
    }
    effects
}

fn on_review_ready(
    state: &mut AppState,
    record_id: &str,
    files: &u64,
    adds: &u64,
    dels: &u64,
) -> Vec<Effect> {
    let seen = state.is_shown(record_id) && state.term_focused;
    let notify = !state.term_focused;
    let mut effects = Vec::new();
    if let Some(pane) = state.pane_by_id_mut(record_id) {
        pane.review = Some((*files, *adds, *dels));
        pane.push(Line::Note(format!(
            "ready to review: {files} file(s), +{adds}/-{dels}"
        )));
        if !seen {
            pane.done = true; // glyph only — completions are calm
        }
        if notify {
            effects.push(Effect::Notify {
                title: format!("{} ready to review", pane.agent_id),
                body: format!("{files} file(s), +{adds}/-{dels}"),
            });
        }
    }
    effects
}

fn on_checks_failed(state: &mut AppState, record_id: &str, output: &str) -> Vec<Effect> {
    let shown = state.is_shown(record_id);
    let seen = shown && state.term_focused;
    let notify = !state.term_focused;
    let mut effects = Vec::new();
    if let Some(pane) = state.pane_by_id_mut(record_id) {
        if pane.check_retries < CHECK_RETRY_CAP {
            // A failing gate loops back to the subagent, not the human.
            pane.check_retries += 1;
            let retry = pane.check_retries;
            pane.push(Line::Note(format!(
                "checks failed — sent back to the agent (retry {retry}/{CHECK_RETRY_CAP})"
            )));
            pane.turn_active = true;
            pane.done = false;
            effects.push(Effect::Prompt {
                record_id: record_id.to_owned(),
                text: format!(
                    "The verification checks failed in your worktree. Fix the failures and make the checks pass.\n\nCheck output:\n{output}"
                ),
            });
        } else {
            // Retries exhausted: the human decides.
            pane.review = Some((0, 0, 0));
            pane.push_external(Line::Error(format!(
                "checks still failing after {CHECK_RETRY_CAP} retries — review manually"
            )));
            if !seen {
                pane.attention = true;
            }
            if !shown {
                effects.push(Effect::Bell);
            }
            if notify {
                effects.push(Effect::Notify {
                    title: format!("{} checks failing", pane.agent_id),
                    body: format!(
                        "still failing after {CHECK_RETRY_CAP} retries — review manually"
                    ),
                });
            }
        }
    }
    effects
}

fn on_diff_loaded(state: &mut AppState, record_id: &str, text: &str) -> Vec<Effect> {
    if let Some(pane) = state.pane_by_id_mut(record_id) {
        pane.flush_tail();
        for line in unified_to_lines(text) {
            pane.push(line);
        }
    }
    Vec::new()
}

fn on_op_done(state: &mut AppState, record_id: &str, message: &str, ok: &bool) -> Vec<Effect> {
    if let Some(pane) = state.pane_by_id_mut(record_id) {
        if *ok {
            // Integrated — the human has engaged with this result.
            pane.review = None;
            pane.done = false;
            pane.attention = false;
            pane.push_external(Line::Note(message.to_owned()));
        } else {
            pane.push_external(Line::Error(message.to_owned()));
        }
    }
    Vec::new()
}

fn on_paste(state: &mut AppState, text: &str) -> Vec<Effect> {
    // One event, whole text: pasting must never act like typed keys
    // (N Enter submissions) or feed panes that can't take input.
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    match state.mode {
        Mode::Command => {
            if let Some(palette) = state.palette.as_mut() {
                // The palette is a one-line filter.
                palette.input.push_str(one_line(&text).trim());
                palette.selected = 0;
            }
            Vec::new()
        }
        Mode::Normal => match state.focused() {
            Some(p) if p.kind == PaneKind::Pty && !p.exited => {
                vec![Effect::PtyPaste {
                    record_id: p.record_id.clone(),
                    text,
                }]
            }
            // Monitors are read-only (TUI_SPEC_V3 I2) — paste has
            // nowhere to land.
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn on_scroll(state: &mut AppState, up: &bool) -> Vec<Effect> {
    // Overlays (leader / picker / palette / confirm / which-key)
    // capture input: the wheel must not page — or worse, type into —
    // the pane behind them (mirrors `reduce_click`'s gate).
    if state.mode != Mode::Normal || state.keys_help {
        return Vec::new();
    }
    let Some(pane) = state.focused_mut() else {
        return Vec::new();
    };
    match pane.kind {
        PaneKind::Monitor => {
            if *up {
                pane.scroll_page_up();
            } else {
                pane.scroll_page_down();
            }
            Vec::new()
        }
        // PTY panes own their scrollback: forward as arrow presses.
        PaneKind::Pty => {
            let record_id = pane.record_id.clone();
            let code = if *up { KeyCode::Up } else { KeyCode::Down };
            (0..3)
                .map(|_| Effect::PtyKey {
                    record_id: record_id.clone(),
                    key: KeyEvent::from(code),
                })
                .collect()
        }
    }
}

fn on_focus(state: &mut AppState, gained: &bool) -> Vec<Effect> {
    state.term_focused = *gained;
    if *gained {
        // Back at the terminal: what is on screen counts as seen.
        mark_shown_seen(state);
    }
    Vec::new()
}

fn on_tick(state: &mut AppState) -> Vec<Effect> {
    state.tick = state.tick.wrapping_add(1);
    // Notices are transient: decay off the status bar rather than
    // lingering until something else overwrites them.
    if state.notice.is_some() && state.tick.wrapping_sub(state.notice_at) > NOTICE_DECAY_TICKS {
        state.notice = None;
    }
    Vec::new()
}

fn on_serve_status(state: &mut AppState, ok: &bool) -> Vec<Effect> {
    state.serve_ok = Some(*ok);
    Vec::new()
}

fn on_force_quit(state: &mut AppState) -> Vec<Effect> {
    state.should_quit = true;
    vec![Effect::Quit]
}

fn on_key(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    // The which-key overlay swallows exactly one key to dismiss —
    // checked before anything else so even Ctrl-C just closes it
    // instead of reaching the child mid-read.
    if state.keys_help {
        state.keys_help = false;
        return Vec::new();
    }
    // Ctrl-C never quits the manager (quit lives in the palette and
    // leader `c` on the last pane; the loop's ForceQuit covers
    // teardown). In overlay modes it cancels like Esc; in NORMAL it
    // interrupts the FOCUSED AGENT (PTY: raw 0x03 passes through;
    // ACP: cancel the in-flight turn).
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if state.mode != Mode::Normal {
            let esc = KeyEvent::from(KeyCode::Esc);
            return match state.mode {
                Mode::Leader => reduce_key_leader(state, &esc),
                Mode::Picker => reduce_key_picker(state, &esc),
                Mode::Command => reduce_key_command(state, &esc),
                Mode::Confirm => reduce_key_confirm(state, &esc),
                Mode::Normal => Vec::new(),
            };
        }
        if let Some(pane) = state.focused()
            && !pane.exited
        {
            let record_id = pane.record_id.clone();
            return match (pane.kind, pane.owner) {
                (PaneKind::Pty, _) => vec![Effect::PtyKey {
                    record_id,
                    key: *key,
                }],
                (PaneKind::Monitor, Ownership::Human) => {
                    vec![Effect::CancelTurn { record_id }]
                }
                // The orchestrator owns its subagents' turns —
                // nothing to interrupt from here; say so instead of
                // swallowing the key silently.
                (PaneKind::Monitor, Ownership::Orchestrator) => {
                    state.notice = Some(
                        "orchestrator-managed subagent — interrupt it from the orchestrator".into(),
                    );
                    Vec::new()
                }
            };
        }
        // Dead pane / nothing focused: nothing to interrupt, and a
        // reflexive Ctrl-C must not tear down the whole tower.
        state.notice = Some(format!(
            "nothing to interrupt — quit via {} p → quit",
            state.leader_label()
        ));
        return Vec::new();
    }
    match state.mode {
        Mode::Normal => reduce_key_normal(state, key),
        Mode::Leader => reduce_key_leader(state, key),
        Mode::Picker => reduce_key_picker(state, key),
        Mode::Command => reduce_key_command(state, key),
        Mode::Confirm => reduce_key_confirm(state, key),
    }
}

/// Collapse text into one mode-bar-sized line: whitespace runs (including
/// newlines) become single spaces, capped at 200 chars with an ellipsis.
fn one_line(text: &str) -> String {
    const CAP: usize = 200;
    let mut out = String::new();
    let mut count = 0usize;
    for word in text.split_whitespace() {
        if count > 0 {
            out.push(' ');
        }
        out.push_str(word);
        count += word.chars().count() + 1;
        if count > CAP {
            out.push('…');
            break;
        }
    }
    out
}

/// Fold one translated update into a pane's scrollback.
fn apply_update(pane: &mut PaneState, update: &SessionUpdateKind) {
    match update {
        // Streamed deltas go through the two-region model: only
        // newline-terminated text commits; the remainder is the mutable tail.
        SessionUpdateKind::MessageChunk { text, .. } => pane.stream(TailKind::Message, text),
        SessionUpdateKind::ThoughtChunk { text, .. } => pane.stream(TailKind::Thought, text),
        SessionUpdateKind::ToolCall {
            id,
            title,
            status,
            diff,
        } => {
            pane.push_external(Line::Tool {
                id: id.clone(),
                title: title.clone(),
                status: status.clone(),
            });
            if let Some(raw) = diff {
                pane.push_tool_diff(id, raw);
            }
        }
        SessionUpdateKind::ToolCallUpdate {
            id,
            status,
            title,
            diff,
        } => {
            // Merge into the existing tool line by id; if absent, append a new one.
            if let Some(Line::Tool {
                title: t,
                status: s,
                ..
            }) = pane
                .lines
                .iter_mut()
                .rev()
                .find(|l| matches!(l, Line::Tool { id: lid, .. } if lid == id))
            {
                if let Some(new_status) = status {
                    *s = new_status.clone();
                }
                if let Some(new_title) = title {
                    *t = new_title.clone();
                }
            } else {
                pane.push_external(Line::Tool {
                    id: id.clone(),
                    title: title.clone().unwrap_or_default(),
                    status: status.clone().unwrap_or(ToolStatus::Pending),
                });
            }
            if let Some(raw) = diff {
                pane.push_tool_diff(id, raw);
            }
        }
        // Context-window occupancy + cost: shown in the header/roster, not
        // scrollback.
        SessionUpdateKind::Usage { used, size, cost } => {
            pane.usage = Some((*used, *size));
            if cost.is_some() {
                pane.cost = cost.clone();
            }
        }
    }
}
