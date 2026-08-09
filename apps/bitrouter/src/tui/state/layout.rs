//! Viewport geometry and mouse hit-testing: the clickable regions the
//! renderer records each frame (`ClickTarget`, `ClickZone`) and the PTY
//! pane's drawn content rect (`PtyArea`).
//!
//! There is no split model. The viewport shows exactly ONE agent — the
//! focused one ([`AppState::focus`](super::AppState::focus)) — because the
//! terminal multiplexer the user is already running does splitting better
//! than a PTY emulator nested inside another PTY emulator can.

/// What a recorded click zone does when the human clicks inside it. The
/// renderer rebuilds the zone list every frame (like [`AppState::pty_areas`]);
/// the [`AppEvent::Click`] reducer hit-tests the pointer against them.
///
/// [`AppState::pty_areas`]: super::AppState::pty_areas
/// [`AppEvent::Click`]: crate::tui::event::AppEvent::Click
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickTarget {
    /// A manager-view row — an index into [`AppState::fleet`] order.
    ///
    /// [`AppState::fleet`]: super::AppState::fleet
    AgentRow(usize),
    /// The manager's `+ new session` footer — opens the harness picker.
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

/// A PTY pane's drawn **content** rectangle (inside its border), recorded by
/// the renderer each frame. Drives two loop-side jobs: resizing the emulator +
/// PTY (SIGWINCH) when the layout changes, and hit-testing the pointer so
/// mouse events over a mouse-reporting inner app forward to it (pane-relative).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyArea {
    pub record_id: String,
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
}

impl PtyArea {
    /// Whether cell `(col, row)` falls inside the content area.
    pub fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.x && col < self.x + self.cols && row >= self.y && row < self.y + self.rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_area_contains_is_half_open() {
        // Content area at (2,1), 4 cols × 3 rows → cols [2,6), rows [1,4).
        let area = PtyArea {
            record_id: "rec".into(),
            x: 2,
            y: 1,
            cols: 4,
            rows: 3,
        };
        assert!(area.contains(2, 1), "top-left inclusive");
        assert!(area.contains(5, 3), "bottom-right inclusive");
        assert!(!area.contains(6, 3), "one past the last column is outside");
        assert!(!area.contains(5, 4), "one past the last row is outside");
        assert!(!area.contains(1, 1), "the border column is outside");
    }

    #[test]
    fn click_zone_contains_is_half_open() {
        let zone = ClickZone {
            x: 4,
            y: 2,
            w: 3,
            h: 2,
            target: ClickTarget::AgentRow(0),
        };
        assert!(zone.contains(4, 2));
        assert!(zone.contains(6, 3));
        assert!(!zone.contains(7, 3));
        assert!(!zone.contains(6, 4));
    }
}
