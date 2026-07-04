//! Pure render state + reducer for the TUI. No `ratatui`/`tokio` deps.

use crate::tui::event::{AppEvent, Effect, PermOption};
use bitrouter_substrate::translate::{PermissionOutcome, SessionUpdateKind, ToolStatus};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Max scrollback lines retained per pane (ring buffer).
const SCROLLBACK_CAP: usize = 2000;

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
}

impl AppState {
    pub fn new(pane: PaneState) -> Self {
        Self {
            panes: vec![pane],
            focus: 0,
            input: String::new(),
            should_quit: false,
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
        AppEvent::Key(key) => reduce_key(state, key),
    }
}

/// Handle a keypress. Permission keys take priority when a prompt is pending.
fn reduce_key(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    let focus_id = match state.focused() {
        Some(p) => p.record_id.clone(),
        None => return Vec::new(),
    };
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

    // Input editing / submit lands in Task 5.
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
}
