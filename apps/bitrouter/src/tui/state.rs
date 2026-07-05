//! Pure render state + reducer for the TUI. No `ratatui`/`tokio` deps.

use crate::tui::event::{AppEvent, Effect, PermOption};
use bitrouter_substrate::translate::{PermissionOutcome, SessionUpdateKind, ToolStatus};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Max scrollback lines retained per pane (ring buffer).
const SCROLLBACK_CAP: usize = 2000;

/// Which key-handling mode the TUI is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Keys go to the focused pane's prompt (default).
    Normal,
    /// Pane-management keys (new/close/focus/zoom).
    Agent,
    /// Selecting an agent to spawn.
    Picker,
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
}

/// A pending permission surfaced in the pane, as display data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingView {
    pub title: String,
    pub diff: Option<String>,
    pub options: Vec<PermOption>,
}

/// One agent pane's state.
#[derive(Debug, Clone)]
pub struct PaneState {
    pub record_id: String,
    pub agent_id: String,
    pub lines: Vec<Line>,
    pub pending: Option<PendingView>,
    pub exited: bool,
}

impl PaneState {
    pub fn new(record_id: String, agent_id: String) -> Self {
        Self {
            record_id,
            agent_id,
            lines: Vec::new(),
            pending: None,
            exited: false,
        }
    }

    fn push(&mut self, line: Line) {
        self.lines.push(line);
        if self.lines.len() > SCROLLBACK_CAP {
            let overflow = self.lines.len() - SCROLLBACK_CAP;
            self.lines.drain(0..overflow);
        }
    }
}

/// A cohort of agent panes shown together. `focus` indexes `panes`.
#[derive(Debug, Clone)]
pub struct Tab {
    pub title: String,
    pub panes: Vec<PaneState>,
    pub focus: usize,
}

/// Whole-app render state. Holds N tabs, each with N agent panes; `active_tab`
/// indexes `tabs`. Accessors return `Option` because a tab or pane may be absent
/// transiently (e.g. right after the last pane closes, before `should_quit`).
#[derive(Debug, Clone)]
pub struct AppState {
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    pub input: String,
    pub should_quit: bool,
    pub mode: Mode,
    pub zoom: bool,
    pub picker: Option<PickerState>,
    pub available_agents: Vec<String>,
    pub notice: Option<String>,
    pub broadcast_input: String,
}

impl AppState {
    pub fn new(pane: PaneState) -> Self {
        Self {
            tabs: vec![Tab {
                title: "1".to_string(),
                panes: vec![pane],
                focus: 0,
            }],
            active_tab: 0,
            input: String::new(),
            should_quit: false,
            mode: Mode::Normal,
            zoom: false,
            picker: None,
            available_agents: Vec::new(),
            notice: None,
            broadcast_input: String::new(),
        }
    }

    /// Set the list of agent ids the picker offers (from the config catalog).
    pub fn set_available_agents(&mut self, agents: Vec<String>) {
        self.available_agents = agents;
    }

    /// The active tab.
    pub fn active(&self) -> Option<&Tab> {
        self.tabs.get(self.active_tab)
    }

