//! Pure render state + reducer for the TUI. No `ratatui`/`tokio` deps.
//!
//! Two screens. The DEFAULT screen is the focused agent's viewport at full
//! terminal width over a one-line status bar — nothing else, so the harness's
//! own TUI gets every column it asks for. The MANAGER screen (leader `m`) is
//! a full-screen overlay listing the whole fleet, and it is where supervision
//! happens: decide (`y`/`a`/`n`), review (`D`/`m`/`p`/`r`), focus, close.
//!
//! The manager is not optional chrome. A focused PTY pane is locked-mode
//! passthrough, so NORMAL-mode supervision keys never fire while the
//! orchestrator has the keyboard — the manager is the only surface on which a
//! blocked subagent can be seen and unblocked.

use std::collections::HashMap;

use crate::risk::Risk;
use crate::tui::event::{AppEvent, Effect};
use agent_client_protocol::schema::v1::StopReason;
use bitrouter_substrate::translate::UsageCost;
use crossterm::event::{KeyCode, KeyModifiers};

pub mod diff;
pub mod pane;
use self::pane::{PaneKind, PaneState};
pub mod overlay;
use self::overlay::{DEFAULT_LEADER, ManagerState, Mode, PaletteState, PickerState};
pub mod layout;
use self::layout::{ClickZone, PtyArea};
pub mod events;
pub mod keys;
use self::events::reduce_inner;

/// Max scrollback lines retained per pane (ring buffer).
const SCROLLBACK_CAP: usize = 2000;

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

/// Whole-app render state: a flat agent list + which one has the viewport.
/// Accessors return `Option` because the agent list may be empty transiently
/// (right after the last agent closes, before `should_quit`).
#[derive(Debug, Clone)]
pub struct AppState {
    /// Every live agent, in spawn order (stable; `fleet()` sorts a projection).
    pub agents: Vec<PaneState>,
    /// The `record_id` owning the viewport — the ONE agent on screen, which
    /// also receives NORMAL-mode input. `None` only transiently, between the
    /// focused agent closing and its replacement being picked.
    pub focus: Option<String>,
    /// Manager-overlay state (present while `mode == Manager`).
    pub manager: Option<ManagerState>,
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
    /// Each PTY pane's drawn content rectangle — the loop resizes the emulator
    /// and PTY (SIGWINCH) when one changes, and hit-tests the pointer against
    /// it to forward mouse events to a mouse-reporting inner app. Rebuilt every
    /// frame by the renderer.
    pub pty_areas: Vec<PtyArea>,
    /// Clickable regions recorded by the renderer for the current frame (the
    /// manager view's rows and its `+ new session` footer). Rebuilt every
    /// frame, like [`AppState::pty_areas`].
    pub click_zones: Vec<ClickZone>,
}

impl AppState {
    pub fn new(pane: PaneState) -> Self {
        let focus = Some(pane.record_id.clone());
        Self {
            agents: vec![pane],
            focus,
            manager: None,
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

    /// The fleet list — the manager view's canonical order: EVERY agent,
    /// orchestrator sessions and ACP subagents alike, sorted by actionability
    /// bucket (needs-you > review > attention > done > working > idle > dead).
    /// Needs-you rows order by risk (high first) then age (oldest pending
    /// first), so the head is always the next decision; other buckets keep
    /// spawn order (the sort is stable).
    ///
    /// The two sidebars this replaced split the same set by pane kind — PTY
    /// sessions on the left, ACP monitors on the right. That is an
    /// implementation detail of how an agent was launched, not a distinction
    /// the human thinks in, so the manager shows one list and tags the kind.
    pub fn fleet(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.agents.len()).collect();
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

    /// Indices of the PTY panes (orchestrator sessions and interactive
    /// attaches) in spawn order. Deliberately NOT actionability-sorted: this
    /// backs `leader 1..9`, and a chord that means a different session
    /// depending on who happens to be blocked is a chord you cannot learn.
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

    /// The pane owning the viewport (receives NORMAL-mode input).
    pub fn focused(&self) -> Option<&PaneState> {
        let id = self.focus.as_deref()?;
        self.agents.iter().find(|p| p.record_id == id)
    }

    /// The pane owning the viewport, mutably.
    pub fn focused_mut(&mut self) -> Option<&mut PaneState> {
        let id = self.focus.clone()?;
        self.agents.iter_mut().find(|p| p.record_id == id)
    }

    /// Give `record_id` the viewport, and count it as seen.
    pub(super) fn focus_on(&mut self, record_id: String) {
        self.focus = Some(record_id);
        mark_shown_seen(self);
    }

    /// Whether `record_id` is the agent currently on screen.
    fn is_shown(&self, record_id: &str) -> bool {
        self.focus.as_deref() == Some(record_id)
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
    // bucket, so the manager can show how long it has been
    // working/blocked/done.
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

/// Mark the agent on screen as seen (the user is now looking at it): clears
/// `attention` and decays `done` back to idle.
fn mark_shown_seen(state: &mut AppState) {
    if !state.term_focused {
        // On screen but the human is elsewhere — not seen yet.
        return;
    }
    let Some(shown) = state.focus.clone() else {
        return;
    };
    if let Some(pane) = state.pane_by_id_mut(&shown) {
        pane.attention = false;
        pane.done = false;
    }
}

#[cfg(test)]
mod tests;
