//! Pure render state + reducer for the TUI. No `ratatui`/`tokio` deps.
//!
//! One screen: a fixed left rail (roster sorted by actionability + radar) and
//! a splittable detail viewport showing 1–4 agents. The rail is the canonical
//! list of every agent; the detail split is ephemeral layout, not structure.

use std::collections::HashMap;

use crate::tui::event::{AppEvent, Effect, PermOption, Risk};
use bitrouter_substrate::translate::{PermissionOutcome, SessionUpdateKind, ToolStatus};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Max scrollback lines retained per pane (ring buffer).
const SCROLLBACK_CAP: usize = 2000;

/// Max agents shown at once in the detail viewport.
const MAX_SHOWN: usize = 4;

/// Which key-handling mode the TUI is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Keys go to the focused detail pane's prompt (default).
    Normal,
    /// Manager keys: rail navigation, split, spawn, close.
    Agent,
    /// Selecting an agent to spawn.
    Picker,
    /// Selecting multiple agents to send one message to all of them.
    Broadcast,
    /// Fuzzy command palette (`:` on an empty prompt, or `:` in AGENT mode).
    Command,
}

/// One palette command. The table is static; actions map onto existing
/// reducer paths so the palette adds discoverability, not new behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    SpawnAgent,
    CloseAgent,
    SplitH,
    SplitV,
    Unsplit,
    Broadcast,
    Queue,
    Autonomy,
    KillDone,
    KeysHelp,
    Quit,
}

/// Palette entries: display name → command. Order = display order when the
/// filter is empty.
pub const COMMANDS: &[(&str, Command)] = &[
    ("spawn agent", Command::SpawnAgent),
    ("close agent", Command::CloseAgent),
    ("split horizontal", Command::SplitH),
    ("split vertical", Command::SplitV),
    ("unsplit", Command::Unsplit),
    ("broadcast", Command::Broadcast),
    ("queue", Command::Queue),
    ("autonomy cycle", Command::Autonomy),
    ("kill done", Command::KillDone),
    ("keys help", Command::KeysHelp),
    ("quit", Command::Quit),
];

/// State of the command palette overlay.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PaletteState {
    pub input: String,
    pub selected: usize,
}

impl PaletteState {
    /// Commands whose name fuzzy-matches (case-insensitive subsequence) the
    /// current input, in table order.
    pub fn matches(&self) -> Vec<(&'static str, Command)> {
        COMMANDS
            .iter()
            .copied()
            .filter(|(name, _)| fuzzy_match(name, &self.input))
            .collect()
    }
}

/// Case-insensitive subsequence match: every `needle` char appears in
/// `haystack` in order. An empty needle matches everything.
fn fuzzy_match(haystack: &str, needle: &str) -> bool {
    let mut hay = haystack.chars().flat_map(char::to_lowercase);
    needle
        .chars()
        .flat_map(char::to_lowercase)
        .all(|n| hay.any(|h| h == n))
}

/// State of the agent picker overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerState {
    pub agents: Vec<String>,
    pub selected: usize,
}

/// One rendered scrollback line, tagged for styling by the UI layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// A prompt the user submitted (`› …`).
    UserPrompt(String),
    /// Agent message text.
    Message(String),
    /// Agent thinking text.
    Thought(String),
    /// A tool call: title + status.
    Tool {
        id: String,
        title: String,
        status: ToolStatus,
    },
    /// A manager-side failure surfaced in the pane (e.g. a prompt that never
    /// reached the agent). Rendered in the danger style.
    Error(String),
    /// An autonomy-tier decision the manager made on the user's behalf.
    /// Nothing auto-resolves silently — every one lands here.
    AutoResolved(String),
}

/// A pending permission surfaced in the pane, as display data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingView {
    pub title: String,
    pub diff: Option<String>,
    pub options: Vec<PermOption>,
    pub risk: Risk,
}

/// Per-agent autonomy tier: which permission requests reach the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Autonomy {
    /// Every request surfaces (default for a fresh, untrusted agent).
    #[default]
    Manual,
    /// Low-risk requests auto-allow; high-risk surface.
    Assisted,
    /// Everything auto-allows (logged, never silent).
    Auto,
}

impl Autonomy {
    /// Cycle Manual → Assisted → Auto → Manual.
    fn next(self) -> Self {
        match self {
            Autonomy::Manual => Autonomy::Assisted,
            Autonomy::Assisted => Autonomy::Auto,
            Autonomy::Auto => Autonomy::Manual,
        }
    }

    /// Short label for the rail row and log lines.
    pub fn label(self) -> &'static str {
        match self {
            Autonomy::Manual => "manual",
            Autonomy::Assisted => "assisted",
            Autonomy::Auto => "auto",
        }
    }
}

/// One agent pane's state.
#[derive(Debug, Clone)]
pub struct PaneState {
    pub record_id: String,
    pub agent_id: String,
    /// Terse harness tag shown in the pane header (e.g. `claude`, `codex`).
    /// Empty when unknown.
    pub harness: String,
    pub lines: Vec<Line>,
    pub pending: Option<PendingView>,
    pub exited: bool,
    pub selected: bool,
    pub attention: bool,
    /// Which permission requests reach the user (cycled with `A` on the rail).
    pub autonomy: Autonomy,
    /// Arrival order of the current `pending` (from `AppState.perm_seq`);
    /// the queue orders needs-you rows oldest-first with it.
    pub pending_seq: u64,
    /// `None` = follow the tail; `Some(i)` = pinned with line `i` first visible.
    /// Content-pinned: new output never moves a pinned view.
    pub scroll: Option<usize>,
    /// Inner height (rows) this pane last rendered at; recorded by the UI so
    /// paging moves by exactly one screen (ratatui stateful-render idiom).
    pub viewport: usize,
}

impl PaneState {
    pub fn new(record_id: String, agent_id: String) -> Self {
        Self {
            record_id,
            agent_id,
            harness: String::new(),
            lines: Vec::new(),
            pending: None,
            exited: false,
            selected: false,
            attention: false,
            autonomy: Autonomy::default(),
            pending_seq: 0,
            scroll: None,
            viewport: 0,
        }
    }