    /// The active tab, mutably.
    pub fn active_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active_tab)
    }

    /// The active tab's focused pane.
    pub fn focused(&self) -> Option<&PaneState> {
        let t = self.tabs.get(self.active_tab)?;
        t.panes.get(t.focus)
    }

    /// The active tab's focused pane, mutably.
    pub fn focused_mut(&mut self) -> Option<&mut PaneState> {
        let t = self.tabs.get_mut(self.active_tab)?;
        t.panes.get_mut(t.focus)
    }

    /// Find a pane by `record_id` across ALL tabs (updates/permissions may target
    /// a pane in a non-active tab).
    fn pane_by_id_mut(&mut self, record_id: &str) -> Option<&mut PaneState> {
        self.tabs
            .iter_mut()
            .flat_map(|t| t.panes.iter_mut())
            .find(|p| p.record_id == record_id)
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
            if let Some(pane) = state.pane_by_id_mut(record_id) {
                pane.exited = true;
            }
            Vec::new()
        }
        AppEvent::Permission {
            record_id,
            title,
            diff,
            options,
        } => {
            if let Some(pane) = state.pane_by_id_mut(record_id) {
                pane.pending = Some(PendingView {
                    title: title.clone(),
                    diff: diff.clone(),
                    options: options.clone(),
                });
            }
            Vec::new()
        }
        AppEvent::AgentSpawned {
            record_id,
            agent_id,
        } => {
            if let Some(tab) = state.active_mut() {
                tab.panes
                    .push(PaneState::new(record_id.clone(), agent_id.clone()));
                tab.focus = tab.panes.len() - 1;
            }
            state.notice = None;
            Vec::new()
        }
        AppEvent::AgentSpawnFailed { agent_id, error } => {
            state.notice = Some(format!("failed to spawn {agent_id}: {error}"));
            Vec::new()
        }
        AppEvent::Key(key) => match state.mode {
            Mode::Normal => reduce_key_normal(state, key),
            Mode::Agent => reduce_key_agent(state, key),
            Mode::Picker => reduce_key_picker(state, key),
        },
    }
}

