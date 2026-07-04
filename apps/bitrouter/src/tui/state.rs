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
    // Implemented in Tasks 3–5.
    let _ = (state, event);
    Vec::new()
}