    fn push(&mut self, line: Line) {
        self.lines.push(line);
        if self.lines.len() > SCROLLBACK_CAP {
            let overflow = self.lines.len() - SCROLLBACK_CAP;
            self.lines.drain(0..overflow);
            // Keep a pinned view on the same content as the buffer slides.
            if let Some(s) = &mut self.scroll {
                *s = s.saturating_sub(overflow);
            }
        }
    }

    /// Page the view up (into history), pinning it if it was following.
    fn scroll_page_up(&mut self) {
        let page = self.viewport.max(1);
        let tail_start = self.lines.len().saturating_sub(page);
        let start_now = self.scroll.unwrap_or(tail_start);
        self.scroll = Some(start_now.saturating_sub(page));
    }

    /// Page the view down (toward the tail); reaching it resumes following.
    fn scroll_page_down(&mut self) {
        let page = self.viewport.max(1);
        if let Some(s) = self.scroll {
            let next = s + page;
            if next + page >= self.lines.len() {
                self.scroll = None; // back at the tail — follow again
            } else {
                self.scroll = Some(next);
            }
        }
    }

    /// Actionability bucket for the roster sort. Lower = closer to the top.
    fn bucket(&self) -> u8 {
        if self.pending.is_some() {
            0 // needs you
        } else if self.attention {
            1 // something happened in the background
        } else if !self.exited {
            2 // running
        } else {
            3 // dead
        }
    }
}

/// How the detail viewport is divided when showing more than one agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Split {
    /// Side-by-side columns.
    H,
    /// Stacked rows.
    V,
}

/// Which agents the detail viewport shows and how. Ephemeral layout state —
/// closing an agent prunes it; the split direction applies to all slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailLayout {
    /// `record_id`s of the shown agents, in slot order (1..=MAX_SHOWN).
    pub shown: Vec<String>,
    pub split: Split,
    /// Index into `shown` of the slot that receives NORMAL-mode input.
    pub focus: usize,
}

impl DetailLayout {
    /// Show exactly one agent.
    fn solo(record_id: String) -> Self {
        Self {
            shown: vec![record_id],
            split: Split::H,
            focus: 0,
        }
    }

    /// Add `record_id` as a new slot in `split` direction (or refocus it if
    /// already shown). Full viewport (MAX_SHOWN) refocuses instead of adding.
    fn add(&mut self, record_id: String, split: Split) {
        self.split = split;
        if let Some(i) = self.shown.iter().position(|r| r == &record_id) {
            self.focus = i;
            return;
        }
        if self.shown.len() >= MAX_SHOWN {
            return;
        }
        self.shown.push(record_id);
        self.focus = self.shown.len() - 1;
    }

    /// Remove the focused slot (keeps at least one).
    fn remove_focused(&mut self) {
        if self.shown.len() > 1 {
            self.shown.remove(self.focus);
            if self.focus >= self.shown.len() {
                self.focus = self.shown.len() - 1;
            }
        }
    }

    /// Drop `record_id` from the layout if shown; clamps focus.
    fn prune(&mut self, record_id: &str) {
        self.shown.retain(|r| r != record_id);
        if self.focus >= self.shown.len() {
            self.focus = self.shown.len().saturating_sub(1);
        }
    }

