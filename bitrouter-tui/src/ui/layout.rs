use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Pre-computed layout rectangles for the TUI.
pub struct AppLayout {
    pub top_bar: Rect,
    pub feed: Rect,
    pub input_bar: Rect,
    pub status_bar: Rect,
}

impl AppLayout {
    /// Compute the layout from the terminal area.
    pub fn compute(area: Rect) -> Self {
        let cols = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // top bar (agent pills)
                Constraint::Min(0),    // feed (fills remaining)
                Constraint::Length(3), // input bar
                Constraint::Length(1), // status bar
            ])
            .split(area);

        Self {
            top_bar: cols[0],
            feed: cols[1],
            input_bar: cols[2],
            status_bar: cols[3],
        }
    }
}

/// Compute a centered rectangle of the given percentage size within `area`.
pub fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_width = area.width * percent_x / 100;
    let popup_height = area.height * percent_y / 100;
    let x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_height)) / 2;
    Rect::new(x, y, popup_width, popup_height)
}
