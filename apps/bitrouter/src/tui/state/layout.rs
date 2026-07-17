//! Detail-viewport layout and mouse hit-testing: how the 1-4 shown panes
//! are split (`Split`, `DetailLayout`) and the clickable regions the
//! renderer records each frame (`ClickTarget`, `ClickZone`).

use super::MAX_SHOWN;

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
    pub(super) fn solo(record_id: String) -> Self {
        Self {
            shown: vec![record_id],
            split: Split::H,
            focus: 0,
        }
    }

    /// Add `record_id` as a new slot in `split` direction (or refocus it if
    /// already shown). Full viewport (MAX_SHOWN) refocuses instead of adding.
    pub(super) fn add(&mut self, record_id: String, split: Split) {
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
    pub(super) fn remove_focused(&mut self) {
        if self.shown.len() > 1 {
            self.shown.remove(self.focus);
            if self.focus >= self.shown.len() {
                self.focus = self.shown.len() - 1;
            }
        }
    }

    /// Drop `record_id` from the layout if shown; clamps focus.
    pub(super) fn prune(&mut self, record_id: &str) {
        self.shown.retain(|r| r != record_id);
        if self.focus >= self.shown.len() {
            self.focus = self.shown.len().saturating_sub(1);
        }
    }

    /// The focused slot's record id.
    pub(super) fn focused_id(&self) -> Option<&str> {
        self.shown.get(self.focus).map(String::as_str)
    }
}

/// What a recorded click zone does when the human clicks inside it. The
/// renderer rebuilds the zone list every frame (like [`AppState::pty_areas`]);
/// the [`AppEvent::Click`] reducer hit-tests the pointer against them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickTarget {
    /// The `<<` status-bar button: toggle the sessions (left) sidebar.
    ToggleSessions,
    /// The `>>` status-bar button: toggle the subagents (right) sidebar.
    ToggleSubagents,
    /// A sessions-panel row — an index into [`AppState::sessions_list`] order.
    SessionRow(usize),
    /// A subagents-rail row — an index into [`AppState::roster`] order.
    RailRow(usize),
    /// The sessions panel's `+ new session` footer — opens the harness picker.
    NewSession,
}

/// A clickable region recorded by the renderer for the current frame. Pure
/// geometry (no `ratatui` in this module — the renderer converts its `Rect`s),
/// so the reducer can hit-test the pointer without a retained widget tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClickZone {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub target: ClickTarget,
}

impl ClickZone {
    /// Whether cell `(col, row)` falls inside this zone (top-left inclusive).
    pub(super) fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.x && col < self.x + self.w && row >= self.y && row < self.y + self.h
    }
}