    /// The focused slot's record id.
    fn focused_id(&self) -> Option<&str> {
        self.shown.get(self.focus).map(String::as_str)
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
    /// Rail cursor: an index into `roster()` order.
    pub rail_cursor: usize,
    /// Queue focus mode (`q` in AGENT mode): the rail shows only agents that
    /// need you. Cleared when leaving AGENT mode.
    pub queue_only: bool,
    /// Monotonic permission-arrival counter backing `PaneState.pending_seq`.
    perm_seq: u64,
    /// agent_id → harness tag, from the config catalog.
    pub harness_by_agent: HashMap<String, String>,
    pub input: String,
    pub should_quit: bool,
    pub mode: Mode,
    pub picker: Option<PickerState>,
    /// Command palette overlay state (present while `mode == Command`).
    pub palette: Option<PaletteState>,
    /// Which-key overlay: lists the current mode's bindings; any key dismisses.
    pub keys_help: bool,
    pub available_agents: Vec<String>,
    pub notice: Option<String>,
    pub broadcast_input: String,
}

impl AppState {
    pub fn new(pane: PaneState) -> Self {
        let detail = DetailLayout::solo(pane.record_id.clone());
        Self {
            agents: vec![pane],
            detail,
            rail_cursor: 0,
            queue_only: false,
            perm_seq: 0,
            harness_by_agent: HashMap::new(),
            input: String::new(),
            should_quit: false,
            mode: Mode::Normal,
            picker: None,
            palette: None,
            keys_help: false,
            available_agents: Vec::new(),
            notice: None,
            broadcast_input: String::new(),
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

    /// Roster order: indices into `agents`, sorted by actionability bucket
    /// (needs-you > attention > running > dead). Needs-you rows order by risk
    /// (high first) then age (oldest pending first) — the queue; other buckets
    /// keep spawn order. In queue focus mode (`queue_only`) only needs-you
    /// rows are listed.
    pub fn roster(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.agents.len())
            .filter(|&i| !self.queue_only || self.agents[i].pending.is_some())
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

    /// The agent under the rail cursor.
    pub fn rail_selected(&self) -> Option<&PaneState> {
        let order = self.roster();
        order
            .get(self.rail_cursor.min(order.len().saturating_sub(1)))
            .and_then(|&i| self.agents.get(i))
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

    /// Clamp the rail cursor into the current roster (which may be filtered).
    fn clamp_rail_cursor(&mut self) {
        let len = self.roster().len();
        if len == 0 {
            self.rail_cursor = 0;
        } else if self.rail_cursor >= len {
            self.rail_cursor = len - 1;
        }
    }
}

/// Fold one event into state, returning effects for the loop to run.
/// PURE: no I/O, no session access.
pub fn reduce(state: &mut AppState, event: &AppEvent) -> Vec<Effect> {
    match event {
        AppEvent::Update { record_id, update } => {
            if let Some(pane) = state.pane_by_id_mut(record_id) {
                apply_update(pane, update);
            }
            Vec::new()
        }
        AppEvent::Exited { record_id } => {
            let shown = state.is_shown(record_id);
            let mut effects = Vec::new();
            if let Some(pane) = state.pane_by_id_mut(record_id) {
                pane.exited = true;
                // A dead agent's decision is moot — drop it from the queue.
                // (The loop's teardown drops the resolvable handle → Deny.)
                pane.pending = None;
                if !shown {
                    pane.attention = true;
                    effects.push(Effect::Bell);
                }
            }
            state.clamp_rail_cursor();
            effects
        }
        AppEvent::Permission {
            record_id,
            title,
            diff,
            options,
            risk,
        } => {
            let shown = state.is_shown(record_id);
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
                    pane.push(Line::AutoResolved(format!(
                        "auto-allowed ({}): {title}",
                        pane.autonomy.label()
                    )));
                    effects.push(Effect::ResolvePermission {
                        record_id: record_id.clone(),
                        outcome: PermissionOutcome::AllowOnce,
                    });
                } else {
                    pane.pending = Some(PendingView {
                        title: title.clone(),
                        diff: diff.clone(),
                        options: options.clone(),
                        risk: *risk,
                    });
                    pane.pending_seq = seq;
                    if !shown {
                        pane.attention = true;
                        effects.push(Effect::Bell);
                    }
                }
            }
            effects
        }
        AppEvent::AgentSpawned {
            record_id,
            agent_id,
        } => {
            let mut pane = PaneState::new(record_id.clone(), agent_id.clone());
            if let Some(h) = state.harness_by_agent.get(agent_id) {
                pane.harness = h.clone();
            }
            state.agents.push(pane);
            // A just-spawned agent is what you want to look at: open it solo.
            state.detail = DetailLayout::solo(record_id.clone());
            state.notice = None;
            Vec::new()
        }
        AppEvent::AgentSpawnFailed { agent_id, error } => {
            state.notice = Some(format!("failed to spawn {agent_id}: {error}"));
            Vec::new()
        }
        AppEvent::PromptFailed { record_id, error } => {
            let shown = state.is_shown(record_id);
            let mut effects = Vec::new();
            if let Some(pane) = state.pane_by_id_mut(record_id) {
                pane.push(Line::Error(format!("prompt failed: {error}")));
                if !shown {
                    pane.attention = true;
                    effects.push(Effect::Bell);
                }
            }
            effects
        }
        AppEvent::Key(key) => {
            // Ctrl-C is a global quit — every mode, even with a permission
            // pending (the loop's teardown drops the pending handle, which
            // Denies it in the substrate). Also the loop's synthesized quit
            // key on input-stream end, so it must never be swallowed by a
            // mode's fallthrough.
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                state.should_quit = true;
                return vec![Effect::Quit];
            }
            // The which-key overlay swallows exactly one key to dismiss.
            if state.keys_help {
                state.keys_help = false;
                return Vec::new();
            }
            match state.mode {
                Mode::Normal => reduce_key_normal(state, key),
                Mode::Agent => reduce_key_agent(state, key),
                Mode::Picker => reduce_key_picker(state, key),
                Mode::Broadcast => reduce_key_broadcast(state, key),
                Mode::Command => reduce_key_command(state, key),
            }
        }
    }
}

/// NORMAL-mode keys. Permission keys take priority when a prompt is pending.
fn reduce_key_normal(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    // Ctrl-A enters AGENT (manager) mode with the cursor on the focused agent.
    if key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL) {
        state.mode = Mode::Agent;
        // Park the rail cursor on the detail-focused agent's roster row.
        if let Some(id) = state.detail.focused_id().map(str::to_string) {
            let order = state.roster();
            if let Some(pos) = order.iter().position(|&i| state.agents[i].record_id == id) {
                state.rail_cursor = pos;
            }
        }
        return Vec::new();
    }
    // Ctrl-B enters BROADCAST mode (with a cleared selection).
    if key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL) {
        for p in state.agents.iter_mut() {
            p.selected = false;
        }
        state.mode = Mode::Broadcast;
        return Vec::new();
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
    let has_pending = state
        .focused()
        .map(|p| p.pending.is_some())
        .unwrap_or(false);

    if has_pending {
        let outcome = match key.code {
            KeyCode::Char('y') => Some(PermissionOutcome::AllowOnce),
            KeyCode::Char('a') => Some(PermissionOutcome::AllowAlways),
            KeyCode::Char('n') => Some(PermissionOutcome::Deny),
            _ => None,
        };
        if let Some(outcome) = outcome {
            if let Some(pane) = state.focused_mut() {
                pane.pending = None;
            }
            return vec![Effect::ResolvePermission {
                record_id: focus_id,
                outcome,
            }];
        }
        return Vec::new();
    }

    match key.code {
        // `:` on an empty prompt opens the command palette; mid-sentence it
        // is a literal colon.
        KeyCode::Char(':') if state.input.is_empty() => {
            state.palette = Some(PaletteState::default());
            state.mode = Mode::Command;
            Vec::new()
        }
        KeyCode::Char(c) => {
            state.input.push(c);
            Vec::new()
        }
        KeyCode::Backspace => {
            state.input.pop();
            Vec::new()
        }
        KeyCode::Enter => {
            let text = std::mem::take(&mut state.input);
            if text.is_empty() {
                return Vec::new();
            }
            if let Some(pane) = state.focused_mut() {
                pane.push(Line::UserPrompt(text.clone()));
            }
            vec![Effect::Prompt {
                record_id: focus_id,
                text,
            }]
        }
        _ => Vec::new(),
    }
}

