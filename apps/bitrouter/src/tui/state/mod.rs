//! Pure render state + reducer for the TUI. No `ratatui`/`tokio` deps.
//!
//! One screen: a fixed left rail (roster sorted by actionability + radar) and
//! a splittable detail viewport showing 1–4 agents. The rail is the canonical
//! list of every agent; the detail split is ephemeral layout, not structure.

use std::collections::HashMap;

use crate::risk::Risk;
use crate::tui::event::{AppEvent, Effect};
use agent_client_protocol::schema::v1::StopReason;
use bitrouter_substrate::translate::UsageCost;
use crossterm::event::{KeyCode, KeyModifiers};

pub mod diff;
pub mod pane;
use self::pane::{Ownership, PaneKind, PaneState};
pub mod overlay;
use self::overlay::{DEFAULT_LEADER, Mode, PaletteState, PickerState};
pub mod layout;
use self::layout::{ClickZone, DetailLayout};
pub mod events;
pub mod keys;
use self::events::reduce_inner;

/// Max scrollback lines retained per pane (ring buffer).
const SCROLLBACK_CAP: usize = 2000;

/// Max agents shown at once in the detail viewport.
const MAX_SHOWN: usize = 4;

/// How many times failing verification checks are looped back to the agent
/// before the failure surfaces to the human.
const CHECK_RETRY_CAP: u8 = 2;

/// The canned rejection verdict (TUI_SPEC_V3 §5). There is no composer to
/// type a note into; the verdict itself is the signal — the owner (the
/// orchestrator, or the agent directly for hatch spawns) decides what to
/// change.
const REJECT_NOTE: &str =
    "the human reviewed the diff and rejected it — revise and finish the task";

/// Short label for a turn's ACP stop reason.
pub fn stop_label(stop: &StopReason) -> &'static str {
    match stop {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::MaxTurnRequests => "max_turn_requests",
        StopReason::Refusal => "refusal",
        StopReason::Cancelled => "cancelled",
        // The schema enum is #[non_exhaustive].
        _ => "unknown",
    }
}

/// UI ticks per second (the loop ticks every 200ms).
const TICKS_PER_SEC: u64 = 5;

/// How long a status-bar notice stays before decaying (~8s of ticks). The
/// durable facts a notice used to carry live in the status bar's right zone
/// (`serve` dot, counts), so the text itself can be transient.
const NOTICE_DECAY_TICKS: u64 = 8 * TICKS_PER_SEC;

/// Compact duration: sub-minute in seconds, sub-hour in minutes, then `NhMMm`.
fn fmt_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Whole-app render state: a flat agent list + the detail layout + rail cursor.
/// Accessors return `Option` because the agent list may be empty transiently
/// (right after the last agent closes, before `should_quit`).
#[derive(Debug, Clone)]
pub struct AppState {
    /// Every live agent, in spawn order (stable; the roster sorts a projection).
    pub agents: Vec<PaneState>,
    pub detail: DetailLayout,
    /// User-collapsed sidebars (palette `toggle sessions`/`toggle subagents`;
    /// narrow terminals also auto-collapse at render time without touching
    /// these).
    pub sessions_collapsed: bool,
    pub subagents_collapsed: bool,
    /// The configured one-shot leader chord (`tui.leader`, default
    /// [`DEFAULT_LEADER`]) — the only key NORMAL intercepts before PTY
    /// passthrough.
    pub leader: (KeyCode, KeyModifiers),
    /// Monotonic permission-arrival counter backing `PaneState.pending_seq`.
    perm_seq: u64,
    /// agent_id → harness tag, from the config catalog.
    pub harness_by_agent: HashMap<String, String>,
    pub should_quit: bool,
    pub mode: Mode,
    pub picker: Option<PickerState>,
    /// Command palette overlay state (present while `mode == Command`).
    pub palette: Option<PaletteState>,
    /// Which-key overlay: lists the current mode's bindings; any key dismisses.
    pub keys_help: bool,
    /// UI tick counter (drives the running spinner frame).
    pub tick: u64,
    /// The outer terminal has focus (crossterm focus events; assumed focused
    /// until told otherwise). While unfocused, completions and gated
    /// permissions emit outer-terminal notifications, and shown panes accrue
    /// unseen state that regaining focus clears.
    pub term_focused: bool,
    /// `NO_COLOR` requested: draw glyphs/styles without foreground colors.
    pub no_color: bool,
    pub available_agents: Vec<String>,
    /// Interactive binaries the `new session` picker offers (from the
    /// harness catalog: `claude`, `codex`, …).
    pub available_sessions: Vec<String>,
    /// Latest daemon-reachability probe (`None` = never probed) — the status
    /// bar's `serve ●/✗` dot, kept live by the loop's periodic probe.
    pub serve_ok: Option<bool>,
    pub notice: Option<String>,
    /// Tick at which `notice` last changed (stamped by the reduce wrapper) —
    /// notices decay off the status bar after [`NOTICE_DECAY_TICKS`] instead
    /// of lingering forever.
    notice_at: u64,
    /// The configured worktree bootstrap hook (`worktrees.bootstrap`), if any.
    pub bootstrap_cmd: Option<String>,
    /// The human's per-session bootstrap decision: `None` = not asked yet
    /// (the first isolated spawn asks), `Some(true)` = run it on every new
    /// worktree, `Some(false)` = skip it for this session.
    pub bootstrap_decision: Option<bool>,
    /// The spawn awaiting the bootstrap decision (present in `Mode::Confirm`).
    pub confirm_agent: Option<String>,
    /// Each PTY pane's inner size `(cols, rows)` as last drawn — the loop
    /// resizes the emulator + PTY (SIGWINCH) when one changes. Rebuilt every
    /// frame by the renderer.
    pub pty_areas: Vec<(String, (u16, u16))>,
    /// Clickable regions recorded by the renderer for the current frame (mouse
    /// hit-test targets: sidebar toggle buttons + roster rows). Rebuilt every
    /// frame, like [`AppState::pty_areas`].
    pub click_zones: Vec<ClickZone>,
}

impl AppState {
    pub fn new(pane: PaneState) -> Self {
        let detail = DetailLayout::solo(pane.record_id.clone());
        Self {
            agents: vec![pane],
            detail,
            sessions_collapsed: false,
            subagents_collapsed: false,
            leader: DEFAULT_LEADER,
            perm_seq: 0,
            harness_by_agent: HashMap::new(),
            should_quit: false,
            mode: Mode::Normal,
            picker: None,
            palette: None,
            keys_help: false,
            tick: 0,
            term_focused: true,
            no_color: false,
            available_agents: Vec::new(),
            available_sessions: Vec::new(),
            serve_ok: None,
            notice: None,
            notice_at: 0,
            bootstrap_cmd: None,
            bootstrap_decision: None,
            confirm_agent: None,
            pty_areas: Vec::new(),
            click_zones: Vec::new(),
        }
    }

    /// Set the list of agent ids the picker offers (from the config catalog).
    pub fn set_available_agents(&mut self, agents: Vec<String>) {
        self.available_agents = agents;
    }

    /// Set the agent_id → harness-tag map (from the config catalog).
    pub fn set_harness_map(&mut self, map: HashMap<String, String>) {
        for pane in self.agents.iter_mut() {
            if let Some(h) = map.get(&pane.agent_id) {
                pane.harness = h.clone();
            }
        }
        self.harness_by_agent = map;
    }

    /// Roster order for the subagents panel: indices of the ACP panes, sorted
    /// by actionability bucket (needs-you > attention > running > dead).
    /// Needs-you rows order by risk (high first) then age (oldest pending
    /// first) — the queue; other buckets keep spawn order. PTY panes live in
    /// the sessions panel ([`sessions_list`](Self::sessions_list)).
    pub fn roster(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.agents.len())
            .filter(|&i| self.agents[i].kind == PaneKind::Monitor)
            .collect();
        order.sort_by_key(|&i| {
            let p = &self.agents[i];
            match &p.pending {
                Some(pending) => {
                    let risk_rank = match pending.risk {
                        Risk::High => 0u64,
                        Risk::Low => 1,
                    };
                    (p.bucket(), risk_rank, p.pending_seq)
                }
                None => (p.bucket(), 0, i as u64),
            }
        });
        order
    }

    /// Terminal-title badge: pending attention counts by glyph (`⚠` needs
    /// you, `◆` review, `●` background trouble, `◉` done-unseen), or a calm
    /// app name when all clear. The loop re-emits the title (OSC 2) whenever
    /// this changes, so the tab/window name works as a badge.
    pub fn title_badge(&self) -> String {
        let mut badge = String::from("bitrouter");
        for (glyph, n) in self.badge_counts() {
            badge.push_str(&format!(" {glyph}{n}"));
        }
        if badge == "bitrouter" {
            badge.push_str(" tui");
        }
        badge
    }

    /// Non-zero attention counts by glyph (`⚠` needs you, `◆` review, `●`
    /// background trouble, `◉` done-unseen) — shared by the terminal-title
    /// badge and the status bar's right zone, so the two always agree.
    pub fn badge_counts(&self) -> Vec<(char, usize)> {
        [(0u8, '⚠'), (1, '◆'), (2, '●'), (3, '◉')]
            .into_iter()
            .filter_map(|(bucket, glyph)| {
                let n = self.agents.iter().filter(|p| p.bucket() == bucket).count();
                (n > 0).then_some((glyph, n))
            })
            .collect()
    }

    /// The manager-layer state of every ACP agent, in the durable
    /// [`FleetAgent`](bitrouter_substrate::fleet::FleetAgent) shape — the
    /// fleet-state snapshot's agent list. PTY panes (the orchestrator,
    /// interactive attaches) and orchestrator-owned monitors are not fleet
    /// agents and are skipped. Monitors are read-only (TUI_SPEC_V3 I2):
    /// there is no composer, so no draft to persist.
    pub fn fleet_agents(&self) -> Vec<bitrouter_substrate::fleet::FleetAgent> {
        self.agents
            .iter()
            .filter(|p| p.kind == PaneKind::Monitor && p.owner == Ownership::Human)
            .map(|p| bitrouter_substrate::fleet::FleetAgent {
                record_id: p.record_id.clone(),
                autonomy: p.autonomy.label().to_string(),
                review: p.review,
                port: p.port,
                pending: p.pending.as_ref().map(|pending| pending.title.clone()),
                draft: None,
                turn_active: p.turn_active,
                exited: p.exited,
            })
            .collect()
    }

    /// Sessions-panel order: indices of the PTY panes (orchestrator sessions
    /// and interactive attaches) in spawn order — stable, herdr-spaces style.
    pub fn sessions_list(&self) -> Vec<usize> {
        (0..self.agents.len())
            .filter(|&i| self.agents[i].kind == PaneKind::Pty)
            .collect()
    }

    /// Cumulative metered cost across the fleet — the status bar's `$`
    /// figure. Sums panes reporting the first-seen currency (mixed
    /// currencies don't add; later ones are skipped rather than lied about).
    pub fn total_cost(&self) -> Option<UsageCost> {
        let mut total: Option<UsageCost> = None;
        for cost in self.agents.iter().filter_map(|p| p.cost.as_ref()) {
            match &mut total {
                None => total = Some(cost.clone()),
                Some(t) if t.currency == cost.currency => t.amount += cost.amount,
                Some(_) => {}
            }
        }
        total
    }

    /// The durable identity of every orchestrator session, for the fleet
    /// snapshot (interactive attaches are transient and skipped).
    pub fn fleet_sessions(&self) -> Vec<bitrouter_substrate::fleet::OrchestratorState> {
        self.agents
            .iter()
            .filter(|p| p.is_session())
            .map(|p| bitrouter_substrate::fleet::OrchestratorState {
                binary: p.agent_id.clone(),
                model: p.model.clone(),
            })
            .collect()
    }

    /// The detail-focused pane (receives NORMAL-mode input).
    pub fn focused(&self) -> Option<&PaneState> {
        let id = self.detail.focused_id()?;
        self.agents.iter().find(|p| p.record_id == id)
    }

    /// The detail-focused pane, mutably.
    pub fn focused_mut(&mut self) -> Option<&mut PaneState> {
        let id = self.detail.focused_id()?.to_string();
        self.agents.iter_mut().find(|p| p.record_id == id)
    }

    /// Whether `record_id` is visible in the detail viewport.
    fn is_shown(&self, record_id: &str) -> bool {
        self.detail.shown.iter().any(|r| r == record_id)
    }

    fn pane_by_id_mut(&mut self, record_id: &str) -> Option<&mut PaneState> {
        self.agents.iter_mut().find(|p| p.record_id == record_id)
    }

    /// The configured leader chord as a short display label (`⌃space`,
    /// `⌃]`, …) — every affordance renders THIS, so hints stay honest when
    /// `tui.leader` is customized.
    pub fn leader_label(&self) -> String {
        match self.leader.0 {
            KeyCode::Char(' ') => "⌃space".to_string(),
            KeyCode::Char(c) => format!("⌃{c}"),
            _ => "leader".to_string(),
        }
    }
}

