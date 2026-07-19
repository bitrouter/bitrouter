//! Overlay / mode state: the key-handling `Mode`, the command palette
//! (`Command`, `PaletteState`), the leader menu (`LeaderAction`), and the
//! agent picker (`PickerState`), plus leader-chord parsing.

use crossterm::event::{KeyCode, KeyModifiers};

/// Which key-handling mode the TUI is in. NORMAL is the only hub
/// (TUI_SPEC_V3 §3/I3): supervision is inline, and the one-shot leader
/// prefix covers the few rare verbs — there is no sticky manager mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Keys go to the focused pane (PTY passthrough); supervision inline.
    Normal,
    /// One-shot leader prefix: the which-key overlay is up and exactly one
    /// leaf key runs, then back to `Normal` (or into a `Command`/`Picker`
    /// leaf). Never sticky.
    Leader,
    /// Selecting an agent to spawn.
    Picker,
    /// Fuzzy command palette (`:`, or `leader p`).
    Command,
    /// Approving the worktree bootstrap hook before the first isolated spawn
    /// (it executes shell — shown to the human on first use, per session).
    Confirm,
}

/// The one-shot leader prefix (TUI_SPEC_V3 §3): `Ctrl-Space` by default —
/// never `Ctrl-A`/`Ctrl-B`, which are readline keys the orchestrator PTY
/// owns.
pub const DEFAULT_LEADER: (KeyCode, KeyModifiers) = (KeyCode::Char(' '), KeyModifiers::CONTROL);

/// Parse a `tui.leader` spec (`ctrl-<key>`, `<key>` = one char or `space`)
/// into the chord the reducer matches. `None` = unparseable — the caller
/// falls back to [`DEFAULT_LEADER`].
pub fn parse_leader(spec: &str) -> Option<(KeyCode, KeyModifiers)> {
    let rest = spec.trim().to_ascii_lowercase();
    let rest = rest.strip_prefix("ctrl-")?;
    let code = match rest {
        "space" => KeyCode::Char(' '),
        _ => {
            let mut chars = rest.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None; // exactly one key after the modifier
            }
            KeyCode::Char(c)
        }
    };
    Some((code, KeyModifiers::CONTROL))
}

/// One palette command. The table is static; actions map onto existing
/// reducer paths so the palette adds discoverability, not new behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    SpawnAgent,
    NewSession,
    CloseAgent,
    SplitH,
    SplitV,
    Unsplit,
    Autonomy,
    KillDone,
    ToggleSessions,
    ToggleSubagents,
    KeysHelp,
    Quit,
}

/// Palette entries: display name → command. Order = display order when the
/// filter is empty.
pub const COMMANDS: &[(&str, Command)] = &[
    ("spawn subagent", Command::SpawnAgent),
    ("new session", Command::NewSession),
    ("close agent", Command::CloseAgent),
    ("split horizontal", Command::SplitH),
    ("split vertical", Command::SplitV),
    ("unsplit", Command::Unsplit),
    ("autonomy cycle", Command::Autonomy),
    ("kill done", Command::KillDone),
    ("toggle sessions", Command::ToggleSessions),
    ("toggle subagents", Command::ToggleSubagents),
    ("keys help", Command::KeysHelp),
    ("quit", Command::Quit),
];

/// One leader leaf (TUI_SPEC_V3 §3). Dispatched by `leader_action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaderAction {
    /// Focus session N (0-based; `leader 1..9`).
    FocusSession(usize),
    NextActionable,
    NewSession,
    Palette,
    Close,
    Autonomy,
    Attach,
    KeysHelp,
}

/// The leader leaf map — the single source both `leader_action` (dispatch)
/// and the which-key overlay (docs) derive from, so a binding and its help
/// line cannot drift apart (TUI_SPEC_V3 §9 keyboard parity). `1-9` (a key
/// range) and `Esc` (the fall-through cancel) are the only rows the overlay
/// adds by hand.
pub const LEADER_LEAVES: &[(KeyCode, &str, LeaderAction)] = &[
    (
        KeyCode::Tab,
        "focus next actionable subagent",
        LeaderAction::NextActionable,
    ),
    (
        KeyCode::Char('n'),
        "new session (harness picker)",
        LeaderAction::NewSession,
    ),
    (KeyCode::Char('p'), "command palette", LeaderAction::Palette),
    (
        KeyCode::Char('c'),
        "close the focused pane",
        LeaderAction::Close,
    ),
    (
        KeyCode::Char('a'),
        "cycle its autonomy tier",
        LeaderAction::Autonomy,
    ),
    (
        KeyCode::Char('t'),
        "attach: drive it natively",
        LeaderAction::Attach,
    ),
    (KeyCode::Char('?'), "keys help", LeaderAction::KeysHelp),
];

/// Resolve a leader leaf key: digits focus sessions, everything else comes
/// from `LEADER_LEAVES`, and `None` cancels the prefix.
pub(super) fn leader_action(code: KeyCode) -> Option<LeaderAction> {
    if let KeyCode::Char(c @ '1'..='9') = code {
        return Some(LeaderAction::FocusSession((c as usize) - ('1' as usize)));
    }
    LEADER_LEAVES
        .iter()
        .find(|(key, _, _)| *key == code)
        .map(|&(_, _, action)| action)
}

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

/// What the picker overlay spawns on Enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerPurpose {
    /// An ACP subagent from the config catalog — the human-spawn hatch,
    /// reachable only as the palette entry `spawn subagent` (TUI_SPEC_V3 §4).
    Subagent,
    /// A native orchestrator session on a PTY (`N` / `new session`).
    Session,
}

/// State of the agent picker overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerState {
    pub agents: Vec<String>,
    pub selected: usize,
    pub purpose: PickerPurpose,
}