/// AGENT-mode keys: rail navigation + detail layout + queue + spawn/close.
fn reduce_key_agent(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Esc => {
            state.queue_only = false;
            state.clamp_rail_cursor();
            state.mode = Mode::Normal;
            Vec::new()
        }
        // ── Rail navigation. ──
        KeyCode::Down | KeyCode::Char('j') => {
            let max = state.roster().len().saturating_sub(1);
            state.rail_cursor = (state.rail_cursor + 1).min(max);
            Vec::new()
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.rail_cursor = state.rail_cursor.saturating_sub(1);
            Vec::new()
        }
        // ── Queue. ──
        // Toggle queue focus: the rail shows only agents that need you.
        KeyCode::Char('q') => {
            state.queue_only = !state.queue_only;
            state.clamp_rail_cursor();
            Vec::new()
        }
        // Resolve the cursor agent's pending decision from the rail — the
        // same `pending` the pane shows inline, so either surface clears both.
        // `d` denies (not `n`, which spawns in this mode).
        KeyCode::Char(c @ ('y' | 'a' | 'd'))
            if state.rail_selected().is_some_and(|p| p.pending.is_some()) =>
        {
            let outcome = match c {
                'y' => PermissionOutcome::AllowOnce,
                'a' => PermissionOutcome::AllowAlways,
                _ => PermissionOutcome::Deny,
            };
            resolve_rail_pending(state, outcome)
        }
        // Open the cursor agent solo in the detail (and return to typing).
        KeyCode::Enter => {
            if let Some(id) = state.rail_selected().map(|p| p.record_id.clone()) {
                state.detail = DetailLayout::solo(id);
                clear_shown_attention(state);
                state.queue_only = false;
                state.clamp_rail_cursor();
                state.mode = Mode::Normal;
            }
            Vec::new()
        }
        // Split the detail: add the cursor agent side-by-side / stacked.
        KeyCode::Char('s') => {
            if let Some(id) = state.rail_selected().map(|p| p.record_id.clone()) {
                state.detail.add(id, Split::H);
                clear_shown_attention(state);
            }
            Vec::new()
        }
        KeyCode::Char('v') => {
            if let Some(id) = state.rail_selected().map(|p| p.record_id.clone()) {
                state.detail.add(id, Split::V);
                clear_shown_attention(state);
            }
            Vec::new()
        }
        // Drop the focused detail slot (never below one).
        KeyCode::Char('u') => {
            state.detail.remove_focused();
            Vec::new()
        }
        // Cycle / jump detail-slot focus.
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
            if !state.detail.shown.is_empty() {
                state.detail.focus = (state.detail.focus + 1) % state.detail.shown.len();
            }
            clear_shown_attention(state);
            Vec::new()
        }
        KeyCode::Left | KeyCode::Char('h') => {
            let n = state.detail.shown.len();
            if n > 0 {
                state.detail.focus = (state.detail.focus + n - 1) % n;
            }
            clear_shown_attention(state);
            Vec::new()
        }
        KeyCode::Char(c @ '1'..='9') => {
            let idx = (c as usize) - ('1' as usize);
            if idx < state.detail.shown.len() {
                state.detail.focus = idx;
            }
            clear_shown_attention(state);
            Vec::new()
        }
        KeyCode::Char('n') => {
            state.picker = Some(PickerState {
                agents: state.available_agents.clone(),
                selected: 0,
            });
            state.mode = Mode::Picker;
            Vec::new()
        }
        // Command palette + which-key.
        KeyCode::Char(':') => {
            state.palette = Some(PaletteState::default());
            state.mode = Mode::Command;
            Vec::new()
        }
        KeyCode::Char('?') => {
            state.keys_help = true;
            Vec::new()
        }
        // Cycle the cursor agent's autonomy tier (capital A — lowercase `a`
        // grants allow-always on a pending row).
        KeyCode::Char('A') => {
            if let Some(id) = state.rail_selected().map(|p| p.record_id.clone())
                && let Some(pane) = state.pane_by_id_mut(&id)
            {
                pane.autonomy = pane.autonomy.next();
                let label = pane.autonomy.label();
                pane.push(Line::AutoResolved(format!("autonomy set to {label}")));
            }
            Vec::new()
        }
        // Close the cursor agent.
        KeyCode::Char('x') => close_rail_selected(state),
        _ => Vec::new(),
    }
}