/// Fold one event into state, returning effects for the loop to run.
/// PURE: no I/O, no session access.
pub fn reduce(state: &mut AppState, event: &AppEvent) -> Vec<Effect> {
    let notice_before = state.notice.clone();
    let effects = reduce_inner(state, event);
    // A fresh notice starts its decay clock (the Tick arm clears it).
    if state.notice != notice_before && state.notice.is_some() {
        state.notice_at = state.tick;
    }
    // Time-in-state: stamp the tick whenever a pane changes actionability
    // bucket, so the rail can show how long it has been working/blocked/done.
    let tick = state.tick;
    for pane in state.agents.iter_mut() {
        let bucket = pane.bucket();
        if bucket != pane.last_bucket {
            pane.last_bucket = bucket;
            pane.since = tick;
        }
    }
    effects
}

/// Mark every agent visible in the detail viewport as seen (the user is now
/// looking at them): clears `attention` and decays `done` back to idle.
fn mark_shown_seen(state: &mut AppState) {
    if !state.term_focused {
        // On screen but the human is elsewhere — not seen yet.
        return;
    }
    let shown: Vec<String> = state.detail.shown.clone();
    for pane in state.agents.iter_mut() {
        if shown.iter().any(|r| r == &pane.record_id) {
            pane.attention = false;
            pane.done = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::diff::{DiffLine, Line};
    use super::keys::{request_spawn, run_command};
    use super::layout::{ClickTarget, Split};
    use super::overlay::{
        COMMANDS, Command, LEADER_LEAVES, LeaderAction, PickerPurpose, leader_action, parse_leader,
    };
    use super::pane::{Autonomy, PendingView, TailKind};
    use super::*;
    use crate::risk::Risk;
    use crate::tui::event::{AppEvent, DiffData, Effect, PermOption};
    use bitrouter_substrate::translate::{PermissionOutcome, SessionUpdateKind, ToolStatus};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn pane() -> PaneState {
        PaneState::new("rec-1".into(), "claude".into())
    }

    fn allow_deny() -> Vec<PermOption> {
        vec![
            PermOption {
                outcome: PermissionOutcome::AllowOnce,
                label: "allow".into(),
            },
            PermOption {
                outcome: PermissionOutcome::Deny,
                label: "deny".into(),
            },
        ]
    }

    fn msg(i: usize) -> AppEvent {
        // Newline-terminated so each chunk commits one scrollback line.
        AppEvent::Update {
            record_id: "rec-1".into(),
            update: SessionUpdateKind::MessageChunk {
                message_id: None,
                text: format!("line {i}\n"),
            },
        }
    }

    fn chunk_to(record_id: &str, text: &str) -> AppEvent {
        AppEvent::Update {
            record_id: record_id.into(),
            update: SessionUpdateKind::MessageChunk {
                message_id: None,
                text: text.into(),
            },
        }
    }

    fn press(code: KeyCode) -> AppEvent {
        AppEvent::Key(KeyEvent::from(code))
    }

    /// Three agents r0/r1/r2 in spawn order; detail shows r0 solo.
    fn agents3() -> AppState {
        let mut st = AppState::new(PaneState::new("r0".into(), "a0".into()));
        st.agents.push(PaneState::new("r1".into(), "a1".into()));
        st.agents.push(PaneState::new("r2".into(), "a2".into()));
        st
    }

    fn click(col: u16, row: u16) -> AppEvent {
        AppEvent::Click { col, row }
    }

    #[test]
    fn click_toggle_button_collapses_the_sidebar() {
        let mut st = agents3();
        st.click_zones.push(ClickZone {
            x: 0,
            y: 20,
            w: 3,
            h: 1,
            target: ClickTarget::ToggleSessions,
        });
        assert!(!st.sessions_collapsed);
        reduce(&mut st, &click(1, 20));
        assert!(st.sessions_collapsed, "<< toggles the sessions sidebar");
        reduce(&mut st, &click(1, 20));
        assert!(!st.sessions_collapsed, "clicking again restores it");
    }

    #[test]
    fn click_on_a_rail_row_opens_it_solo_in_agent_mode() {
        let mut st = agents3();
        // roster() with three idle agents keeps spawn order: row 1 == r1.
        st.click_zones.push(ClickZone {
            x: 24,
            y: 3,
            w: 20,
            h: 2,
            target: ClickTarget::RailRow(1),
        });
        reduce(&mut st, &click(30, 4));
        assert_eq!(
            st.mode,
            Mode::Normal,
            "no mode to enter — click just focuses"
        );
        assert_eq!(
            st.detail.shown,
            vec!["r1".to_string()],
            "the clicked agent opens solo"
        );
    }

    #[test]
    fn clicking_a_shown_panes_row_focuses_its_slot() {
        // A split's slots are focus-switchable by clicking their rows —
        // never collapsed back to solo (Fable review finding 4).
        let mut st = agents3(); // detail = [r0]
        run_command(&mut st, Command::SplitH); // [r0, r1], focus 1
        assert_eq!(st.detail.focus, 1);
        st.click_zones.push(ClickZone {
            x: 24,
            y: 3,
            w: 20,
            h: 2,
            target: ClickTarget::RailRow(0), // r0's row (all same bucket)
        });
        reduce(&mut st, &click(30, 4));
        assert_eq!(
            st.detail.shown,
            vec!["r0".to_string(), "r1".to_string()],
            "the split survives the click"
        );
        assert_eq!(st.detail.focus, 0, "focus moved to the clicked slot");
    }

    #[test]
    fn clicking_new_session_footer_opens_the_picker() {
        let mut st = agents3();
        st.available_sessions = vec!["claude".into()];
        st.click_zones.push(ClickZone {
            x: 0,
            y: 22,
            w: 24,
            h: 1,
            target: ClickTarget::NewSession,
        });
        reduce(&mut st, &click(2, 22));
        assert_eq!(st.mode, Mode::Picker);
        assert!(
            st.picker
                .as_ref()
                .is_some_and(|p| p.purpose == PickerPurpose::Session)
        );
    }

    #[test]
    fn click_on_a_session_row_focuses_the_sessions_panel() {
        let mut st = agents3();
        let mut orch = PaneState::new("orch".into(), "claude".into());
        orch.kind = PaneKind::Pty;
        st.agents.push(orch);
        // sessions_list() holds only the PTY pane: row 0 == orch.
        st.click_zones.push(ClickZone {
            x: 0,
            y: 2,
            w: 24,
            h: 2,
            target: ClickTarget::SessionRow(0),
        });
        reduce(&mut st, &click(5, 2));
        assert_eq!(
            st.mode,
            Mode::Normal,
            "no mode to enter — click just focuses"
        );
        assert_eq!(st.detail.shown, vec!["orch".to_string()]);
    }

    #[test]
    fn click_outside_every_zone_is_a_noop() {
        let mut st = agents3();
        st.click_zones.push(ClickZone {
            x: 0,
            y: 0,
            w: 2,
            h: 1,
            target: ClickTarget::ToggleSubagents,
        });
        reduce(&mut st, &click(50, 50));
        assert!(!st.subagents_collapsed, "a miss changes nothing");
        assert_eq!(st.mode, Mode::Normal);
    }

    #[test]
    fn clicks_are_swallowed_while_an_overlay_is_up() {
        let mut st = agents3();
        st.mode = Mode::Picker;
        st.click_zones.push(ClickZone {
            x: 0,
            y: 20,
            w: 3,
            h: 1,
            target: ClickTarget::ToggleSessions,
        });
        reduce(&mut st, &click(1, 20));
        assert!(
            !st.sessions_collapsed,
            "a click behind the picker must not act on the zone under it"
        );
    }

    /// The human-bridge escalations reuse the notice / attention / review-queue
    /// affordances rather than a new UI subsystem.
    #[cfg(unix)]
    #[test]
    fn human_bridge_events_reuse_notice_attention_and_review() {
        let mut st = AppState::new(PaneState::new("mcp:h1".into(), "codex-acp".into()));
        let find = |st: &AppState| {
            st.agents
                .iter()
                .find(|p| p.record_id == "mcp:h1")
                .expect("mirror pane")
                .clone()
        };

        // notify → a status-bar notice; the pane is untouched.
        reduce(
            &mut st,
            &AppEvent::BridgeNotify {
                message: "heads up".into(),
            },
        );
        assert_eq!(st.notice.as_deref(), Some("heads up"));

        // request_attach → the pane needs attention; the notice names the agent.
        reduce(
            &mut st,
            &AppEvent::BridgeRequestAttach {
                record_id: "mcp:h1".into(),
            },
        );
        assert!(find(&st).attention, "attach lifts the pane in the roster");
        assert!(
            st.notice
                .as_deref()
                .is_some_and(|n| n.contains("codex-acp")),
            "notice names the agent: {:?}",
            st.notice
        );

        // request_review → the pane enters the review queue.
        reduce(
            &mut st,
            &AppEvent::BridgeRequestReview {
                record_id: "mcp:h1".into(),
            },
        );
        assert!(
            find(&st).review.is_some(),
            "review flags the pane into the queue"
        );
    }

    // ── Scrollback paging. ──

    #[test]
    fn pageup_pins_view_and_new_output_does_not_move_it() {
        let mut st = AppState::new(pane());
        st.agents[0].viewport = 10;
        for i in 0..50 {
            reduce(&mut st, &msg(i));
        }
        reduce(&mut st, &press(KeyCode::PageUp));
        // Follow start was 40 (50 - viewport); one page up pins at 30.
        assert_eq!(st.agents[0].scroll, Some(30));
        for i in 50..60 {
            reduce(&mut st, &msg(i));
        }
        assert_eq!(
            st.agents[0].scroll,
            Some(30),
            "pinned view must not move when new output arrives"
        );
    }

    #[test]
    fn pagedown_returns_to_follow_at_tail() {
        let mut st = AppState::new(pane());
        st.agents[0].viewport = 10;
        for i in 0..50 {
            reduce(&mut st, &msg(i));
        }
        reduce(&mut st, &press(KeyCode::PageUp)); // pin at 30
        reduce(&mut st, &press(KeyCode::PageUp)); // pin at 20
        assert_eq!(st.agents[0].scroll, Some(20));
        reduce(&mut st, &press(KeyCode::PageDown)); // 30 — still off-tail
        assert_eq!(st.agents[0].scroll, Some(30));
        reduce(&mut st, &press(KeyCode::PageDown)); // window reaches tail
        assert_eq!(
            st.agents[0].scroll, None,
            "reaching the tail resumes following"
        );
    }

    #[test]
    fn pageup_clamps_at_top() {
        let mut st = AppState::new(pane());
        st.agents[0].viewport = 10;
        for i in 0..15 {
            reduce(&mut st, &msg(i));
        }
        reduce(&mut st, &press(KeyCode::PageUp));
        assert_eq!(st.agents[0].scroll, Some(0));
        reduce(&mut st, &press(KeyCode::PageUp)); // already at top — stays
        assert_eq!(st.agents[0].scroll, Some(0));
    }

    #[test]
    fn scroll_pin_tracks_ring_buffer_drain() {
        let mut st = AppState::new(pane());
        st.agents[0].viewport = 10;
        for i in 0..SCROLLBACK_CAP {
            reduce(&mut st, &msg(i));
        }
        reduce(&mut st, &press(KeyCode::PageUp));
        let pinned = st.agents[0].scroll.unwrap_or(0);
        reduce(&mut st, &msg(SCROLLBACK_CAP)); // overflows the cap by one
        assert_eq!(
            st.agents[0].scroll,
            Some(pinned.saturating_sub(1)),
            "pin slides with the ring buffer so it stays on the same content"
        );
    }

    #[test]
    fn pageup_works_while_permission_pending() {
        let mut st = AppState::new(pane());
        st.agents[0].viewport = 10;
        for i in 0..50 {
            reduce(&mut st, &msg(i));
        }
        reduce(
            &mut st,
            &AppEvent::Permission {
                record_id: "rec-1".into(),
                title: "WRITE src/x.rs".into(),
                diff: None,
                options: allow_deny(),
                risk: Risk::High,
            },
        );
        let effects = reduce(&mut st, &press(KeyCode::PageUp));
        assert!(effects.is_empty(), "scrolling resolves nothing");
        assert_eq!(st.agents[0].scroll, Some(30));
        assert!(
            st.agents[0].pending.is_some(),
            "pending permission untouched by scrolling"
        );
    }

    // ── Quit / interrupt (TUI_SPEC §9/§12: Ctrl-C interrupts, quit is on
    // the leader). ──

    fn ctrl_c() -> AppEvent {
        AppEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
    }

    #[test]
    fn ctrl_c_cancels_overlay_modes_like_esc_and_never_quits() {
        for mode in [Mode::Leader, Mode::Picker, Mode::Command, Mode::Confirm] {
            let mut st = AppState::new(pane());
            st.mode = mode;
            match mode {
                Mode::Picker => {
                    st.picker = Some(PickerState {
                        agents: vec!["alpha".into()],
                        selected: 0,
                        purpose: PickerPurpose::Subagent,
                    });
                }
                Mode::Command => st.palette = Some(PaletteState::default()),
                Mode::Confirm => st.confirm_agent = Some("alpha".into()),
                _ => {}
            }
            let effects = reduce(&mut st, &ctrl_c());
            assert!(!st.should_quit, "Ctrl-C must not quit from {mode:?}");
            assert!(effects.is_empty(), "cancel is effect-free from {mode:?}");
            assert_eq!(st.mode, Mode::Normal, "back to NORMAL from {mode:?}");
        }
        // The overlay state itself is cleared, exactly as Esc would.
        let mut st = AppState::new(pane());
        st.mode = Mode::Command;
        st.palette = Some(PaletteState::default());
        reduce(&mut st, &ctrl_c());
        assert!(st.palette.is_none(), "palette cleared like Esc");
    }

    #[test]
    fn ctrl_c_dismisses_keys_help_without_touching_the_child() {
        // Reflexively closing the help overlay with Ctrl-C must not send a
        // raw 0x03 into the focused PTY child (or quit).
        let mut st = AppState::new(pane());
        st.agents[0].kind = PaneKind::Pty;
        st.keys_help = true;
        let effects = reduce(&mut st, &ctrl_c());
        assert!(effects.is_empty(), "no PtyKey leaks past the overlay");
        assert!(!st.keys_help, "overlay dismissed");
        assert!(!st.should_quit);
    }

    #[test]
    fn ctrl_c_in_normal_interrupts_the_focused_agent_not_the_manager() {
        // ACP pane: cancel the in-flight turn.
        let mut st = AppState::new(pane());
        let effects = reduce(&mut st, &ctrl_c());
        assert_eq!(
            effects,
            vec![Effect::CancelTurn {
                record_id: "rec-1".into()
            }]
        );
        assert!(!st.should_quit, "the manager survives");

        // PTY pane: raw 0x03 passes through to the child.
        let mut st = AppState::new(pane());
        st.agents[0].kind = PaneKind::Pty;
        let effects = reduce(&mut st, &ctrl_c());
        assert!(
            matches!(&effects[..], [Effect::PtyKey { record_id, .. }] if record_id == "rec-1"),
            "{effects:?}"
        );
        assert!(!st.should_quit);

        // Dead pane: nothing to interrupt — a reflexive Ctrl-C (the moment a
        // pane crashes) must NOT tear down the tower; it points at quit.
        let mut st = AppState::new(pane());
        st.agents[0].exited = true;
        let effects = reduce(&mut st, &ctrl_c());
        assert!(effects.is_empty());
        assert!(!st.should_quit, "the manager survives a dead-pane Ctrl-C");
        assert!(
            st.notice.as_deref().is_some_and(|n| n.contains("quit")),
            "the notice points at the real quit path: {:?}",
            st.notice
        );

        // Orchestrator-owned monitor: nothing to interrupt from here — but
        // say so instead of swallowing the key silently.
        let mut st = AppState::new(pane());
        st.agents[0].owner = Ownership::Orchestrator;
        let effects = reduce(&mut st, &ctrl_c());
        assert!(effects.is_empty());
        assert!(!st.should_quit);
        assert!(
            st.notice
                .as_deref()
                .is_some_and(|n| n.contains("orchestrator")),
            "refusal is explained: {:?}",
            st.notice
        );
    }

    #[test]
    fn scroll_is_inert_while_an_overlay_captures_input() {
        // The wheel must not type into the PTY behind an armed leader /
        // picker / palette / confirm — and must not disturb the overlay.
        for mode in [Mode::Leader, Mode::Picker, Mode::Command, Mode::Confirm] {
            let mut st = AppState::new(pane());
            st.agents[0].kind = PaneKind::Pty;
            st.mode = mode;
            let fx = reduce(&mut st, &AppEvent::Scroll { up: true });
            assert!(fx.is_empty(), "no PtyKey leaks from {mode:?}: {fx:?}");
            assert_eq!(st.mode, mode, "the overlay stays armed");
        }
        // Same while the which-key overlay is up in NORMAL.
        let mut st = AppState::new(pane());
        st.agents[0].kind = PaneKind::Pty;
        st.keys_help = true;
        let fx = reduce(&mut st, &AppEvent::Scroll { up: false });
        assert!(fx.is_empty());
        assert!(st.keys_help, "scroll is not the dismissing key");
    }

    #[test]
    fn force_quit_always_tears_down() {
        // The loop synthesizes ForceQuit on input-stream end; it must quit
        // even where Ctrl-C would interrupt.
        let mut st = AppState::new(pane());
        let effects = reduce(&mut st, &AppEvent::ForceQuit);
        assert_eq!(effects, vec![Effect::Quit]);
        assert!(st.should_quit);
    }

    // ── Locked-mode passthrough (PTY pane focused). ──

    #[test]
    fn pty_pane_routes_every_key_except_the_leader() {
        let mut st = AppState::new(pane());
        st.agents[0].kind = PaneKind::Pty;
        // Plain keys, Ctrl-B (readline), PgUp: all pass through.
        for key in [
            KeyEvent::from(KeyCode::Char('x')),
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            KeyEvent::from(KeyCode::PageUp),
            KeyEvent::from(KeyCode::Enter),
        ] {
            let fx = reduce(&mut st, &AppEvent::Key(key));
            assert!(
                matches!(&fx[..], [Effect::PtyKey { .. }]),
                "{key:?} must pass through: {fx:?}"
            );
        }
        // Ctrl-A is readline Home — it passes through like any other key.
        let fx = reduce(
            &mut st,
            &AppEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        );
        assert!(
            matches!(&fx[..], [Effect::PtyKey { .. }]),
            "Ctrl-A passes through: {fx:?}"
        );
        assert_eq!(st.mode, Mode::Normal, "no manager mode to enter");
        // The one leader: Ctrl-Space opens the which-key prefix.
        let fx = reduce(
            &mut st,
            &AppEvent::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL)),
        );
        assert!(fx.is_empty());
        assert_eq!(st.mode, Mode::Leader, "the leader never reaches the child");
    }

    // ── Prompt failures. ──

    #[test]
    fn prompt_failed_surfaces_error_line_in_pane() {
        let mut st = AppState::new(pane());
        let effects = reduce(
            &mut st,
            &AppEvent::PromptFailed {
                record_id: "rec-1".into(),
                error: "acp transport closed".into(),
            },
        );
        // Shown pane: visible error line, no attention/bell needed.
        assert!(effects.is_empty());
        assert!(matches!(
            st.agents[0].lines.last(),
            Some(Line::Error(e)) if e.contains("acp transport closed")
        ));
        assert!(!st.agents[0].attention);
    }

    #[test]
    fn prompt_failed_on_background_pane_flags_attention_and_bells() {
        let mut st = agents3(); // detail shows only r0
        let effects = reduce(
            &mut st,
            &AppEvent::PromptFailed {
                record_id: "r2".into(),
                error: "boom".into(),
            },
        );
        assert_eq!(effects, vec![Effect::Bell]);
        assert!(st.agents[2].attention);
        assert!(matches!(st.agents[2].lines.last(), Some(Line::Error(_))));
    }

    // ── App shape + updates. ──

    #[test]
    fn new_app_shows_the_initial_agent_solo() {
        let st = AppState::new(pane());
        assert_eq!(st.agents.len(), 1);
        assert_eq!(st.detail.shown, vec!["rec-1".to_string()]);
        assert_eq!(st.detail.focus, 0);
    }

    #[test]
    fn permission_event_sets_pending_view() {
        let mut st = AppState::new(pane());
        let diff = DiffData {
            path: "src/x.rs".into(),
            old: "a\n".into(),
            new: "b\n".into(),
        };
        reduce(
            &mut st,
            &AppEvent::Permission {
                record_id: "rec-1".into(),
                title: "WRITE src/x.rs".into(),
                diff: Some(diff.clone()),
                options: allow_deny(),
                risk: Risk::High,
            },
        );
        let pending = st.agents[0].pending.as_ref().expect("pending set");
        assert_eq!(pending.title, "WRITE src/x.rs");
        assert_eq!(pending.diff.as_ref(), Some(&diff));
        assert_eq!(pending.options.len(), 2);
    }

    #[test]
    fn y_key_resolves_pending_allow_once_and_clears_it() {
        let mut st = AppState::new(pane());
        reduce(
            &mut st,
            &AppEvent::Permission {
                record_id: "rec-1".into(),
                title: "WRITE".into(),
                diff: None,
                options: allow_deny(),
                risk: Risk::High,
            },
        );
        let effects = reduce(&mut st, &press(KeyCode::Char('y')));
        assert_eq!(
            effects,
            vec![Effect::ResolvePermission {
                record_id: "rec-1".into(),
                outcome: PermissionOutcome::AllowOnce,
            }]
        );
        assert!(
            st.agents[0].pending.is_none(),
            "pending cleared after resolve"
        );
    }

    #[test]
    fn n_key_resolves_pending_deny() {
        let mut st = AppState::new(pane());
        reduce(
            &mut st,
            &AppEvent::Permission {
                record_id: "rec-1".into(),
                title: "WRITE".into(),
                diff: None,
                options: allow_deny(),
                risk: Risk::High,
            },
        );
        let effects = reduce(&mut st, &press(KeyCode::Char('n')));
        assert_eq!(
            effects,
            vec![Effect::ResolvePermission {
                record_id: "rec-1".into(),
                outcome: PermissionOutcome::Deny,
            }]
        );
    }

    // ── Two-region streaming: only newline-terminated text commits. ──

    #[test]
    fn unterminated_chunk_stays_in_the_tail_not_scrollback() {
        let mut st = AppState::new(pane());
        let effects = reduce(&mut st, &chunk_to("rec-1", "hi"));
        assert!(effects.is_empty());
        assert!(
            st.agents[0].lines.is_empty(),
            "half-formed line must not commit"
        );
        assert_eq!(
            st.agents[0].tail,
            Some((TailKind::Message, "hi".to_string()))
        );
    }

    #[test]
    fn word_by_word_deltas_commit_one_line_per_newline() {
        // The core A0 defect: streamed deltas must not render one word per
        // scrollback line.
        let mut st = AppState::new(pane());
        for delta in ["Hello", " ", "world", "!\nSecond", " line\n"] {
            reduce(&mut st, &chunk_to("rec-1", delta));
        }
        assert_eq!(
            st.agents[0].lines,
            vec![
                Line::Message("Hello world!".into()),
                Line::Message("Second line".into()),
            ]
        );
        assert_eq!(st.agents[0].tail, None, "fully committed");
    }

    #[test]
    fn kind_switch_flushes_the_other_streams_partial_line() {
        let mut st = AppState::new(pane());
        reduce(
            &mut st,
            &AppEvent::Update {
                record_id: "rec-1".into(),
                update: SessionUpdateKind::ThoughtChunk {
                    message_id: None,
                    text: "thinking".into(),
                },
            },
        );
        reduce(&mut st, &chunk_to("rec-1", "answer\n"));
        assert_eq!(
            st.agents[0].lines,
            vec![
                Line::Thought("thinking".into()),
                Line::Message("answer".into()),
            ],
            "partial thought commits before the message starts"
        );
    }

    #[test]
    fn tool_call_flushes_a_partial_streamed_line_first() {
        let mut st = AppState::new(pane());
        reduce(&mut st, &chunk_to("rec-1", "partial"));
        reduce(
            &mut st,
            &AppEvent::Update {
                record_id: "rec-1".into(),
                update: SessionUpdateKind::ToolCall {
                    id: "t1".into(),
                    title: "run tests".into(),
                    status: ToolStatus::Running,
                    diff: None,
                },
            },
        );
        assert_eq!(
            st.agents[0].lines,
            vec![
                Line::Message("partial".into()),
                Line::Tool {
                    id: "t1".into(),
                    title: "run tests".into(),
                    status: ToolStatus::Running
                },
            ],
            "ordering stays faithful: the partial line lands before the tool"
        );
    }

    #[test]
    fn fenced_code_commits_as_code_lines_with_lang() {
        let mut st = AppState::new(pane());
        reduce(
            &mut st,
            &chunk_to("rec-1", "```rust\nfn main() {}\n```\nafter\n"),
        );
        assert_eq!(
            st.agents[0].lines,
            vec![
                Line::Message("```rust".into()),
                Line::Code {
                    text: "fn main() {}".into(),
                    lang: "rust".into()
                },
                Line::Message("```".into()),
                Line::Message("after".into()),
            ]
        );
    }

    #[test]
    fn crlf_deltas_commit_clean_lines() {
        let mut st = AppState::new(pane());
        reduce(&mut st, &chunk_to("rec-1", "one\r\ntwo\r\n"));
        assert_eq!(
            st.agents[0].lines,
            vec![Line::Message("one".into()), Line::Message("two".into())]
        );
    }

    // ── Turn end (stop_reason capture). ──

    #[test]
    fn turn_ended_flushes_tail_clears_working_and_records_stop() {
        let mut st = AppState::new(pane());
        st.agents[0].turn_active = true; // a turn is in flight
        reduce(&mut st, &chunk_to("rec-1", "no trailing newline"));
        reduce(
            &mut st,
            &AppEvent::TurnEnded {
                record_id: "rec-1".into(),
                stop_reason: StopReason::EndTurn,
            },
        );
        let pane = &st.agents[0];
        assert!(!pane.turn_active, "turn over");
        assert_eq!(pane.last_stop, Some(StopReason::EndTurn));
        assert_eq!(pane.tail, None);
        assert!(
            pane.lines
                .contains(&Line::Message("no trailing newline".into())),
            "unterminated output commits at turn end"
        );
        assert!(
            !pane.attention,
            "clean end on the shown pane needs no marker"
        );
    }

    #[test]
    fn abnormal_turn_end_leaves_a_note() {
        let mut st = AppState::new(pane());
        reduce(
            &mut st,
            &AppEvent::TurnEnded {
                record_id: "rec-1".into(),
                stop_reason: StopReason::Cancelled,
            },
        );
        assert!(matches!(
            st.agents[0].lines.last(),
            Some(Line::Note(n)) if n.contains("cancelled")
        ));
    }

    #[test]
    fn background_turn_end_sets_done_without_bell() {
        let mut st = agents3(); // detail shows only r0
        let fx = reduce(
            &mut st,
            &AppEvent::TurnEnded {
                record_id: "r1".into(),
                stop_reason: StopReason::EndTurn,
            },
        );
        assert!(
            !fx.contains(&Effect::Bell),
            "completions are calm — no bell"
        );
        assert!(st.agents[1].done, "but the tower flags them done-unseen");
        assert!(
            !st.agents[1].attention,
            "done is inbox material, not trouble"
        );
    }

    // ── Usage + cost. ──

    #[test]
    fn usage_update_records_occupancy_and_cost() {
        let mut st = AppState::new(pane());
        reduce(
            &mut st,
            &AppEvent::Update {
                record_id: "rec-1".into(),
                update: SessionUpdateKind::Usage {
                    used: 1500,
                    size: 200_000,
                    cost: Some(UsageCost {
                        amount: 0.25,
                        currency: "USD".into(),
                    }),
                },
            },
        );
        assert_eq!(st.agents[0].usage, Some((1500, 200_000)));
        assert_eq!(
            st.agents[0].cost,
            Some(UsageCost {
                amount: 0.25,
                currency: "USD".into()
            })
        );
        // A later usage tick without cost keeps the last metered cost.
        reduce(
            &mut st,
            &AppEvent::Update {
                record_id: "rec-1".into(),
                update: SessionUpdateKind::Usage {
                    used: 1600,
                    size: 200_000,
                    cost: None,
                },
            },
        );
        assert!(st.agents[0].cost.is_some(), "cost survives cost-less ticks");
    }

    #[test]
    fn tool_call_diff_pushes_rendered_lines_once() {
        let mut st = AppState::new(pane());
        let raw = "x.rs\n[old]\na\n[new]\nb";
        let tool = |diff: Option<&str>, status: Option<ToolStatus>| AppEvent::Update {
            record_id: "rec-1".into(),
            update: SessionUpdateKind::ToolCallUpdate {
                id: "t1".into(),
                status,
                title: Some("WRITE x.rs".into()),
                diff: diff.map(str::to_string),
            },
        };
        reduce(&mut st, &tool(Some(raw), Some(ToolStatus::Running)));
        let with_diff = st.agents[0].lines.len();
        assert!(
            st.agents[0]
                .lines
                .contains(&Line::Diff(DiffLine::Add("b".into()))),
            "diff rendered under the tool line: {:?}",
            st.agents[0].lines
        );
        // The completion update repeats the same diff — no duplicate render.
        reduce(&mut st, &tool(Some(raw), Some(ToolStatus::Ok)));
        assert_eq!(
            st.agents[0].lines.len(),
            with_diff,
            "repeated diff must not duplicate"
        );
    }

    #[test]
    fn tool_call_then_update_merges_status() {
        let mut st = AppState::new(pane());
        reduce(
            &mut st,
            &AppEvent::Update {
                record_id: "rec-1".into(),
                update: SessionUpdateKind::ToolCall {
                    id: "t1".into(),
                    title: "run tests".into(),
                    status: ToolStatus::Running,
                    diff: None,
                },
            },
        );
        reduce(
            &mut st,
            &AppEvent::Update {
                record_id: "rec-1".into(),
                update: SessionUpdateKind::ToolCallUpdate {
                    id: "t1".into(),
                    status: Some(ToolStatus::Ok),
                    title: None,
                    diff: None,
                },
            },
        );
        assert_eq!(
            st.agents[0].lines,
            vec![Line::Tool {
                id: "t1".into(),
                title: "run tests".into(),
                status: ToolStatus::Ok
            }],
        );
    }

    #[test]
    fn update_for_unknown_record_is_ignored() {
        let mut st = AppState::new(pane());
        reduce(
            &mut st,
            &AppEvent::Update {
                record_id: "nope".into(),
                update: SessionUpdateKind::MessageChunk {
                    message_id: None,
                    text: "x".into(),
                },
            },
        );
        assert!(st.agents[0].lines.is_empty());
    }

    // ── Spawn. ──

    #[test]
    fn agent_spawned_appends_and_opens_solo() {
        let mut st = AppState::new(pane());
        reduce(
            &mut st,
            &AppEvent::AgentSpawned {
                record_id: "r9".into(),
                agent_id: "fake".into(),
                port: None,
            },
        );
        assert_eq!(st.agents.len(), 2);
        assert_eq!(st.agents[1].record_id, "r9");
        assert_eq!(st.agents[1].agent_id, "fake");
        assert_eq!(st.detail.shown, vec!["r9".to_string()]);
    }

    #[test]
    fn spawned_agent_gets_harness_from_map() {
        let mut st = AppState::new(pane());
        st.set_harness_map(HashMap::from([("fake".to_string(), "codex".to_string())]));
        reduce(
            &mut st,
            &AppEvent::AgentSpawned {
                record_id: "r9".into(),
                agent_id: "fake".into(),
                port: None,
            },
        );
        assert_eq!(st.agents[1].harness, "codex");
    }

    #[test]
    fn agent_spawn_failed_sets_notice_and_adds_no_pane() {
        let mut st = AppState::new(pane());
        reduce(
            &mut st,
            &AppEvent::AgentSpawnFailed {
                agent_id: "fake".into(),
                error: "boom".into(),
            },
        );
        assert_eq!(st.agents.len(), 1);
        assert_eq!(st.notice.as_deref(), Some("failed to spawn fake: boom"));
    }

    // ── NORMAL-mode keys: monitors are read-only (TUI_SPEC_V3 I2). ──

    #[test]
    fn monitor_pane_is_read_only() {
        let mut st = AppState::new(pane());
        for key in [
            press(KeyCode::Char('h')),
            press(KeyCode::Char('i')),
            press(KeyCode::Enter),
        ] {
            let fx = reduce(&mut st, &key);
            assert!(
                !fx.iter()
                    .any(|f| matches!(f, Effect::Prompt { .. } | Effect::PtyPaste { .. })),
                "a Monitor never emits a prompt or paste: {fx:?}"
            );
        }
        assert!(
            st.agents[0].lines.is_empty(),
            "nothing lands in a read-only transcript (Line no longer even
             has a user-prompt variant — read-only by construction)"
        );
    }

    #[test]
    fn leader_opens_whichkey() {
        let mut st = AppState::new(pane());
        let fx = reduce(
            &mut st,
            &AppEvent::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL)),
        );
        assert!(fx.is_empty());
        assert_eq!(st.mode, Mode::Leader, "leader → which-key prefix");
    }

    #[test]
    fn ctrl_a_is_not_a_leader() {
        // A focused PTY session owns readline Home: Ctrl-A passes through
        // untouched and enters no manager mode (TUI_SPEC_V3 §3).
        let mut st = AppState::new(pane());
        st.agents[0].kind = PaneKind::Pty;
        let fx = reduce(
            &mut st,
            &AppEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        );
        assert!(
            matches!(&fx[..], [Effect::PtyKey { .. }]),
            "Ctrl-A is passthrough: {fx:?}"
        );
        assert_eq!(st.mode, Mode::Normal, "no mode change");
    }

    #[test]
    fn configured_leader_replaces_the_default() {
        let mut st = AppState::new(pane());
        st.agents[0].kind = PaneKind::Pty;
        st.leader = parse_leader("ctrl-]").expect("parseable");
        // The configured chord arms the prefix…
        let fx = reduce(
            &mut st,
            &AppEvent::Key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL)),
        );
        assert!(fx.is_empty());
        assert_eq!(st.mode, Mode::Leader);
        // …and the default no longer does: Ctrl-Space reaches the child.
        st.mode = Mode::Normal;
        let fx = reduce(
            &mut st,
            &AppEvent::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL)),
        );
        assert!(
            matches!(&fx[..], [Effect::PtyKey { .. }]),
            "unconfigured chord passes through: {fx:?}"
        );
        assert_eq!(st.mode, Mode::Normal);
    }

    #[test]
    fn parse_leader_accepts_ctrl_chords_and_rejects_garbage() {
        assert_eq!(parse_leader("ctrl-space"), Some(DEFAULT_LEADER));
        assert_eq!(
            parse_leader("Ctrl-]"),
            Some((KeyCode::Char(']'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_leader("ctrl-\\"),
            Some((KeyCode::Char('\\'), KeyModifiers::CONTROL))
        );
        assert_eq!(parse_leader("space"), None, "modifier required");
        assert_eq!(parse_leader("ctrl-abc"), None, "one key only");
        assert_eq!(parse_leader(""), None);
    }

    #[test]
    fn esc_returns_to_normal_from_leader() {
        let mut st = AppState::new(pane());
        st.mode = Mode::Leader;
        reduce(&mut st, &press(KeyCode::Esc));
        assert_eq!(st.mode, Mode::Normal);
    }

    // ── Roster sort. ──

    #[test]
    fn roster_sorts_by_actionability_stable_within_bucket() {
        let mut st = agents3(); // r0 r1 r2 all running
        st.agents[2].pending = Some(PendingView {
            title: "WRITE".into(),
            diff: None,
            options: vec![],
            risk: Risk::High,
        }); // r2 needs you → top
        st.agents[0].exited = true; // r0 dead → bottom
        let order = st.roster();
        assert_eq!(order, vec![2, 1, 0], "needs-you > running > dead");
    }

    #[test]
    fn roster_puts_attention_above_running() {
        let mut st = agents3();
        st.agents[1].attention = true;
        let order = st.roster();
        assert_eq!(order, vec![1, 0, 2]);
    }

    // ── Detail layout (splits are palette-only in v3). ──

    #[test]
    fn split_commands_add_most_actionable_unshown_agent() {
        let mut st = agents3(); // detail = [r0]
        run_command(&mut st, Command::SplitH);
        assert_eq!(st.detail.shown, vec!["r0".to_string(), "r1".to_string()]);
        assert_eq!(st.detail.split, Split::H);
        assert_eq!(st.detail.focus, 1, "new slot takes focus");
    }

    #[test]
    fn split_with_every_agent_shown_sets_a_notice() {
        let mut st = AppState::new(pane()); // one agent, already shown
        run_command(&mut st, Command::SplitH);
        assert_eq!(st.detail.shown, vec!["rec-1".to_string()], "unchanged");
        assert!(
            st.notice.as_deref().is_some_and(|n| n.contains("split")),
            "explains why nothing happened"
        );
    }

    #[test]
    fn split_caps_at_four_shown() {
        let mut st = agents3();
        st.agents.push(PaneState::new("r3".into(), "a3".into()));
        st.agents.push(PaneState::new("r4".into(), "a4".into()));
        for _ in 0..4 {
            run_command(&mut st, Command::SplitH);
        }
        assert_eq!(st.detail.shown.len(), 4, "fifth split is refused");
        assert!(!st.detail.shown.contains(&"r4".to_string()));
    }

    #[test]
    fn unsplit_drops_focused_slot_but_never_below_one() {
        let mut st = agents3();
        run_command(&mut st, Command::SplitH); // [r0, r1], focus 1
        run_command(&mut st, Command::Unsplit);
        assert_eq!(st.detail.shown, vec!["r0".to_string()]);
        assert_eq!(st.detail.focus, 0);
        run_command(&mut st, Command::Unsplit); // already solo — no-op
        assert_eq!(st.detail.shown, vec!["r0".to_string()]);
    }

    #[test]
    fn spawn_command_opens_picker_with_available_agents() {
        let mut st = AppState::new(pane());
        st.available_agents = vec!["fake".into(), "claude-acp".into()];
        run_command(&mut st, Command::SpawnAgent);
        assert_eq!(st.mode, Mode::Picker);
        let p = st.picker.as_ref().expect("picker set");
        assert_eq!(p.agents, vec!["fake".to_string(), "claude-acp".to_string()]);
        assert_eq!(p.selected, 0);
        assert_eq!(p.purpose, PickerPurpose::Subagent);
    }

    // ── Close (leader `c` acts on the focused pane). ──

    #[test]
    fn leader_c_closes_focused_agent_and_emits_close_agent() {
        let mut st = agents3();
        st.detail = DetailLayout::solo("r1".into());
        st.mode = Mode::Leader;
        let fx = reduce(&mut st, &press(KeyCode::Char('c')));
        assert_eq!(
            fx,
            vec![Effect::CloseAgent {
                record_id: "r1".into()
            }]
        );
        assert_eq!(st.mode, Mode::Normal, "one-shot");
        assert_eq!(st.agents.len(), 2);
        assert_eq!(st.agents[0].record_id, "r0");
        assert_eq!(st.agents[1].record_id, "r2");
        assert!(!st.should_quit);
    }

    #[test]
    fn leader_c_on_last_agent_sets_should_quit() {
        let mut st = AppState::new(pane());
        st.mode = Mode::Leader;
        let fx = reduce(&mut st, &press(KeyCode::Char('c')));
        assert_eq!(
            fx,
            vec![Effect::CloseAgent {
                record_id: "rec-1".into()
            }]
        );
        assert!(st.should_quit);
        assert!(st.agents.is_empty());
    }

    #[test]
    fn closing_the_shown_agent_refills_detail_with_roster_head() {
        let mut st = agents3(); // detail = [r0]
        st.agents[2].attention = true; // r2 = roster head after r0 closes
        st.mode = Mode::Leader;
        let fx = reduce(&mut st, &press(KeyCode::Char('c')));
        assert_eq!(
            fx,
            vec![Effect::CloseAgent {
                record_id: "r0".into()
            }]
        );
        assert_eq!(
            st.detail.shown,
            vec!["r2".to_string()],
            "detail refilled with the most actionable agent"
        );
    }

    // ── Polish: leader Tab (next actionable), wheel scroll. ──

    #[test]
    fn y_resolves_the_top_pending_and_advances_to_the_next() {
        let mut st = agents3(); // detail = [r0]
        reduce(&mut st, &perm("r2", "older wants"));
        reduce(&mut st, &perm("r1", "newer wants"));
        // Top of the queue = r2 (same risk, oldest first) — resolved even
        // though r0 holds the focus.
        let fx = reduce(&mut st, &press(KeyCode::Char('y')));
        assert_eq!(
            fx,
            vec![Effect::ResolvePermission {
                record_id: "r2".into(),
                outcome: PermissionOutcome::AllowOnce,
            }]
        );
        assert!(st.agents[2].pending.is_none(), "top item cleared");
        assert_eq!(
            st.detail.shown,
            vec!["r1".to_string()],
            "focus advances to the next pending item (batch clear)"
        );
        // The next `n` denies r1's — queue drained.
        let fx = reduce(&mut st, &press(KeyCode::Char('n')));
        assert_eq!(
            fx,
            vec![Effect::ResolvePermission {
                record_id: "r1".into(),
                outcome: PermissionOutcome::Deny,
            }]
        );
        assert!(st.agents.iter().all(|p| p.pending.is_none()));
    }

    #[test]
    fn leader_p_opens_the_command_palette() {
        let mut st = AppState::new(pane());
        st.mode = Mode::Leader;
        let fx = reduce(&mut st, &press(KeyCode::Char('p')));
        assert!(fx.is_empty());
        assert_eq!(st.mode, Mode::Command);
        assert!(st.palette.is_some(), "palette overlay armed");
    }

    #[test]
    fn leader_leaves_are_one_shot() {
        // Every leaf leaves `Leader` in exactly one key: back to NORMAL, or
        // into a Command/Picker leaf — never a sticky mode (TUI_SPEC_V3 I3).
        // Driven from LEADER_LEAVES so a new leaf is covered automatically.
        let keys = [KeyCode::Char('1'), KeyCode::Esc]
            .into_iter()
            .chain(LEADER_LEAVES.iter().map(|&(key, _, _)| key));
        for key in keys {
            let mut st = agents3();
            st.available_sessions = vec!["claude".into()];
            st.mode = Mode::Leader;
            reduce(&mut st, &press(key));
            assert!(
                matches!(st.mode, Mode::Normal | Mode::Picker | Mode::Command),
                "{key:?} must leave Leader in one key, got {:?}",
                st.mode
            );
            assert_ne!(st.mode, Mode::Leader, "{key:?} left the prefix armed");
        }
    }

    #[test]
    fn leader_table_is_the_single_source_for_dispatch_and_help() {
        // TUI_SPEC_V3 §9 keyboard parity: every documented leaf dispatches
        // to exactly the action its table row declares (the overlay renders
        // the same rows, so binding and help line cannot disagree).
        for &(key, what, action) in LEADER_LEAVES {
            assert_eq!(
                leader_action(key),
                Some(action),
                "table row {what:?} must dispatch its own action"
            );
        }
        // The two hand rows: digits focus sessions, Esc cancels.
        assert_eq!(
            leader_action(KeyCode::Char('3')),
            Some(LeaderAction::FocusSession(2)),
            "digits are 1-based session ordinals"
        );
        assert_eq!(leader_action(KeyCode::Esc), None, "Esc cancels the prefix");
    }

    #[test]
    fn leader_tab_focuses_the_next_actionable_agent() {
        let mut st = agents3(); // detail = [r0]
        reduce(&mut st, &perm("r2", "wants")); // r2 needs you
        st.mode = Mode::Leader;
        reduce(&mut st, &press(KeyCode::Tab));
        assert_eq!(st.mode, Mode::Normal, "one-shot");
        assert_eq!(
            st.detail.shown,
            vec!["r2".to_string()],
            "the actionable agent takes the detail"
        );
    }

    #[test]
    fn wheel_scroll_pages_acp_and_forwards_to_pty() {
        let mut st = AppState::new(pane());
        st.agents[0].viewport = 10;
        for i in 0..50 {
            reduce(&mut st, &msg(i));
        }
        reduce(&mut st, &AppEvent::Scroll { up: true });
        assert_eq!(st.agents[0].scroll, Some(30), "wheel pages the scrollback");
        reduce(&mut st, &AppEvent::Scroll { up: false });
        assert_eq!(st.agents[0].scroll, None, "back to following");

        let mut st = AppState::new(pane());
        st.agents[0].kind = PaneKind::Pty;
        let fx = reduce(&mut st, &AppEvent::Scroll { up: true });
        assert_eq!(fx.len(), 3, "PTY pane gets arrow presses");
        assert!(matches!(&fx[0], Effect::PtyKey { .. }));
    }

    // ── Attach (§13-B4). ──

    #[test]
    fn leader_t_attaches_the_focused_live_monitor_only() {
        let mut st = agents3();
        st.detail = DetailLayout::solo("r1".into());
        st.mode = Mode::Leader;
        let fx = reduce(&mut st, &press(KeyCode::Char('t')));
        assert_eq!(
            fx,
            vec![Effect::Attach {
                record_id: "r1".into()
            }]
        );
        assert_eq!(st.mode, Mode::Normal, "one-shot");
        // A session can't attach to itself.
        let mut pty = PaneState::new("session-1".into(), "claude".into());
        pty.kind = PaneKind::Pty;
        st.agents.push(pty);
        st.detail = DetailLayout::solo("session-1".into());
        st.mode = Mode::Leader;
        assert!(reduce(&mut st, &press(KeyCode::Char('t'))).is_empty());
        // A dead agent has nothing to drive.
        st.agents[1].exited = true;
        st.detail = DetailLayout::solo("r1".into());
        st.mode = Mode::Leader;
        assert!(reduce(&mut st, &press(KeyCode::Char('t'))).is_empty());
    }

    #[test]
    fn pty_attached_adds_a_solo_pty_pane() {
        let mut st = agents3();
        reduce(
            &mut st,
            &AppEvent::PtyAttached {
                record_id: "attach:r1".into(),
                agent_id: "claude⤴a1".into(),
            },
        );
        let pane = st
            .agents
            .iter()
            .find(|p| p.record_id == "attach:r1")
            .expect("attach pane added");
        assert_eq!(pane.kind, PaneKind::Pty);
        assert_eq!(st.detail.shown, vec!["attach:r1".to_string()], "solo");
        assert_eq!(st.mode, Mode::Normal, "keys route to the attach");
        assert!(st.notice.as_deref().is_some_and(|n| n.contains("detach")));
    }

    // ── Sessions panel (sessions left, subagents right). ──

    /// Two sessions (a PTY orchestrator + one more) beside two ACP agents.
    fn fleet_state() -> AppState {
        let mut orch = PaneState::new("orchestrator".into(), "claude".into());
        orch.kind = PaneKind::Pty;
        orch.harness = "pty".into();
        let mut st = AppState::new(orch);
        st.agents.push(PaneState::new("r1".into(), "a1".into()));
        st.agents.push(PaneState::new("r2".into(), "a2".into()));
        let mut s2 = PaneState::new("session-1".into(), "codex".into());
        s2.kind = PaneKind::Pty;
        s2.harness = "pty".into();
        st.agents.push(s2);
        st
    }

    #[test]
    fn roster_lists_acp_panes_and_sessions_list_pty_panes() {
        let st = fleet_state();
        let roster: Vec<&str> = st
            .roster()
            .into_iter()
            .map(|i| st.agents[i].record_id.as_str())
            .collect();
        assert_eq!(roster, vec!["r1", "r2"], "roster = ACP only");
        let sessions: Vec<&str> = st
            .sessions_list()
            .into_iter()
            .map(|i| st.agents[i].record_id.as_str())
            .collect();
        assert_eq!(
            sessions,
            vec!["orchestrator", "session-1"],
            "sessions = PTY panes in spawn order"
        );
    }

    #[test]
    fn leader_digit_focuses_session_n() {
        let mut st = fleet_state(); // sessions: orchestrator, session-1
        st.mode = Mode::Leader;
        reduce(&mut st, &press(KeyCode::Char('2')));
        assert_eq!(st.mode, Mode::Normal, "one-shot");
        assert_eq!(st.detail.shown, vec!["session-1".to_string()]);
        // Out of range → notice, focus untouched.
        st.mode = Mode::Leader;
        reduce(&mut st, &press(KeyCode::Char('9')));
        assert_eq!(st.detail.shown, vec!["session-1".to_string()]);
        assert!(
            st.notice
                .as_deref()
                .is_some_and(|n| n.contains("no session"))
        );
    }

    #[test]
    fn leader_n_opens_the_session_picker_and_enter_spawns() {
        let mut st = fleet_state();
        st.available_sessions = vec!["claude".into(), "codex".into()];
        st.mode = Mode::Leader;
        reduce(&mut st, &press(KeyCode::Char('n')));
        assert_eq!(st.mode, Mode::Picker);
        let picker = st.picker.as_ref().expect("picker");
        assert_eq!(picker.purpose, PickerPurpose::Session);
        assert_eq!(picker.agents, vec!["claude".to_string(), "codex".into()]);
        reduce(&mut st, &press(KeyCode::Down));
        let fx = reduce(&mut st, &press(KeyCode::Enter));
        assert_eq!(
            fx,
            vec![Effect::SpawnSession {
                binary: "codex".into()
            }],
            "session spawn bypasses the worktree-bootstrap confirm"
        );
    }

    #[test]
    fn session_spawned_adds_a_solo_pty_pane_with_model() {
        let mut st = fleet_state();
        reduce(
            &mut st,
            &AppEvent::SessionSpawned {
                record_id: "session-2".into(),
                binary: "codex".into(),
                model: Some("supergrok:grok-4.5".into()),
            },
        );
        let pane = st
            .agents
            .iter()
            .find(|p| p.record_id == "session-2")
            .expect("session pane added");
        assert_eq!(pane.kind, PaneKind::Pty);
        assert!(pane.is_session());
        assert_eq!(pane.model.as_deref(), Some("supergrok:grok-4.5"));
        assert_eq!(st.detail.shown, vec!["session-2".to_string()], "solo");
    }

    #[test]
    fn leader_c_closes_a_focused_session() {
        let mut st = fleet_state();
        st.detail = DetailLayout::solo("session-1".into());
        st.mode = Mode::Leader;
        let fx = reduce(&mut st, &press(KeyCode::Char('c')));
        assert_eq!(
            fx,
            vec![Effect::CloseAgent {
                record_id: "session-1".into()
            }]
        );
        assert!(!st.agents.iter().any(|p| p.record_id == "session-1"));
    }

    #[test]
    fn review_verbs_never_fire_on_a_focused_session() {
        // A focused PTY session owns the keyboard: review keys pass through
        // to the child instead of firing on some other pane's review.
        let mut st = fleet_state(); // detail = [orchestrator], a PTY
        st.agents[1].review = Some((1, 2, 3));
        for key in ['m', 'p', 'D', 'r'] {
            let fx = reduce(&mut st, &press(KeyCode::Char(key)));
            assert!(
                matches!(&fx[..], [Effect::PtyKey { .. }]),
                "'{key}' routes to the PTY child: {fx:?}"
            );
        }
        assert!(st.agents[1].review.is_some(), "review untouched");
    }

    #[test]
    fn fleet_sessions_reports_binaries_and_models_but_not_attaches() {
        let mut st = fleet_state();
        st.agents[0].model = Some("supergrok:grok-4.5".into());
        let mut attach = PaneState::new("attach:r1".into(), "claude⤴a1".into());
        attach.kind = PaneKind::Pty;
        attach.harness = "attach".into();
        st.agents.push(attach);
        let sessions = st.fleet_sessions();
        let binaries: Vec<&str> = sessions.iter().map(|s| s.binary.as_str()).collect();
        assert_eq!(binaries, vec!["claude", "codex"], "attach pane skipped");
        assert_eq!(sessions[0].model.as_deref(), Some("supergrok:grok-4.5"));
    }

    #[test]
    fn palette_toggles_collapse_the_sidebars() {
        let mut st = fleet_state();
        let _ = run_command(&mut st, Command::ToggleSessions);
        let _ = run_command(&mut st, Command::ToggleSubagents);
        assert!(st.sessions_collapsed && st.subagents_collapsed);
        let _ = run_command(&mut st, Command::ToggleSessions);
        assert!(!st.sessions_collapsed, "toggle back");
    }

    #[test]
    fn notices_decay_after_a_few_seconds_of_ticks() {
        let mut st = fleet_state();
        reduce(
            &mut st,
            &AppEvent::AgentSpawnFailed {
                agent_id: "a1".into(),
                error: "boom".into(),
            },
        );
        assert!(st.notice.is_some());
        for _ in 0..NOTICE_DECAY_TICKS {
            reduce(&mut st, &AppEvent::Tick);
        }
        assert!(st.notice.is_some(), "still visible inside the window");
        reduce(&mut st, &AppEvent::Tick);
        assert!(st.notice.is_none(), "decayed one tick past the window");
    }

    #[test]
    fn serve_status_updates_the_daemon_dot() {
        let mut st = fleet_state();
        assert_eq!(st.serve_ok, None);
        reduce(&mut st, &AppEvent::ServeStatus { ok: true });
        assert_eq!(st.serve_ok, Some(true));
        reduce(&mut st, &AppEvent::ServeStatus { ok: false });
        assert_eq!(st.serve_ok, Some(false));
    }

    // ── Picker. ──

    fn picker_state(agents: &[&str]) -> AppState {
        let mut st = AppState::new(pane());
        let agents: Vec<String> = agents.iter().map(|s| s.to_string()).collect();
        st.available_agents = agents.clone();
        st.mode = Mode::Picker;
        st.picker = Some(PickerState {
            agents,
            selected: 0,
            purpose: PickerPurpose::Subagent,
        });
        st
    }

    #[test]
    fn picker_down_then_up_clamps_at_bounds() {
        let mut st = picker_state(&["a", "b", "c"]);
        let down = |st: &mut AppState| {
            reduce(st, &press(KeyCode::Down));
        };
        let up = |st: &mut AppState| {
            reduce(st, &press(KeyCode::Up));
        };
        down(&mut st);
        assert_eq!(st.picker.as_ref().expect("p").selected, 1);
        down(&mut st);
        assert_eq!(st.picker.as_ref().expect("p").selected, 2);
        down(&mut st);
        assert_eq!(st.picker.as_ref().expect("p").selected, 2); // clamp
        up(&mut st);
        assert_eq!(st.picker.as_ref().expect("p").selected, 1);
        up(&mut st);
        assert_eq!(st.picker.as_ref().expect("p").selected, 0);
        up(&mut st);
        assert_eq!(st.picker.as_ref().expect("p").selected, 0); // clamp
    }

    #[test]
    fn picker_enter_spawns_selected_and_returns_to_normal() {
        let mut st = picker_state(&["fake", "claude"]);
        reduce(&mut st, &press(KeyCode::Down)); // select "claude"
        let fx = reduce(&mut st, &press(KeyCode::Enter));
        assert_eq!(
            fx,
            vec![Effect::SpawnAgent {
                agent_id: "claude".into()
            }]
        );
        assert_eq!(st.mode, Mode::Normal);
        assert!(st.picker.is_none());
    }

    #[test]
    fn picker_esc_cancels_with_no_effect() {
        let mut st = picker_state(&["fake"]);
        let fx = reduce(&mut st, &press(KeyCode::Esc));
        assert!(fx.is_empty());
        assert_eq!(st.mode, Mode::Normal);
        assert!(st.picker.is_none());
    }

    #[test]
    fn picker_enter_on_empty_list_just_closes() {
        let mut st = picker_state(&[]);
        let fx = reduce(&mut st, &press(KeyCode::Enter));
        assert!(fx.is_empty());
        assert_eq!(st.mode, Mode::Normal);
        assert!(st.picker.is_none());
    }

    // ── Bootstrap confirm (worktree hook gating). ──

    fn picker_with_bootstrap() -> AppState {
        let mut st = picker_state(&["fake"]);
        st.bootstrap_cmd = Some("cp $BITROUTER_BASE_REPO/.env .".into());
        st
    }

    #[test]
    fn first_spawn_with_bootstrap_asks_instead_of_spawning() {
        let mut st = picker_with_bootstrap();
        let fx = reduce(&mut st, &press(KeyCode::Enter));
        assert!(fx.is_empty(), "the hook executes shell — ask first");
        assert_eq!(st.mode, Mode::Confirm);
        assert_eq!(st.confirm_agent.as_deref(), Some("fake"));
        assert_eq!(st.bootstrap_decision, None);
    }

    #[test]
    fn confirm_y_approves_for_the_session_and_releases_the_spawn() {
        let mut st = picker_with_bootstrap();
        reduce(&mut st, &press(KeyCode::Enter));
        let fx = reduce(&mut st, &press(KeyCode::Char('y')));
        // The grant also broadcasts to connected MCP bridges (Unix), then
        // releases the pending spawn.
        #[cfg(unix)]
        assert_eq!(
            fx,
            vec![
                Effect::BridgeBootstrapApproved,
                Effect::SpawnAgent {
                    agent_id: "fake".into()
                }
            ]
        );
        #[cfg(not(unix))]
        assert_eq!(
            fx,
            vec![Effect::SpawnAgent {
                agent_id: "fake".into()
            }]
        );
        assert_eq!(st.bootstrap_decision, Some(true));
        assert_eq!(st.mode, Mode::Normal);
        assert!(st.confirm_agent.is_none());
        // Second spawn: decided — no re-ask.
        let fx = request_spawn(&mut st, "fake".into());
        assert_eq!(fx.len(), 1, "asked once per session");
    }

    #[test]
    fn confirm_n_skips_bootstrap_but_still_spawns() {
        let mut st = picker_with_bootstrap();
        reduce(&mut st, &press(KeyCode::Enter));
        let fx = reduce(&mut st, &press(KeyCode::Char('n')));
        assert_eq!(
            fx,
            vec![Effect::SpawnAgent {
                agent_id: "fake".into()
            }]
        );
        assert_eq!(st.bootstrap_decision, Some(false));
    }

    #[test]
    fn confirm_esc_cancels_the_spawn_and_keeps_asking_next_time() {
        let mut st = picker_with_bootstrap();
        reduce(&mut st, &press(KeyCode::Enter));
        let fx = reduce(&mut st, &press(KeyCode::Esc));
        assert!(fx.is_empty(), "cancelled — nothing spawns");
        assert_eq!(st.mode, Mode::Normal);
        assert_eq!(st.bootstrap_decision, None, "undecided: ask again");
        assert!(st.confirm_agent.is_none());
    }

    #[test]
    fn spawn_without_bootstrap_config_never_asks() {
        let mut st = picker_state(&["fake"]);
        let fx = reduce(&mut st, &press(KeyCode::Enter));
        assert_eq!(
            fx,
            vec![Effect::SpawnAgent {
                agent_id: "fake".into()
            }]
        );
        assert_eq!(st.mode, Mode::Normal);
    }

    #[test]
    fn agent_spawned_records_the_allocated_port() {
        let mut st = AppState::new(pane());
        reduce(
            &mut st,
            &AppEvent::AgentSpawned {
                record_id: "r9".into(),
                agent_id: "fake".into(),
                port: Some(3101),
            },
        );
        assert_eq!(st.agents[1].port, Some(3101));
    }

    // ── Review queue (TUI_SPEC §7). ──

    fn review_ready(record_id: &str) -> AppEvent {
        AppEvent::ReviewReady {
            record_id: record_id.into(),
            files: 2,
            adds: 10,
            dels: 3,
        }
    }

    #[test]
    fn clean_turn_end_asks_the_loop_to_check_review() {
        let mut st = AppState::new(pane());
        let fx = reduce(
            &mut st,
            &AppEvent::TurnEnded {
                record_id: "rec-1".into(),
                stop_reason: StopReason::EndTurn,
            },
        );
        assert_eq!(
            fx,
            vec![Effect::CheckReview {
                record_id: "rec-1".into()
            }]
        );
        // Abnormal ends don't feed the review queue.
        let fx = reduce(
            &mut st,
            &AppEvent::TurnEnded {
                record_id: "rec-1".into(),
                stop_reason: StopReason::Cancelled,
            },
        );
        assert!(fx.is_empty());
    }

    #[test]
    fn review_ready_sets_state_and_sorts_to_rail_head() {
        let mut st = agents3();
        reduce(&mut st, &review_ready("r2"));
        assert_eq!(st.agents[2].review, Some((2, 10, 3)));
        assert!(matches!(
            st.agents[2].lines.last(),
            Some(Line::Note(n)) if n.contains("+10/-3")
        ));
        let order = st.roster();
        assert_eq!(order[0], 2, "review outranks idle agents");
        // But needs-you still outranks review.
        reduce(&mut st, &perm("r1", "wants"));
        assert_eq!(st.roster()[0], 1, "pending beats review");
    }

    #[test]
    fn review_keys_emit_integration_effects() {
        let mut st = agents3();
        reduce(&mut st, &review_ready("r1"));
        st.detail = DetailLayout::solo("r1".into()); // review inline on focus

        let fx = reduce(&mut st, &press(KeyCode::Char('m')));
        assert_eq!(
            fx,
            vec![Effect::Merge {
                record_id: "r1".into()
            }]
        );
        let fx = reduce(&mut st, &press(KeyCode::Char('p')));
        assert_eq!(
            fx,
            vec![Effect::Apply {
                record_id: "r1".into()
            }]
        );
        let fx = reduce(&mut st, &press(KeyCode::Char('D')));
        assert_eq!(
            fx,
            vec![Effect::LoadDiff {
                record_id: "r1".into()
            }]
        );
    }

    #[test]
    fn review_keys_are_inert_without_review_state() {
        let mut st = agents3(); // detail = [r0], no review
        for c in ['m', 'p', 'D'] {
            let fx = reduce(&mut st, &press(KeyCode::Char(c)));
            assert!(fx.is_empty(), "'{c}' without review state is a no-op");
        }
    }

    #[test]
    fn reject_human_owned_reprompts() {
        let mut st = agents3(); // human-owned monitors (Ownership::Human)
        reduce(&mut st, &review_ready("r1"));
        st.detail = DetailLayout::solo("r1".into());
        let fx = reduce(&mut st, &press(KeyCode::Char('r')));
        assert_eq!(
            fx,
            vec![Effect::Prompt {
                record_id: "r1".into(),
                text: REJECT_NOTE.into(),
            }],
            "the human owns the hatch spawn — reject re-prompts it directly"
        );
        assert!(st.agents[1].review.is_none(), "review cleared");
        assert!(st.agents[1].turn_active, "the revision turn is in flight");
    }

    #[cfg(unix)]
    #[test]
    fn reject_orchestrator_owned_sets_task_outcome() {
        let mut st = AppState::new(pane());
        spawn_mirror(&mut st); // mcp:abc123, Ownership::Orchestrator
        reduce(&mut st, &review_ready("mcp:abc123"));
        st.detail = DetailLayout::solo("mcp:abc123".into());
        let fx = reduce(&mut st, &press(KeyCode::Char('r')));
        assert_eq!(
            fx,
            vec![Effect::ReviewVerdict {
                record_id: "mcp:abc123".into(),
                note: REJECT_NOTE.into(),
            }],
            "the verdict is the subagent's task outcome — never a prompt"
        );
        assert!(
            !fx.iter().any(|f| matches!(f, Effect::Prompt { .. })),
            "no prompt reaches an orchestrator-owned subagent"
        );
        let mirror = st.agents.iter().find(|p| p.record_id == "mcp:abc123");
        assert!(mirror.is_some_and(|p| p.review.is_none()), "review cleared");
    }

    #[cfg(unix)]
    #[test]
    fn reject_on_a_disconnected_orchestrator_is_honest() {
        // Bridge gone (pane exited): there is no consumer for the verdict —
        // rejecting must not emit ReviewVerdict or claim it was routed.
        let mut st = AppState::new(pane());
        spawn_mirror(&mut st); // mcp:abc123, Ownership::Orchestrator
        reduce(&mut st, &review_ready("mcp:abc123"));
        reduce(
            &mut st,
            &AppEvent::BridgeGone {
                record_ids: vec!["mcp:abc123".into()],
            },
        );
        st.detail = DetailLayout::solo("mcp:abc123".into());
        let fx = reduce(&mut st, &press(KeyCode::Char('r')));
        assert!(fx.is_empty(), "no verdict effect for a dead bridge: {fx:?}");
        assert!(
            st.notice
                .as_deref()
                .is_some_and(|n| n.contains("disconnected") && !n.contains("routed")),
            "the notice must not claim delivery: {:?}",
            st.notice
        );
        let mirror = st.agents.iter().find(|p| p.record_id == "mcp:abc123");
        assert!(mirror.is_some_and(|p| p.review.is_none()), "review cleared");
    }

    #[test]
    fn reject_clears_review_and_opens_the_pane() {
        let mut st = agents3();
        reduce(&mut st, &review_ready("r1"));
        st.detail = DetailLayout::solo("r1".into());
        let fx = reduce(&mut st, &press(KeyCode::Char('r')));
        assert_eq!(fx.len(), 1, "one routed rejection effect: {fx:?}");
        assert!(st.agents[1].review.is_none(), "review cleared");
        assert_eq!(st.mode, Mode::Normal);
        assert!(st.notice.as_deref().is_some_and(|n| n.contains("rejected")));
        // Monitors are read-only: typing after a reject stays inert.
        let fx = reduce(&mut st, &press(KeyCode::Enter));
        assert!(
            !fx.iter().any(|f| matches!(f, Effect::Prompt { .. })),
            "no keyboard prompt path exists: {fx:?}"
        );
    }

    #[test]
    fn successful_op_clears_review_failed_op_keeps_it() {
        let mut st = agents3();
        reduce(&mut st, &review_ready("r1"));
        reduce(
            &mut st,
            &AppEvent::OpDone {
                record_id: "r1".into(),
                message: "merge failed: dirty".into(),
                ok: false,
            },
        );
        assert!(
            st.agents[1].review.is_some(),
            "failed op keeps the queue item"
        );
        assert!(matches!(st.agents[1].lines.last(), Some(Line::Error(_))));
        reduce(
            &mut st,
            &AppEvent::OpDone {
                record_id: "r1".into(),
                message: "merged bitrouter/a1-x".into(),
                ok: true,
            },
        );
        assert!(st.agents[1].review.is_none(), "merged — out of the queue");
        assert!(matches!(st.agents[1].lines.last(), Some(Line::Note(_))));
    }

    #[test]
    fn failing_checks_loop_back_to_the_agent_then_surface() {
        let mut st = AppState::new(pane());
        let fail = |st: &mut AppState| {
            reduce(
                st,
                &AppEvent::ChecksFailed {
                    record_id: "rec-1".into(),
                    output: "test x failed".into(),
                },
            )
        };
        // First two failures: feedback goes back to the agent, not the human.
        for retry in 1..=2u8 {
            let fx = fail(&mut st);
            assert_eq!(fx.len(), 1, "retry {retry} re-prompts the agent");
            assert!(matches!(
                &fx[0],
                Effect::Prompt { record_id, text }
                    if record_id == "rec-1" && text.contains("test x failed")
            ));
            assert!(st.agents[0].turn_active, "agent is working again");
            assert!(st.agents[0].review.is_none());
        }
        // Third: retries exhausted — the human decides.
        let fx = fail(&mut st);
        assert!(
            !fx.iter().any(|f| matches!(f, Effect::Prompt { .. })),
            "no endless retry loop"
        );
        assert!(st.agents[0].review.is_some(), "surfaces for manual review");
        assert!(matches!(
            st.agents[0].lines.last(),
            Some(Line::Error(e)) if e.contains("review manually")
        ));
    }

    // ── Attention. ──

    #[test]
    fn permission_on_background_pane_sets_attention_and_bell() {
        let mut st = agents3(); // detail shows only r0
        let fx = reduce(
            &mut st,
            &AppEvent::Permission {
                record_id: "r1".into(),
                title: "WRITE".into(),
                diff: None,
                options: vec![],
                risk: Risk::High,
            },
        );
        assert!(st.agents[1].attention);
        assert!(fx.contains(&Effect::Bell));
    }

    #[test]
    fn permission_on_shown_pane_no_attention_no_bell() {
        let mut st = agents3();
        let fx = reduce(
            &mut st,
            &AppEvent::Permission {
                record_id: "r0".into(),
                title: "WRITE".into(),
                diff: None,
                options: vec![],
                risk: Risk::High,
            },
        );
        assert!(!st.agents[0].attention);
        assert!(!fx.contains(&Effect::Bell));
    }

    #[test]
    fn exit_on_background_pane_sets_attention_and_bell() {
        let mut st = agents3();
        let fx = reduce(
            &mut st,
            &AppEvent::Exited {
                record_id: "r2".into(),
            },
        );
        assert!(st.agents[2].exited);
        assert!(st.agents[2].attention);
        assert!(fx.contains(&Effect::Bell));
    }

    #[test]
    fn permission_on_split_shown_pane_is_not_background() {
        let mut st = agents3();
        run_command(&mut st, Command::SplitH); // show r0 + r1
        let fx = reduce(
            &mut st,
            &AppEvent::Permission {
                record_id: "r1".into(),
                title: "WRITE".into(),
                diff: None,
                options: vec![],
                risk: Risk::High,
            },
        );
        assert!(
            !st.agents[1].attention,
            "visible in a split — no attention needed"
        );
        assert!(!fx.contains(&Effect::Bell));
    }

    // ── Decision queue. ──

    fn perm(record_id: &str, title: &str) -> AppEvent {
        perm_with_risk(record_id, title, Risk::High)
    }

    fn perm_with_risk(record_id: &str, title: &str, risk: Risk) -> AppEvent {
        AppEvent::Permission {
            record_id: record_id.into(),
            title: title.into(),
            diff: None,
            options: vec![],
            risk,
        }
    }

    #[test]
    fn queue_orders_pending_by_age_oldest_first() {
        let mut st = agents3();
        reduce(&mut st, &perm("r2", "second wants"));
        reduce(&mut st, &perm("r1", "third wants"));
        // r2's request arrived before r1's → r2 tops the queue.
        let order = st.roster();
        assert_eq!(order[0], 2, "oldest pending first");
        assert_eq!(order[1], 1);
        assert_eq!(order[2], 0, "running agent below the queue");
    }

    #[test]
    fn dead_agents_pending_leaves_the_queue() {
        let mut st = agents3();
        reduce(&mut st, &perm("r1", "wants"));
        assert_eq!(st.roster()[0], 1);
        reduce(
            &mut st,
            &AppEvent::Exited {
                record_id: "r1".into(),
            },
        );
        assert!(
            st.agents[1].pending.is_none(),
            "a dead agent's decision is moot"
        );
    }

    // ── Tiered autonomy. ──

    #[test]
    fn manual_surfaces_every_request_even_low_risk() {
        let mut st = agents3(); // default Manual
        let fx = reduce(&mut st, &perm_with_risk("r0", "read file", Risk::Low));
        assert!(fx.is_empty(), "shown pane, no bell; nothing auto-resolves");
        assert!(st.agents[0].pending.is_some(), "manual always surfaces");
    }

    #[test]
    fn assisted_auto_allows_low_risk_and_logs_it() {
        let mut st = agents3();
        st.agents[0].autonomy = Autonomy::Assisted;
        let fx = reduce(&mut st, &perm_with_risk("r0", "edit src/x.rs", Risk::Low));
        assert_eq!(
            fx,
            vec![Effect::ResolvePermission {
                record_id: "r0".into(),
                outcome: PermissionOutcome::AllowOnce,
            }]
        );
        assert!(st.agents[0].pending.is_none(), "nothing surfaces");
        assert!(
            matches!(
                st.agents[0].lines.last(),
                Some(Line::AutoResolved(l)) if l.contains("assisted") && l.contains("edit src/x.rs")
            ),
            "auto-resolve is logged, never silent"
        );
    }

    #[test]
    fn assisted_surfaces_high_risk() {
        let mut st = agents3();
        st.agents[0].autonomy = Autonomy::Assisted;
        let fx = reduce(&mut st, &perm_with_risk("r0", "rm -rf legacy", Risk::High));
        assert!(fx.is_empty());
        assert!(st.agents[0].pending.is_some(), "high risk reaches the user");
    }

    #[test]
    fn auto_allows_even_high_risk_and_logs_it() {
        let mut st = agents3();
        st.agents[0].autonomy = Autonomy::Auto;
        let fx = reduce(&mut st, &perm_with_risk("r0", "rm -rf legacy", Risk::High));
        assert_eq!(fx.len(), 1, "resolved without surfacing");
        assert!(st.agents[0].pending.is_none());
        assert!(matches!(
            st.agents[0].lines.last(),
            Some(Line::AutoResolved(l)) if l.contains("auto")
        ));
    }

    #[test]
    fn leader_a_cycles_autonomy_and_logs() {
        let mut st = agents3(); // detail = [r0]
        st.mode = Mode::Leader;
        reduce(&mut st, &press(KeyCode::Char('a')));
        assert_eq!(st.agents[0].autonomy, Autonomy::Assisted);
        assert_eq!(st.mode, Mode::Normal, "one-shot");
        st.mode = Mode::Leader;
        reduce(&mut st, &press(KeyCode::Char('a')));
        assert_eq!(st.agents[0].autonomy, Autonomy::Auto);
        st.mode = Mode::Leader;
        reduce(&mut st, &press(KeyCode::Char('a')));
        assert_eq!(st.agents[0].autonomy, Autonomy::Manual, "cycles back");
        assert!(
            matches!(st.agents[0].lines.last(), Some(Line::AutoResolved(l)) if l.contains("manual")),
            "tier changes are logged in the pane"
        );
        assert_eq!(st.agents[1].autonomy, Autonomy::Manual, "per-agent only");
    }

    #[test]
    fn queue_orders_high_risk_above_older_low_risk() {
        let mut st = agents3();
        reduce(&mut st, &perm_with_risk("r0", "older low", Risk::Low));
        reduce(&mut st, &perm_with_risk("r1", "newer high", Risk::High));
        let order = st.roster();
        assert_eq!(order[0], 1, "high risk outranks age");
        assert_eq!(order[1], 0);
    }

    // ── Command palette + which-key. ──

    #[test]
    fn colon_opens_the_command_palette() {
        let mut st = AppState::new(pane());
        reduce(&mut st, &press(KeyCode::Char(':')));
        assert_eq!(st.mode, Mode::Command);
        assert!(st.palette.is_some());
    }

    #[test]
    fn palette_fuzzy_filters_by_subsequence() {
        let p = PaletteState {
            input: "spw".into(),
            selected: 0,
        };
        let names: Vec<&str> = p.matches().iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["spawn subagent"], "s-p-w subsequence");

        let none = PaletteState {
            input: "zzz".into(),
            selected: 0,
        };
        assert!(none.matches().is_empty());

        let all = PaletteState::default();
        assert_eq!(all.matches().len(), COMMANDS.len(), "empty filter = all");
    }

    #[test]
    fn palette_enter_runs_the_selected_command() {
        let mut st = AppState::new(pane());
        reduce(&mut st, &press(KeyCode::Char(':')));
        for c in "quit".chars() {
            reduce(&mut st, &press(KeyCode::Char(c)));
        }
        let fx = reduce(&mut st, &press(KeyCode::Enter));
        assert_eq!(fx, vec![Effect::Quit]);
        assert!(st.should_quit);
    }

    #[test]
    fn palette_enter_with_no_match_just_closes() {
        let mut st = AppState::new(pane());
        reduce(&mut st, &press(KeyCode::Char(':')));
        for c in "zzz".chars() {
            reduce(&mut st, &press(KeyCode::Char(c)));
        }
        let fx = reduce(&mut st, &press(KeyCode::Enter));
        assert!(fx.is_empty(), "no match → no action, no panic");
        assert_eq!(st.mode, Mode::Normal);
        assert!(st.palette.is_none());
    }

    #[test]
    fn palette_spawn_opens_picker() {
        let mut st = AppState::new(pane());
        st.available_agents = vec!["fake".into()];
        reduce(&mut st, &press(KeyCode::Char(':')));
        for c in "spawn".chars() {
            reduce(&mut st, &press(KeyCode::Char(c)));
        }
        reduce(&mut st, &press(KeyCode::Enter));
        assert_eq!(st.mode, Mode::Picker);
        assert!(st.picker.is_some());
    }

    #[test]
    fn palette_kill_done_closes_only_exited_agents() {
        let mut st = agents3();
        st.agents[1].exited = true;
        st.agents[2].exited = true;
        reduce(&mut st, &press(KeyCode::Char(':')));
        for c in "kill".chars() {
            reduce(&mut st, &press(KeyCode::Char(c)));
        }
        let fx = reduce(&mut st, &press(KeyCode::Enter));
        assert_eq!(fx.len(), 2, "two dead agents closed");
        assert_eq!(st.agents.len(), 1);
        assert_eq!(st.agents[0].record_id, "r0");
        assert!(!st.should_quit);
    }

    #[test]
    fn palette_esc_cancels() {
        let mut st = AppState::new(pane());
        reduce(&mut st, &press(KeyCode::Char(':')));
        let fx = reduce(&mut st, &press(KeyCode::Esc));
        assert!(fx.is_empty());
        assert_eq!(st.mode, Mode::Normal);
        assert!(st.palette.is_none());
    }

    #[test]
    fn leader_question_mark_opens_keys_help_and_any_key_dismisses() {
        let mut st = AppState::new(pane());
        st.mode = Mode::Leader;
        reduce(&mut st, &press(KeyCode::Char('?')));
        assert!(st.keys_help);
        assert_eq!(st.mode, Mode::Normal, "one-shot");
        // The dismissing key is swallowed, not acted on.
        let fx = reduce(&mut st, &press(KeyCode::Char('x')));
        assert!(fx.is_empty(), "dismiss key is swallowed");
        assert!(!st.keys_help);
        assert_eq!(st.agents.len(), 1);
    }

    #[test]
    fn opening_an_agent_clears_its_attention() {
        let mut st = agents3();
        st.agents[1].attention = true;
        // Click its rail row (roster: [r1(attn), r0, r2] → row 0 = r1).
        st.click_zones.push(ClickZone {
            x: 0,
            y: 0,
            w: 20,
            h: 2,
            target: ClickTarget::RailRow(0),
        });
        reduce(&mut st, &click(1, 0));
        assert_eq!(st.detail.shown, vec!["r1".to_string()]);
        assert!(!st.agents[1].attention, "looking at it clears attention");
    }

    // ── Done-unseen (the inbox state) + focus tracking. ──

    #[test]
    fn opening_an_agent_decays_done_to_idle() {
        let mut st = agents3();
        st.agents[1].done = true;
        // Click its rail row (r1 tops the roster, done-unseen).
        st.click_zones.push(ClickZone {
            x: 0,
            y: 0,
            w: 20,
            h: 2,
            target: ClickTarget::RailRow(0),
        });
        reduce(&mut st, &click(1, 0));
        assert!(!st.agents[1].done, "viewing decays done back to idle");
    }

    #[test]
    fn done_sorts_above_working_below_attention() {
        let mut st = agents3();
        st.agents[0].turn_active = true; // working
        st.agents[1].done = true; // finished, unseen
        st.agents[2].attention = true; // trouble
        assert_eq!(st.roster(), vec![2, 1, 0], "attention > done > working");
    }

    #[test]
    fn shown_pane_accrues_done_while_unfocused_and_refocus_clears_it() {
        let mut st = agents3(); // r0 shown solo
        reduce(&mut st, &AppEvent::Focus(false));
        let fx = reduce(
            &mut st,
            &AppEvent::TurnEnded {
                record_id: "r0".into(),
                stop_reason: StopReason::EndTurn,
            },
        );
        assert!(st.agents[0].done, "on screen but the human is away");
        assert!(
            fx.iter()
                .any(|e| matches!(e, Effect::Notify { title, .. } if title.contains("finished"))),
            "away completions reach the terminal: {fx:?}"
        );
        reduce(&mut st, &AppEvent::Focus(true));
        assert!(!st.agents[0].done, "coming back marks the shown pane seen");
    }

    #[test]
    fn focused_events_do_not_notify() {
        let mut st = agents3();
        let fx = reduce(
            &mut st,
            &AppEvent::TurnEnded {
                record_id: "r1".into(),
                stop_reason: StopReason::EndTurn,
            },
        );
        let fx2 = reduce(&mut st, &perm("r2", "wants write"));
        assert!(
            !fx.iter()
                .chain(fx2.iter())
                .any(|e| matches!(e, Effect::Notify { .. })),
            "in-terminal signals own the focused case"
        );
    }

    #[test]
    fn unfocused_permission_notifies_with_the_risk_tag() {
        let mut st = agents3();
        reduce(&mut st, &AppEvent::Focus(false));
        let fx = reduce(&mut st, &perm("r1", "rm -rf scratch"));
        assert!(
            fx.iter().any(|e| matches!(
                e,
                Effect::Notify { title, body }
                    if title == "a1 needs approval" && body == "high risk · rm -rf scratch"
            )),
            "{fx:?}"
        );
        assert!(
            fx.contains(&Effect::Bell),
            "the background bell still rings"
        );
    }

    #[test]
    fn review_ready_flags_done_not_attention_and_notifies_when_away() {
        let mut st = agents3();
        reduce(&mut st, &AppEvent::Focus(false));
        let fx = reduce(&mut st, &review_ready("r2"));
        assert!(st.agents[2].done, "a ready review is inbox material");
        assert!(!st.agents[2].attention, "nothing went wrong");
        assert!(
            fx.iter().any(|e| matches!(
                e,
                Effect::Notify { title, body }
                    if title.contains("ready to review") && body.contains("+10/-3")
            )),
            "{fx:?}"
        );
    }

    // ── Time-in-state. ──

    #[test]
    fn elapsed_label_tracks_time_in_state_and_resets_on_change() {
        let mut st = agents3();
        st.agents[0].turn_active = true;
        reduce(&mut st, &AppEvent::Tick); // stamps every pane's bucket
        st.tick += 42 * 5; // 42s later at 5 ticks/sec
        assert_eq!(st.agents[0].elapsed_label(st.tick), Some("42s".into()));
        assert_eq!(
            st.agents[1].elapsed_label(st.tick),
            None,
            "idle rows stay calm"
        );
        // A bucket change restarts the clock.
        reduce(
            &mut st,
            &AppEvent::TurnEnded {
                record_id: "r1".into(),
                stop_reason: StopReason::EndTurn,
            },
        );
        assert_eq!(
            st.agents[1].elapsed_label(st.tick),
            Some("0s".into()),
            "done-unseen just started"
        );
    }

    #[test]
    fn fmt_elapsed_compacts_units() {
        assert_eq!(fmt_elapsed(0), "0s");
        assert_eq!(fmt_elapsed(59), "59s");
        assert_eq!(fmt_elapsed(60), "1m");
        assert_eq!(fmt_elapsed(3599), "59m");
        assert_eq!(fmt_elapsed(3600), "1h00m");
        assert_eq!(fmt_elapsed(4500), "1h15m");
    }

    #[test]
    fn spawn_failure_notice_flattens_multiline_errors() {
        let mut st = agents3();
        reduce(
            &mut st,
            &AppEvent::AgentSpawnFailed {
                agent_id: "claude-acp".into(),
                error:
                    "Internal error: {\n  \"details\": \"Query closed before response received\"\n}"
                        .into(),
            },
        );
        let notice = st.notice.clone().expect("notice set");
        assert!(
            !notice.contains('\n'),
            "one line for the mode bar: {notice:?}"
        );
        assert!(
            notice.contains("Query closed before response received"),
            "the details survive the flatten: {notice}"
        );
        // Pathologically long errors are capped, not dumped.
        reduce(
            &mut st,
            &AppEvent::AgentSpawnFailed {
                agent_id: "x".into(),
                error: "word ".repeat(100),
            },
        );
        let capped = st.notice.expect("notice set");
        assert!(capped.chars().count() < 260, "{}", capped.len());
        assert!(capped.ends_with('…'));
    }

    // ── Fleet-state snapshot. ──

    #[test]
    fn fleet_agents_snapshot_maps_manager_state() {
        let mut st = agents3(); // r0 focused solo
        st.agents[1].autonomy = Autonomy::Auto;
        st.agents[1].review = Some((3, 42, 7));
        st.agents[1].port = Some(3101);
        st.agents[2].pending = Some(PendingView {
            title: "rm -rf scratch".into(),
            diff: None,
            options: vec![],
            risk: Risk::High,
        });
        // PTY panes (orchestrator, attaches) are not fleet agents.
        let mut pty = PaneState::new("orchestrator".into(), "claude".into());
        pty.kind = PaneKind::Pty;
        st.agents.push(pty);

        let snap = st.fleet_agents();
        assert_eq!(snap.len(), 3, "pty panes skipped");
        assert_eq!(snap[0].record_id, "r0");
        assert_eq!(snap[0].autonomy, "manual");
        assert_eq!(snap[1].autonomy, "auto");
        assert_eq!(snap[1].review, Some((3, 42, 7)));
        assert_eq!(snap[1].port, Some(3101));
        assert_eq!(snap[2].pending.as_deref(), Some("rm -rf scratch"));
        assert!(
            snap.iter().all(|a| a.draft.is_none()),
            "monitors are read-only — no drafts to persist"
        );
    }

    // ── Title badge. ──

    #[test]
    fn title_badge_counts_by_glyph_and_reads_calm_when_clear() {
        let mut st = agents3();
        assert_eq!(st.title_badge(), "bitrouter tui");
        st.agents[0].pending = Some(PendingView {
            title: "w".into(),
            diff: None,
            options: vec![],
            risk: Risk::High,
        });
        st.agents[1].review = Some((1, 2, 3));
        st.agents[2].done = true;
        assert_eq!(st.title_badge(), "bitrouter ⚠1 ◆1 ◉1");
    }

    // ── MCP fleet bridge mirroring (Unix). ──

    #[cfg(unix)]
    fn spawn_mirror(st: &mut AppState) {
        reduce(
            st,
            &AppEvent::BridgeSpawned {
                record_id: "mcp:abc123".into(),
                agent_id: "codex-acp".into(),
                port: Some(3111),
            },
        );
    }

    #[cfg(unix)]
    #[test]
    fn bridge_spawn_mirrors_into_the_rail_without_stealing_focus() {
        let mut st = AppState::new(pane());
        st.detail = DetailLayout::solo("rec-1".into());
        spawn_mirror(&mut st);
        let mirror = st.agents.iter().find(|p| p.record_id == "mcp:abc123");
        let mirror = mirror.expect("mirror pane created");
        assert_eq!(mirror.kind, PaneKind::Monitor);
        assert_eq!(mirror.owner, Ownership::Orchestrator);
        assert!(mirror.turn_active, "a bridge spawn starts working");
        assert!(
            st.roster()
                .iter()
                .any(|&i| st.agents[i].record_id == "mcp:abc123"),
            "mirror appears in the subagents roster"
        );
        assert_eq!(
            st.detail.shown,
            vec!["rec-1".to_string()],
            "the human's detail focus is untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bridge_permission_rides_the_decision_queue() {
        let mut st = AppState::new(pane());
        spawn_mirror(&mut st);
        reduce(
            &mut st,
            &AppEvent::Permission {
                record_id: "mcp:abc123".into(),
                title: "rm -rf build".into(),
                diff: None,
                options: allow_deny(),
                risk: Risk::High,
            },
        );
        let mirror = st
            .agents
            .iter()
            .find(|p| p.record_id == "mcp:abc123")
            .expect("mirror pane");
        assert!(mirror.pending.is_some(), "gated request reaches the queue");
        // Resolve inline from NORMAL on the focused mirror.
        st.detail = DetailLayout::solo("mcp:abc123".into());
        let effects = reduce(&mut st, &press(KeyCode::Char('y')));
        assert!(
            effects.contains(&Effect::ResolvePermission {
                record_id: "mcp:abc123".into(),
                outcome: PermissionOutcome::AllowOnce,
            }),
            "resolution flows through the normal effect: {effects:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn live_mirror_refuses_close_and_prompt() {
        let mut st = AppState::new(pane());
        spawn_mirror(&mut st);
        // `leader c` on the focused live mirror: refused with guidance.
        st.detail = DetailLayout::solo("mcp:abc123".into());
        st.mode = Mode::Leader;
        let effects = reduce(&mut st, &press(KeyCode::Char('c')));
        assert!(effects.is_empty(), "no CloseAgent for a live mirror");
        assert!(
            st.agents.iter().any(|p| p.record_id == "mcp:abc123"),
            "mirror pane retained"
        );
        assert!(
            st.notice
                .as_deref()
                .is_some_and(|n| n.contains("close_subagent"))
        );
        // Typing at a focused orchestrator-owned monitor lands on a notice
        // pointing at the owner, never an invisible composer.
        st.mode = Mode::Normal;
        st.detail = DetailLayout::solo("mcp:abc123".into());
        let fx = reduce(&mut st, &press(KeyCode::Char('h')));
        assert!(fx.is_empty(), "typing at a mirror does nothing: {fx:?}");
        assert!(
            st.notice
                .as_deref()
                .is_some_and(|n| n.contains("orchestrator")),
            "notice routes the human to the owner"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bridge_state_and_disconnect_update_the_mirror() {
        let mut st = AppState::new(pane());
        spawn_mirror(&mut st);
        reduce(
            &mut st,
            &AppEvent::BridgeState {
                record_id: "mcp:abc123".into(),
                state: "completed".into(),
            },
        );
        {
            let mirror = st
                .agents
                .iter()
                .find(|p| p.record_id == "mcp:abc123")
                .expect("mirror");
            assert!(!mirror.turn_active);
            assert!(mirror.done, "completed = done-unseen");
        }
        reduce(
            &mut st,
            &AppEvent::BridgeGone {
                record_ids: vec!["mcp:abc123".into()],
            },
        );
        let mirror = st
            .agents
            .iter()
            .find(|p| p.record_id == "mcp:abc123")
            .expect("mirror");
        assert!(mirror.exited, "disconnect marks the mirror dead");
        assert!(mirror.pending.is_none());
    }

    // ── Bracketed paste. ──

    #[test]
    fn paste_at_a_monitor_is_inert() {
        let mut st = AppState::new(pane());
        let effects = reduce(&mut st, &AppEvent::Paste("line1\r\nline2".into()));
        assert!(
            effects.is_empty(),
            "monitors are read-only — paste is inert"
        );
        assert!(
            st.agents[0].lines.is_empty(),
            "nothing lands in the transcript"
        );
    }

    #[test]
    fn paste_routes_to_a_focused_pty_pane() {
        let mut st = AppState::new(pane());
        let mut pty = PaneState::new("session-1".into(), "claude".into());
        pty.kind = PaneKind::Pty;
        st.agents.push(pty);
        st.detail = DetailLayout::solo("session-1".into());
        let effects = reduce(&mut st, &AppEvent::Paste("hello".into()));
        assert_eq!(
            effects,
            vec![Effect::PtyPaste {
                record_id: "session-1".into(),
                text: "hello".into(),
            }]
        );
    }

    #[test]
    fn paneless_permission_denies_instead_of_stranding() {
        let mut st = AppState::new(pane());
        let effects = reduce(
            &mut st,
            &AppEvent::Permission {
                record_id: "ghost".into(),
                title: "WRITE".into(),
                diff: None,
                options: allow_deny(),
                risk: Risk::High,
            },
        );
        assert_eq!(
            effects,
            vec![Effect::ResolvePermission {
                record_id: "ghost".into(),
                outcome: PermissionOutcome::Deny,
            }],
            "a request with no pane to show it in must deny, not hang"
        );
    }
}