/// NORMAL-mode keys. Permission keys take priority when a prompt is pending.
fn reduce_key_normal(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    // Ctrl-A enters AGENT (pane-management) mode.
    if key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL) {
        state.mode = Mode::Agent;
        return Vec::new();
    }
    let focus_id = match state.focused() {
        Some(p) => p.record_id.clone(),
        None => return Vec::new(),
    };
    let has_pending = state
        .focused()
        .map(|p| p.pending.is_some())
        .unwrap_or(false);

    if has_pending {
        // Ctrl-C must escape even a pending permission. Dropping the pending
        // handle in the run loop's teardown defaults the request to Deny, so
        // this is safe.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            state.should_quit = true;
            return vec![Effect::Quit];
        }
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

    // Ctrl-C quits from anywhere.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        state.should_quit = true;
        return vec![Effect::Quit];
    }

    match key.code {
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

/// AGENT-mode keys: pane management.
fn reduce_key_agent(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Esc => {
            state.mode = Mode::Normal;
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
        KeyCode::Char('f') => {
            state.zoom = !state.zoom;
            Vec::new()
        }
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
            if let Some(tab) = state.active_mut()
                && !tab.panes.is_empty()
            {
                tab.focus = (tab.focus + 1) % tab.panes.len();
            }
            Vec::new()
        }
        KeyCode::Left | KeyCode::Char('h') => {
            if let Some(tab) = state.active_mut()
                && !tab.panes.is_empty()
            {
                tab.focus = (tab.focus + tab.panes.len() - 1) % tab.panes.len();
            }
            Vec::new()
        }
        KeyCode::Char(c @ '1'..='9') => {
            let idx = (c as usize) - ('1' as usize);
            if let Some(tab) = state.active_mut()
                && idx < tab.panes.len()
            {
                tab.focus = idx;
            }
            Vec::new()
        }
        KeyCode::Char('x') => close_focused(state),
        KeyCode::Char('t') => {
            let title = (state.tabs.len() + 1).to_string();
            state.tabs.push(Tab {
                title,
                panes: Vec::new(),
                focus: 0,
            });
            state.active_tab = state.tabs.len() - 1;
            Vec::new()
        }
        KeyCode::Char(']') => {
            if !state.tabs.is_empty() {
                state.active_tab = (state.active_tab + 1) % state.tabs.len();
            }
            Vec::new()
        }
        KeyCode::Char('[') => {
            if !state.tabs.is_empty() {
                state.active_tab = (state.active_tab + state.tabs.len() - 1) % state.tabs.len();
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Remove the active tab's focused pane, emit `CloseAgent`, and (if the tab is
/// now empty) remove the tab. Quitting the last pane of the last tab exits.
fn close_focused(state: &mut AppState) -> Vec<Effect> {
    let (record_id, tab_now_empty) = {
        let tab = match state.active_mut() {
            Some(t) => t,
            None => return Vec::new(),
        };
        let record_id = match tab.panes.get(tab.focus) {
            Some(pane) => pane.record_id.clone(),
            None => return Vec::new(),
        };
        tab.panes.remove(tab.focus);
        if tab.panes.is_empty() {
            (record_id, true)
        } else {
            if tab.focus >= tab.panes.len() {
                tab.focus = tab.panes.len() - 1;
            }
            (record_id, false)
        }
    };
    if tab_now_empty {
        state.tabs.remove(state.active_tab);
        if state.tabs.is_empty() {
            state.should_quit = true;
        } else if state.active_tab >= state.tabs.len() {
            state.active_tab = state.tabs.len() - 1;
        }
    }
    vec![Effect::CloseAgent { record_id }]
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
    use crate::tui::event::{AppEvent, Effect, PermOption};
    use bitrouter_substrate::translate::{PermissionOutcome, SessionUpdateKind, ToolStatus};

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

    #[test]
    fn new_app_has_one_tab_with_one_pane() {
        let st = AppState::new(pane());
        assert_eq!(st.tabs.len(), 1);
        assert_eq!(st.active_tab, 0);
        assert_eq!(st.tabs[0].panes.len(), 1);
        assert_eq!(st.tabs[0].focus, 0);
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
            },
        );
        let pending = st.tabs[0].panes[0].pending.as_ref().expect("pending set");
        assert_eq!(pending.title, "WRITE src/x.rs");
        assert_eq!(pending.options.len(), 2);
    }

    #[test]
    fn y_key_resolves_pending_allow_once_and_clears_it() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = AppState::new(pane());
        reduce(
            &mut st,
            &AppEvent::Permission {
                record_id: "rec-1".into(),
                title: "WRITE".into(),
                diff: None,
                options: allow_deny(),
            },
        );
        let effects = reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char('y'))));
        assert_eq!(
            effects,
            vec![Effect::ResolvePermission {
                record_id: "rec-1".into(),
                outcome: PermissionOutcome::AllowOnce,
            }]
        );
        assert!(
            st.tabs[0].panes[0].pending.is_none(),
            "pending cleared after resolve"
        );
    }

    #[test]
    fn n_key_resolves_pending_deny() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = AppState::new(pane());
        reduce(
            &mut st,
            &AppEvent::Permission {
                record_id: "rec-1".into(),
                title: "WRITE".into(),
                diff: None,
                options: allow_deny(),
            },
        );
        let effects = reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char('n'))));
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
        assert_eq!(st.tabs[0].panes[0].lines, vec![Line::Message("hi".into())]);
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
            st.tabs[0].panes[0].lines,
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
        assert!(st.tabs[0].panes[0].lines.is_empty());
    }

    #[test]
    fn agent_spawned_appends_and_focuses_new_pane() {
        let mut st = AppState::new(pane()); // 1 pane, focus 0
        reduce(
            &mut st,
            &AppEvent::AgentSpawned {
                record_id: "r9".into(),
                agent_id: "fake".into(),
            },
        );
        assert_eq!(st.tabs[0].panes.len(), 2);
        assert_eq!(st.tabs[0].focus, 1);
        assert_eq!(st.tabs[0].panes[1].record_id, "r9");
        assert_eq!(st.tabs[0].panes[1].agent_id, "fake");
    }

    #[test]
    fn second_agent_spawned_focuses_newest() {
        let mut st = AppState::new(pane());
        reduce(
            &mut st,
            &AppEvent::AgentSpawned {
                record_id: "r1".into(),
                agent_id: "a".into(),
            },
        );
        reduce(
            &mut st,
            &AppEvent::AgentSpawned {
                record_id: "r2".into(),
                agent_id: "b".into(),
            },
        );
        assert_eq!(st.tabs[0].panes.len(), 3);
        assert_eq!(st.tabs[0].focus, 2);
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
        assert_eq!(st.tabs[0].panes.len(), 1);
        assert_eq!(st.notice.as_deref(), Some("failed to spawn fake: boom"));
    }

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn typing_appends_to_input() {
        let mut st = AppState::new(pane());
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char('h'))));
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char('i'))));
        assert_eq!(st.input, "hi");
    }

    #[test]
    fn backspace_removes_last_char() {
        let mut st = AppState::new(pane());
        st.input = "hi".into();
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Backspace)));
        assert_eq!(st.input, "h");
    }

    #[test]
    fn enter_emits_prompt_effect_records_line_and_clears_input() {
        let mut st = AppState::new(pane());
        st.input = "fix the bug".into();
        let effects = reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Enter)));
        assert_eq!(
            effects,
            vec![Effect::Prompt {
                record_id: "rec-1".into(),
                text: "fix the bug".into(),
            }]
        );
        assert_eq!(st.input, "");
        assert_eq!(
            st.tabs[0].panes[0].lines,
            vec![Line::UserPrompt("fix the bug".into())]
        );
    }

    #[test]
    fn enter_on_empty_input_is_a_noop() {
        let mut st = AppState::new(pane());
        let effects = reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Enter)));
        assert!(effects.is_empty());
        assert!(st.tabs[0].panes[0].lines.is_empty());
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
    fn ctrl_c_during_pending_permission_quits() {
        let mut st = AppState::new(pane());
        reduce(
            &mut st,
            &AppEvent::Permission {
                record_id: "rec-1".into(),
                title: "WRITE".into(),
                diff: None,
                options: allow_deny(),
            },
        );
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let effects = reduce(&mut st, &AppEvent::Key(key));
        assert_eq!(effects, vec![Effect::Quit]);
        assert!(st.should_quit);
    }

    #[test]
    fn ctrl_a_enters_agent_mode() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut st = AppState::new(pane());
        let fx = reduce(
            &mut st,
            &AppEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        );
        assert!(fx.is_empty());
        assert_eq!(st.mode, Mode::Agent);
    }

    #[test]
    fn esc_returns_to_normal_from_agent() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = AppState::new(pane());
        st.mode = Mode::Agent;
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Esc)));
        assert_eq!(st.mode, Mode::Normal);
    }

    #[test]
    fn ctrl_a_does_not_type_into_input() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut st = AppState::new(pane());
        reduce(
            &mut st,
            &AppEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        );
        assert_eq!(st.input, "");
    }

    fn panes3() -> AppState {
        let mut st = AppState::new(PaneState::new("r0".into(), "a0".into()));
        st.tabs[0]
            .panes
            .push(PaneState::new("r1".into(), "a1".into()));
        st.tabs[0]
            .panes
            .push(PaneState::new("r2".into(), "a2".into()));
        st
    }

    #[test]
    fn tab_cycles_focus_forward_wrapping() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = panes3();
        st.mode = Mode::Agent;
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Tab)));
        assert_eq!(st.tabs[0].focus, 1);
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Tab)));
        assert_eq!(st.tabs[0].focus, 2);
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Tab)));
        assert_eq!(st.tabs[0].focus, 0);
    }

    #[test]
    fn left_cycles_focus_backward_wrapping() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = panes3();
        st.mode = Mode::Agent;
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Left)));
        assert_eq!(st.tabs[0].focus, 2);
    }

    #[test]
    fn digit_focuses_pane_in_range_only() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = panes3();
        st.mode = Mode::Agent;
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char('3'))));
        assert_eq!(st.tabs[0].focus, 2);
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char('9'))));
        assert_eq!(st.tabs[0].focus, 2); // out of range → unchanged
    }

    #[test]
    fn f_toggles_zoom() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = panes3();
        st.mode = Mode::Agent;
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char('f'))));
        assert!(st.zoom);
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char('f'))));
        assert!(!st.zoom);
    }

    #[test]
    fn x_closes_focused_and_emits_close_agent() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = panes3();
        st.mode = Mode::Agent;
        st.tabs[0].focus = 1;
        let fx = reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char('x'))));
        assert_eq!(
            fx,
            vec![Effect::CloseAgent {
                record_id: "r1".into()
            }]
        );
        assert_eq!(st.tabs[0].panes.len(), 2);
        assert_eq!(st.tabs[0].panes[0].record_id, "r0");
        assert_eq!(st.tabs[0].panes[1].record_id, "r2");
    }

    #[test]
    fn x_on_last_pane_sets_should_quit() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = AppState::new(pane());
        st.mode = Mode::Agent;
        let fx = reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char('x'))));
        assert_eq!(
            fx,
            vec![Effect::CloseAgent {
                record_id: "rec-1".into()
            }]
        );
        assert!(st.should_quit);
        assert!(st.tabs.is_empty());
    }

    #[test]
    fn n_opens_picker_with_available_agents() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = AppState::new(pane());
        st.mode = Mode::Agent;
        st.available_agents = vec!["fake".into(), "claude-acp".into()];
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char('n'))));
        assert_eq!(st.mode, Mode::Picker);
        let p = st.picker.as_ref().expect("picker set");
        assert_eq!(p.agents, vec!["fake".to_string(), "claude-acp".to_string()]);
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn t_creates_and_switches_to_new_empty_tab() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = AppState::new(pane());
        st.mode = Mode::Agent;
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char('t'))));
        assert_eq!(st.tabs.len(), 2);
        assert_eq!(st.active_tab, 1);
        assert!(st.tabs[1].panes.is_empty());
    }

    #[test]
    fn bracket_keys_cycle_tabs_with_wrap() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = AppState::new(pane());
        st.mode = Mode::Agent;
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char('t')))); // 2 tabs, active 1
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char('t')))); // 3 tabs, active 2
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char(']'))));
        assert_eq!(st.active_tab, 0); // wrapped forward
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char('['))));
        assert_eq!(st.active_tab, 2); // wrapped backward
    }

    #[test]
    fn spawned_agent_goes_to_active_tab() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = AppState::new(pane());
        st.mode = Mode::Agent;
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char('t')))); // active tab 1, empty
        reduce(
            &mut st,
            &AppEvent::AgentSpawned {
                record_id: "r9".into(),
                agent_id: "fake".into(),
            },
        );
        assert_eq!(st.tabs[1].panes.len(), 1);
        assert_eq!(st.tabs[1].panes[0].record_id, "r9");
        assert_eq!(st.tabs[0].panes.len(), 1); // original tab untouched
    }

    #[test]
    fn closing_last_pane_of_a_tab_removes_that_tab() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = AppState::new(pane());
        st.tabs.push(Tab {
            title: "2".into(),
            panes: vec![PaneState::new("r1".into(), "a1".into())],
            focus: 0,
        });
        st.active_tab = 1;
        st.mode = Mode::Agent;
        let fx = reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char('x'))));
        assert_eq!(
            fx,
            vec![Effect::CloseAgent {
                record_id: "r1".into()
            }]
        );
        assert_eq!(st.tabs.len(), 1); // emptied tab removed
        assert_eq!(st.active_tab, 0); // clamped
        assert!(!st.should_quit);
    }

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
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = picker_state(&["a", "b", "c"]);
        let down = |st: &mut AppState| {
            reduce(st, &AppEvent::Key(KeyEvent::from(KeyCode::Down)));
        };
        let up = |st: &mut AppState| {
            reduce(st, &AppEvent::Key(KeyEvent::from(KeyCode::Up)));
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
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = picker_state(&["fake", "claude"]);
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Down))); // select "claude"
        let fx = reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Enter)));
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
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = picker_state(&["fake"]);
        let fx = reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Esc)));
        assert!(fx.is_empty());
        assert_eq!(st.mode, Mode::Normal);
        assert!(st.picker.is_none());
    }

    #[test]
    fn picker_enter_on_empty_list_just_closes() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = picker_state(&[]);
        let fx = reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Enter)));
        assert!(fx.is_empty());
        assert_eq!(st.mode, Mode::Normal);
        assert!(st.picker.is_none());
    }
}