/// COMMAND-mode keys: filter, select, and run a palette command.
fn reduce_key_command(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
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
fn run_command(state: &mut AppState, cmd: Command) -> Vec<Effect> {
    match cmd {
        Command::SpawnAgent => {
            state.picker = Some(PickerState {
                agents: state.available_agents.clone(),
                selected: 0,
            });
            state.mode = Mode::Picker;
            Vec::new()
        }
        Command::CloseAgent => {
            let id = state.detail.focused_id().map(str::to_string);
            match id {
                Some(id) => close_agent_by_id(state, &id),
                None => Vec::new(),
            }
        }
        Command::SplitH | Command::SplitV => {
            let split = if cmd == Command::SplitH {
                Split::H
            } else {
                Split::V
            };
            // Add the most actionable agent not already shown.
            let next = state
                .roster()
                .into_iter()
                .map(|i| state.agents[i].record_id.clone())
                .find(|id| !state.detail.shown.contains(id));
            if let Some(id) = next {
                state.detail.add(id, split);
                clear_shown_attention(state);
            }
            Vec::new()
        }
        Command::Unsplit => {
            state.detail.remove_focused();
            Vec::new()
        }
        Command::Broadcast => {
            for p in state.agents.iter_mut() {
                p.selected = false;
            }
            state.mode = Mode::Broadcast;
            Vec::new()
        }
        Command::Queue => {
            state.queue_only = !state.queue_only;
            state.clamp_rail_cursor();
            state.mode = Mode::Agent;
            Vec::new()
        }
        Command::Autonomy => {
            if let Some(pane) = state.focused_mut() {
                pane.autonomy = pane.autonomy.next();
                let label = pane.autonomy.label();
                pane.push(Line::AutoResolved(format!("autonomy set to {label}")));
            }
            Vec::new()
        }
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

/// Clear the `attention` flag on every agent visible in the detail viewport
/// (the user is now looking at them).
fn clear_shown_attention(state: &mut AppState) {
    let shown: Vec<String> = state.detail.shown.clone();
    for pane in state.agents.iter_mut() {
        if shown.iter().any(|r| r == &pane.record_id) {
            pane.attention = false;
        }
    }
}

/// Resolve the rail-cursor agent's pending permission with `outcome`. One
/// source of truth: this is the same `PaneState.pending` the pane shows
/// inline, so resolving here clears that surface too (and vice versa).
fn resolve_rail_pending(state: &mut AppState, outcome: PermissionOutcome) -> Vec<Effect> {
    let record_id = match state.rail_selected().map(|p| p.record_id.clone()) {
        Some(id) => id,
        None => return Vec::new(),
    };
    if let Some(pane) = state.pane_by_id_mut(&record_id) {
        if pane.pending.take().is_none() {
            return Vec::new();
        }
        pane.attention = false; // decided — nothing left to look at
    }
    state.clamp_rail_cursor();
    vec![Effect::ResolvePermission { record_id, outcome }]
}

/// Close the agent under the rail cursor.
fn close_rail_selected(state: &mut AppState) -> Vec<Effect> {
    match state.rail_selected().map(|p| p.record_id.clone()) {
        Some(id) => close_agent_by_id(state, &id),
        None => Vec::new(),
    }
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
    state.clamp_rail_cursor();
    if state.agents.is_empty() {
        state.should_quit = true;
    } else if state.detail.shown.is_empty() {
        // Refill with the roster head (most actionable agent).
        if let Some(&head) = state.roster().first() {
            state.detail = DetailLayout::solo(state.agents[head].record_id.clone());
        }
    }
    vec![Effect::CloseAgent {
        record_id: record_id.to_string(),
    }]
}

/// PICKER-mode keys: navigate + choose an agent to spawn.
fn reduce_key_picker(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
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
            state.picker = None;
            state.mode = Mode::Normal;
            match selected {
                Some(agent_id) => vec![Effect::SpawnAgent { agent_id }],
                None => Vec::new(), // empty picker → just close, no spawn
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

/// BROADCAST-mode keys: select agents on the rail, type once, send to all.
fn reduce_key_broadcast(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Esc => {
            clear_broadcast(state);
            state.mode = Mode::Normal;
            Vec::new()
        }
        // Space toggles the rail-cursor row.
        KeyCode::Char(' ') => {
            if let Some(id) = state.rail_selected().map(|p| p.record_id.clone())
                && let Some(p) = state.pane_by_id_mut(&id)
            {
                p.selected = !p.selected;
            }
            Vec::new()
        }
        // Digits toggle the Nth roster row.
        KeyCode::Char(c @ '1'..='9') => {
            let idx = (c as usize) - ('1' as usize);
            let order = state.roster();
            if let Some(&agent_idx) = order.get(idx)
                && let Some(p) = state.agents.get_mut(agent_idx)
            {
                p.selected = !p.selected;
            }
            Vec::new()
        }
        KeyCode::Char('a') => {
            for p in state.agents.iter_mut() {
                p.selected = true;
            }
            Vec::new()
        }
        KeyCode::Backspace => {
            state.broadcast_input.pop();
            Vec::new()
        }
        KeyCode::Enter => {
            let text = state.broadcast_input.clone();
            if text.is_empty() {
                return Vec::new();
            }
            let mut effects = Vec::new();
            for p in state.agents.iter_mut() {
                if p.selected {
                    p.lines.push(Line::UserPrompt(text.clone()));
                    effects.push(Effect::Prompt {
                        record_id: p.record_id.clone(),
                        text: text.clone(),
                    });
                }
            }
            clear_broadcast(state);
            state.mode = Mode::Normal;
            effects
        }
        KeyCode::Char(c) => {
            state.broadcast_input.push(c);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Clear the broadcast input and all agent selections.
fn clear_broadcast(state: &mut AppState) {
    state.broadcast_input.clear();
    for p in state.agents.iter_mut() {
        p.selected = false;
    }
}

/// Fold one translated update into a pane's scrollback.
fn apply_update(pane: &mut PaneState, update: &SessionUpdateKind) {
    match update {
        SessionUpdateKind::MessageChunk { text, .. } => pane.push(Line::Message(text.clone())),
        SessionUpdateKind::ThoughtChunk { text, .. } => pane.push(Line::Thought(text.clone())),
        SessionUpdateKind::ToolCall {
            id, title, status, ..
        } => pane.push(Line::Tool {
            id: id.clone(),
            title: title.clone(),
            status: status.clone(),
        }),
        SessionUpdateKind::ToolCallUpdate {
            id, status, title, ..
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
                pane.push(Line::Tool {
                    id: id.clone(),
                    title: title.clone().unwrap_or_default(),
                    status: status.clone().unwrap_or(ToolStatus::Pending),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::event::{AppEvent, Effect, PermOption, Risk};
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
        AppEvent::Update {
            record_id: "rec-1".into(),
            update: SessionUpdateKind::MessageChunk {
                message_id: None,
                text: format!("line {i}"),
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

    // ── Quit. ──

    #[test]
    fn ctrl_c_quits_from_every_mode() {
        for mode in [
            Mode::Normal,
            Mode::Agent,
            Mode::Picker,
            Mode::Broadcast,
            Mode::Command,
        ] {
            let mut st = AppState::new(pane());
            st.mode = mode;
            if mode == Mode::Picker {
                st.picker = Some(PickerState {
                    agents: vec!["alpha".into()],
                    selected: 0,
                });
            }
            let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
            let effects = reduce(&mut st, &AppEvent::Key(key));
            assert!(st.should_quit, "Ctrl-C must quit from {mode:?}");
            assert_eq!(effects, vec![Effect::Quit], "quit effect from {mode:?}");
        }
    }

    #[test]
    fn ctrl_c_during_pending_permission_quits() {
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
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let effects = reduce(&mut st, &AppEvent::Key(key));
        assert_eq!(effects, vec![Effect::Quit]);
        assert!(st.should_quit);
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
        reduce(
            &mut st,
            &AppEvent::Permission {
                record_id: "rec-1".into(),
                title: "WRITE src/x.rs".into(),
                diff: Some("- a\n+ b".into()),
                options: allow_deny(),
                risk: Risk::High,
            },
        );
        let pending = st.agents[0].pending.as_ref().expect("pending set");
        assert_eq!(pending.title, "WRITE src/x.rs");
        assert_eq!(pending.diff.as_deref(), Some("- a\n+ b"));
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

    #[test]
    fn message_chunk_appends_a_message_line() {
        let mut st = AppState::new(pane());
        let ev = AppEvent::Update {
            record_id: "rec-1".into(),
            update: SessionUpdateKind::MessageChunk {
                message_id: None,
                text: "hi".into(),
            },
        };
        let effects = reduce(&mut st, &ev);
        assert!(effects.is_empty());
        assert_eq!(st.agents[0].lines, vec![Line::Message("hi".into())]);
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

    // ── NORMAL-mode input. ──

    #[test]
    fn typing_appends_to_input() {
        let mut st = AppState::new(pane());
        reduce(&mut st, &press(KeyCode::Char('h')));
        reduce(&mut st, &press(KeyCode::Char('i')));
        assert_eq!(st.input, "hi");
    }

    #[test]
    fn backspace_removes_last_char() {
        let mut st = AppState::new(pane());
        st.input = "hi".into();
        reduce(&mut st, &press(KeyCode::Backspace));
        assert_eq!(st.input, "h");
    }

    #[test]
    fn enter_emits_prompt_effect_records_line_and_clears_input() {
        let mut st = AppState::new(pane());
        st.input = "fix the bug".into();
        let effects = reduce(&mut st, &press(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::Prompt {
                record_id: "rec-1".into(),
                text: "fix the bug".into(),
            }]
        );
        assert_eq!(st.input, "");
        assert_eq!(
            st.agents[0].lines,
            vec![Line::UserPrompt("fix the bug".into())]
        );
    }

    #[test]
    fn enter_on_empty_input_is_a_noop() {
        let mut st = AppState::new(pane());
        let effects = reduce(&mut st, &press(KeyCode::Enter));
        assert!(effects.is_empty());
        assert!(st.agents[0].lines.is_empty());
    }

    #[test]
    fn ctrl_c_emits_quit() {
        let mut st = AppState::new(pane());
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let effects = reduce(&mut st, &AppEvent::Key(key));
        assert_eq!(effects, vec![Effect::Quit]);
        assert!(st.should_quit);
    }

    #[test]
    fn ctrl_a_enters_agent_mode() {
        let mut st = AppState::new(pane());
        let fx = reduce(
            &mut st,
            &AppEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        );
        assert!(fx.is_empty());
        assert_eq!(st.mode, Mode::Agent);
    }

    #[test]
    fn ctrl_a_parks_rail_cursor_on_focused_agent() {
        let mut st = agents3();
        st.detail = DetailLayout::solo("r2".into());
        reduce(
            &mut st,
            &AppEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        );
        // All three are running (same bucket) → roster order = spawn order.
        assert_eq!(st.rail_cursor, 2);
    }

    #[test]
    fn esc_returns_to_normal_from_agent() {
        let mut st = AppState::new(pane());
        st.mode = Mode::Agent;
        reduce(&mut st, &press(KeyCode::Esc));
        assert_eq!(st.mode, Mode::Normal);
    }

    #[test]
    fn ctrl_a_does_not_type_into_input() {
        let mut st = AppState::new(pane());
        reduce(
            &mut st,
            &AppEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        );
        assert_eq!(st.input, "");
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

    // ── AGENT mode: rail navigation + detail layout. ──

    #[test]
    fn jk_move_rail_cursor_with_clamping() {
        let mut st = agents3();
        st.mode = Mode::Agent;
        reduce(&mut st, &press(KeyCode::Char('j')));
        assert_eq!(st.rail_cursor, 1);
        reduce(&mut st, &press(KeyCode::Char('j')));
        assert_eq!(st.rail_cursor, 2);
        reduce(&mut st, &press(KeyCode::Char('j'))); // clamp at bottom
        assert_eq!(st.rail_cursor, 2);
        reduce(&mut st, &press(KeyCode::Char('k')));
        assert_eq!(st.rail_cursor, 1);
        reduce(&mut st, &press(KeyCode::Up));
        assert_eq!(st.rail_cursor, 0);
        reduce(&mut st, &press(KeyCode::Up)); // clamp at top
        assert_eq!(st.rail_cursor, 0);
    }

    #[test]
    fn enter_opens_cursor_agent_solo_and_returns_to_normal() {
        let mut st = agents3();
        st.mode = Mode::Agent;
        st.rail_cursor = 1; // r1 (all same bucket → roster = spawn order)
        reduce(&mut st, &press(KeyCode::Enter));
        assert_eq!(st.detail.shown, vec!["r1".to_string()]);
        assert_eq!(st.detail.focus, 0);
        assert_eq!(st.mode, Mode::Normal);
    }

    #[test]
    fn s_adds_cursor_agent_as_horizontal_split() {
        let mut st = agents3(); // detail = [r0]
        st.mode = Mode::Agent;
        st.rail_cursor = 1;
        reduce(&mut st, &press(KeyCode::Char('s')));
        assert_eq!(st.detail.shown, vec!["r0".to_string(), "r1".to_string()]);
        assert_eq!(st.detail.split, Split::H);
        assert_eq!(st.detail.focus, 1, "new slot takes focus");
    }

    #[test]
    fn v_adds_cursor_agent_as_vertical_split() {
        let mut st = agents3();
        st.mode = Mode::Agent;
        st.rail_cursor = 2;
        reduce(&mut st, &press(KeyCode::Char('v')));
        assert_eq!(st.detail.shown, vec!["r0".to_string(), "r2".to_string()]);
        assert_eq!(st.detail.split, Split::V);
    }

    #[test]
    fn split_on_already_shown_agent_refocuses_instead_of_duplicating() {
        let mut st = agents3();
        st.mode = Mode::Agent;
        st.rail_cursor = 0; // r0 already shown
        reduce(&mut st, &press(KeyCode::Char('s')));
        assert_eq!(st.detail.shown, vec!["r0".to_string()], "no duplicate");
        assert_eq!(st.detail.focus, 0);
    }

    #[test]
    fn split_caps_at_four_shown() {
        let mut st = agents3();
        st.agents.push(PaneState::new("r3".into(), "a3".into()));
        st.agents.push(PaneState::new("r4".into(), "a4".into()));
        st.mode = Mode::Agent;
        for cursor in [1usize, 2, 3, 4] {
            st.rail_cursor = cursor;
            reduce(&mut st, &press(KeyCode::Char('s')));
        }
        assert_eq!(st.detail.shown.len(), 4, "fifth split is refused");
        assert!(!st.detail.shown.contains(&"r4".to_string()));
    }

    #[test]
    fn u_unsplits_focused_slot_but_never_below_one() {
        let mut st = agents3();
        st.mode = Mode::Agent;
        st.rail_cursor = 1;
        reduce(&mut st, &press(KeyCode::Char('s'))); // [r0, r1], focus 1
        reduce(&mut st, &press(KeyCode::Char('u')));
        assert_eq!(st.detail.shown, vec!["r0".to_string()]);
        assert_eq!(st.detail.focus, 0);
        reduce(&mut st, &press(KeyCode::Char('u'))); // already solo — no-op
        assert_eq!(st.detail.shown, vec!["r0".to_string()]);
    }

    #[test]
    fn tab_cycles_detail_focus_and_digits_jump() {
        let mut st = agents3();
        st.mode = Mode::Agent;
        st.rail_cursor = 1;
        reduce(&mut st, &press(KeyCode::Char('s'))); // [r0, r1], focus 1
        reduce(&mut st, &press(KeyCode::Tab));
        assert_eq!(st.detail.focus, 0, "Tab wraps focus");
        reduce(&mut st, &press(KeyCode::Char('2')));
        assert_eq!(st.detail.focus, 1, "digit jumps to slot");
        reduce(&mut st, &press(KeyCode::Char('9')));
        assert_eq!(st.detail.focus, 1, "out-of-range digit ignored");
        reduce(&mut st, &press(KeyCode::Left));
        assert_eq!(st.detail.focus, 0, "Left cycles backward");
    }

    #[test]
    fn n_opens_picker_with_available_agents() {
        let mut st = AppState::new(pane());
        st.mode = Mode::Agent;
        st.available_agents = vec!["fake".into(), "claude-acp".into()];
        reduce(&mut st, &press(KeyCode::Char('n')));
        assert_eq!(st.mode, Mode::Picker);
        let p = st.picker.as_ref().expect("picker set");
        assert_eq!(p.agents, vec!["fake".to_string(), "claude-acp".to_string()]);
        assert_eq!(p.selected, 0);
    }

    // ── Close. ──

    #[test]
    fn x_closes_cursor_agent_and_emits_close_agent() {
        let mut st = agents3();
        st.mode = Mode::Agent;
        st.rail_cursor = 1; // r1
        let fx = reduce(&mut st, &press(KeyCode::Char('x')));
        assert_eq!(
            fx,
            vec![Effect::CloseAgent {
                record_id: "r1".into()
            }]
        );
        assert_eq!(st.agents.len(), 2);
        assert_eq!(st.agents[0].record_id, "r0");
        assert_eq!(st.agents[1].record_id, "r2");
        assert!(!st.should_quit);
    }

    #[test]
    fn x_on_last_agent_sets_should_quit() {
        let mut st = AppState::new(pane());
        st.mode = Mode::Agent;
        let fx = reduce(&mut st, &press(KeyCode::Char('x')));
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
        st.mode = Mode::Agent;
        st.rail_cursor = 1; // roster: [r2(attn), r0, r1] → cursor 1 = r0
        let fx = reduce(&mut st, &press(KeyCode::Char('x')));
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

    // ── Broadcast. ──

    fn bc_state() -> AppState {
        let mut st = agents3();
        st.mode = Mode::Broadcast;
        st
    }

    #[test]
    fn ctrl_b_enters_broadcast_and_clears_selection() {
        let mut st = agents3();
        st.agents[0].selected = true;
        reduce(
            &mut st,
            &AppEvent::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
        );
        assert_eq!(st.mode, Mode::Broadcast);
        assert!(!st.agents[0].selected);
    }

    #[test]
    fn space_toggles_rail_cursor_selection() {
        let mut st = bc_state();
        st.rail_cursor = 1; // r1 (same bucket → roster = spawn order)
        reduce(&mut st, &press(KeyCode::Char(' ')));
        assert!(st.agents[1].selected);
        reduce(&mut st, &press(KeyCode::Char(' ')));
        assert!(!st.agents[1].selected);
    }

    #[test]
    fn digit_toggles_nth_roster_row() {
        let mut st = bc_state();
        reduce(&mut st, &press(KeyCode::Char('3'))); // roster row 3 = r2
        assert!(st.agents[2].selected);
        reduce(&mut st, &press(KeyCode::Char('9'))); // out of range → no-op
        assert!(st.agents.iter().filter(|p| p.selected).count() == 1);
    }

    #[test]
    fn a_selects_all_agents() {
        let mut st = bc_state();
        reduce(&mut st, &press(KeyCode::Char('a')));
        assert!(st.agents.iter().all(|p| p.selected));
    }

    #[test]
    fn typing_builds_broadcast_input() {
        let mut st = bc_state();
        for c in ['h', 'i'] {
            reduce(&mut st, &press(KeyCode::Char(c)));
        }
        assert_eq!(st.broadcast_input, "hi");
    }

    #[test]
    fn enter_sends_to_selected_and_returns_to_normal() {
        let mut st = bc_state();
        st.agents[0].selected = true;
        st.agents[2].selected = true;
        st.broadcast_input = "go".into();
        let fx = reduce(&mut st, &press(KeyCode::Enter));
        assert_eq!(
            fx,
            vec![
                Effect::Prompt {
                    record_id: "r0".into(),
                    text: "go".into()
                },
                Effect::Prompt {
                    record_id: "r2".into(),
                    text: "go".into()
                },
            ]
        );
        assert_eq!(st.mode, Mode::Normal);
        assert_eq!(st.broadcast_input, "");
        assert!(st.agents.iter().all(|p| !p.selected));
        assert_eq!(st.agents[0].lines, vec![Line::UserPrompt("go".into())]);
        assert!(st.agents[1].lines.is_empty());
        assert_eq!(st.agents[2].lines, vec![Line::UserPrompt("go".into())]);
    }

    #[test]
    fn enter_with_no_selection_is_a_noop_but_exits() {
        let mut st = bc_state();
        st.broadcast_input = "go".into();
        let fx = reduce(&mut st, &press(KeyCode::Enter));
        assert!(fx.is_empty());
        assert_eq!(st.mode, Mode::Normal);
        assert_eq!(st.broadcast_input, "");
    }

    #[test]
    fn esc_cancels_broadcast() {
        let mut st = bc_state();
        st.agents[0].selected = true;
        st.broadcast_input = "x".into();
        let fx = reduce(&mut st, &press(KeyCode::Esc));
        assert!(fx.is_empty());
        assert_eq!(st.mode, Mode::Normal);
        assert_eq!(st.broadcast_input, "");
        assert!(!st.agents[0].selected);
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
        st.mode = Mode::Agent;
        st.rail_cursor = 1;
        reduce(&mut st, &press(KeyCode::Char('s'))); // show r0 + r1
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
    fn rail_y_resolves_cursor_pending_not_the_focused_pane() {
        let mut st = agents3(); // detail = [r0]
        reduce(&mut st, &perm("r0", "focused wants"));
        reduce(&mut st, &perm("r1", "background wants"));
        st.mode = Mode::Agent;
        // Queue: r0 (older) row 0, r1 row 1. Cursor to r1.
        st.rail_cursor = 1;
        let fx = reduce(&mut st, &press(KeyCode::Char('y')));
        assert_eq!(
            fx,
            vec![Effect::ResolvePermission {
                record_id: "r1".into(),
                outcome: PermissionOutcome::AllowOnce,
            }]
        );
        assert!(st.agents[1].pending.is_none(), "rail resolve clears pane");
        assert!(
            st.agents[0].pending.is_some(),
            "focused pane's pending untouched"
        );
    }

    #[test]
    fn rail_d_denies_cursor_pending() {
        let mut st = agents3();
        reduce(&mut st, &perm("r1", "wants"));
        st.mode = Mode::Agent;
        st.rail_cursor = 0; // r1 tops the roster
        let fx = reduce(&mut st, &press(KeyCode::Char('d')));
        assert_eq!(
            fx,
            vec![Effect::ResolvePermission {
                record_id: "r1".into(),
                outcome: PermissionOutcome::Deny,
            }]
        );
    }

    #[test]
    fn rail_y_without_pending_is_a_noop() {
        let mut st = agents3();
        st.mode = Mode::Agent;
        let fx = reduce(&mut st, &press(KeyCode::Char('y')));
        assert!(fx.is_empty(), "no pending under cursor → nothing resolves");
    }

    #[test]
    fn q_filters_rail_to_queue_and_esc_clears() {
        let mut st = agents3();
        reduce(&mut st, &perm("r1", "wants"));
        st.mode = Mode::Agent;
        reduce(&mut st, &press(KeyCode::Char('q')));
        assert!(st.queue_only);
        assert_eq!(st.roster(), vec![1], "only the needs-you row remains");
        reduce(&mut st, &press(KeyCode::Esc));
        assert!(!st.queue_only, "Esc restores the full rail");
        assert_eq!(st.mode, Mode::Normal);
        assert_eq!(st.roster().len(), 3);
    }

    #[test]
    fn resolving_last_queued_item_clamps_cursor_in_queue_mode() {
        let mut st = agents3();
        reduce(&mut st, &perm("r1", "wants"));
        st.mode = Mode::Agent;
        reduce(&mut st, &press(KeyCode::Char('q')));
        let fx = reduce(&mut st, &press(KeyCode::Char('y')));
        assert_eq!(fx.len(), 1);
        assert!(st.roster().is_empty(), "queue drained");
        assert_eq!(st.rail_cursor, 0, "cursor clamped, no panic");
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
        // Still tops the roster (background death = attention), but the
        // queue itself no longer lists it.
        st.queue_only = true;
        assert!(
            st.roster().is_empty(),
            "queue no longer lists the dead agent"
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
    fn capital_a_cycles_autonomy_and_logs() {
        let mut st = agents3();
        st.mode = Mode::Agent;
        st.rail_cursor = 0; // r0
        reduce(&mut st, &press(KeyCode::Char('A')));
        assert_eq!(st.agents[0].autonomy, Autonomy::Assisted);
        reduce(&mut st, &press(KeyCode::Char('A')));
        assert_eq!(st.agents[0].autonomy, Autonomy::Auto);
        reduce(&mut st, &press(KeyCode::Char('A')));
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
    fn colon_on_empty_prompt_opens_palette_mid_sentence_stays_literal() {
        let mut st = AppState::new(pane());
        reduce(&mut st, &press(KeyCode::Char(':')));
        assert_eq!(st.mode, Mode::Command);
        assert!(st.palette.is_some());

        let mut st = AppState::new(pane());
        st.input = "add field x".into();
        reduce(&mut st, &press(KeyCode::Char(':')));
        assert_eq!(st.mode, Mode::Normal, "mid-sentence colon types");
        assert_eq!(st.input, "add field x:");
    }

    #[test]
    fn palette_fuzzy_filters_by_subsequence() {
        let p = PaletteState {
            input: "spw".into(),
            selected: 0,
        };
        let names: Vec<&str> = p.matches().iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["spawn agent"], "s-p-w subsequence");

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
    fn agent_question_mark_opens_keys_help_and_any_key_dismisses() {
        let mut st = AppState::new(pane());
        st.mode = Mode::Agent;
        reduce(&mut st, &press(KeyCode::Char('?')));
        assert!(st.keys_help);
        // The dismissing key is swallowed, not acted on.
        let fx = reduce(&mut st, &press(KeyCode::Char('x')));
        assert!(fx.is_empty(), "dismiss key must not close an agent");
        assert!(!st.keys_help);
        assert_eq!(st.agents.len(), 1);
    }

    #[test]
    fn opening_an_agent_clears_its_attention() {
        let mut st = agents3();
        st.agents[1].attention = true;
        st.mode = Mode::Agent;
        st.rail_cursor = 0; // roster: [r1(attn), r0, r2] → cursor 0 = r1
        reduce(&mut st, &press(KeyCode::Enter));
        assert_eq!(st.detail.shown, vec!["r1".to_string()]);
        assert!(!st.agents[1].attention, "looking at it clears attention");
    }
}
