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

/// Whole-app render state. M1: exactly one pane. `focus` is its index.
#[derive(Debug, Clone)]
pub struct AppState {
    pub panes: Vec<PaneState>,
    pub focus: usize,
    pub input: String,
    pub should_quit: bool,
    pub mode: Mode,
    pub zoom: bool,
    pub picker: Option<PickerState>,
    pub available_agents: Vec<String>,
    pub notice: Option<String>,
}

impl AppState {
    pub fn new(pane: PaneState) -> Self {
        Self {
            panes: vec![pane],
            focus: 0,
            input: String::new(),
            should_quit: false,
            mode: Mode::Normal,
            zoom: false,
            picker: None,
            available_agents: Vec::new(),
            notice: None,
        }
    }

    /// The focused pane. M1 always has ≥1 pane.
    pub fn focused_mut(&mut self) -> Option<&mut PaneState> {
        self.panes.get_mut(self.focus)
    }

    pub fn focused(&self) -> Option<&PaneState> {
        self.panes.get(self.focus)
    }

    fn pane_by_id_mut(&mut self, record_id: &str) -> Option<&mut PaneState> {
        self.panes.iter_mut().find(|p| p.record_id == record_id)
    }

    /// Set the list of agent ids the picker offers (from the config catalog).
    pub fn set_available_agents(&mut self, agents: Vec<String>) {
        self.available_agents = agents;
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
        AppEvent::AgentSpawned { .. } => Vec::new(), // implemented in Task 4
        AppEvent::AgentSpawnFailed { .. } => Vec::new(), // implemented in Task 4
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
            if !state.panes.is_empty() {
                state.focus = (state.focus + 1) % state.panes.len();
            }
            Vec::new()
        }
        KeyCode::Left | KeyCode::Char('h') => {
            if !state.panes.is_empty() {
                state.focus = (state.focus + state.panes.len() - 1) % state.panes.len();
            }
            Vec::new()
        }
        KeyCode::Char(c @ '1'..='9') => {
            let idx = (c as usize) - ('1' as usize);
            if idx < state.panes.len() {
                state.focus = idx;
            }
            Vec::new()
        }
        KeyCode::Char('x') => close_focused(state),
        _ => Vec::new(),
    }
}

/// Remove the focused pane, adjust `focus`, and emit `CloseAgent` so the run
/// loop shuts the session down. Quitting the last pane exits the TUI.
fn close_focused(state: &mut AppState) -> Vec<Effect> {
    let record_id = match state.panes.get(state.focus) {
        Some(pane) => pane.record_id.clone(),
        None => return Vec::new(),
    };
    state.panes.remove(state.focus);
    if state.panes.is_empty() {
        state.should_quit = true;
    } else if state.focus >= state.panes.len() {
        state.focus = state.panes.len() - 1;
    }
    vec![Effect::CloseAgent { record_id }]
}

/// PICKER-mode keys. Filled in Task 3.
fn reduce_key_picker(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    let _ = (state, key);
    Vec::new()
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
        let pending = st.panes[0].pending.as_ref().expect("pending set");
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
            st.panes[0].pending.is_none(),
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
        assert_eq!(st.panes[0].lines, vec![Line::Message("hi".into())]);
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
            st.panes[0].lines,
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
        assert!(st.panes[0].lines.is_empty());
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
            st.panes[0].lines,
            vec![Line::UserPrompt("fix the bug".into())]
        );
    }

    #[test]
    fn enter_on_empty_input_is_a_noop() {
        let mut st = AppState::new(pane());
        let effects = reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Enter)));
        assert!(effects.is_empty());
        assert!(st.panes[0].lines.is_empty());
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
        st.panes.push(PaneState::new("r1".into(), "a1".into()));
        st.panes.push(PaneState::new("r2".into(), "a2".into()));
        st
    }

    #[test]
    fn tab_cycles_focus_forward_wrapping() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = panes3();
        st.mode = Mode::Agent;
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Tab)));
        assert_eq!(st.focus, 1);
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Tab)));
        assert_eq!(st.focus, 2);
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Tab)));
        assert_eq!(st.focus, 0);
    }

    #[test]
    fn left_cycles_focus_backward_wrapping() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = panes3();
        st.mode = Mode::Agent;
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Left)));
        assert_eq!(st.focus, 2);
    }

    #[test]
    fn digit_focuses_pane_in_range_only() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut st = panes3();
        st.mode = Mode::Agent;
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char('3'))));
        assert_eq!(st.focus, 2);
        reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char('9'))));
        assert_eq!(st.focus, 2); // out of range → unchanged
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
        st.focus = 1;
        let fx = reduce(&mut st, &AppEvent::Key(KeyEvent::from(KeyCode::Char('x'))));
        assert_eq!(
            fx,
            vec![Effect::CloseAgent {
                record_id: "r1".into()
            }]
        );
        assert_eq!(st.panes.len(), 2);
        assert_eq!(st.panes[0].record_id, "r0");
        assert_eq!(st.panes[1].record_id, "r2");
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
        assert!(st.panes.is_empty());
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
}
