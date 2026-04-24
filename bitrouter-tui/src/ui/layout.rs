use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Width of the threads sidebar when visible.
pub const SIDEBAR_WIDTH: u16 = 28;

/// Pre-computed layout rectangles for the TUI.
#[derive(Clone, Copy)]
pub struct AppLayout {
    /// Sidebar column. `None` when the sidebar is hidden.
    pub sidebar: Option<Rect>,
    pub top_bar: Rect,
    pub scrollback: Rect,
    pub status_bar: Rect,
}

impl AppLayout {
    /// Compute the layout from the terminal area.
    pub fn compute(area: Rect, sidebar_visible: bool) -> Self {
        let (sidebar, main) = if sidebar_visible {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(0)])
                .split(area);
            (Some(cols[0]), cols[1])
        } else {
            (None, area)
        };

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // top bar (agent pills)
                Constraint::Min(0),    // scrollback (fills remaining)
                Constraint::Length(1), // status bar
            ])
            .split(main);

        Self {
            sidebar,
            top_bar: rows[0],
            scrollback: rows[1],
            status_bar: rows[2],
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_sidebar_matches_legacy_three_row_layout() {
        let area = Rect::new(0, 0, 100, 30);
        let layout = AppLayout::compute(area, false);
        assert!(layout.sidebar.is_none());
        assert_eq!(layout.top_bar, Rect::new(0, 0, 100, 1));
        assert_eq!(layout.scrollback, Rect::new(0, 1, 100, 28));
        assert_eq!(layout.status_bar, Rect::new(0, 29, 100, 1));
    }

    #[test]
    fn visible_sidebar_reserves_left_column() {
        let area = Rect::new(0, 0, 100, 30);
        let layout = AppLayout::compute(area, true);
        let sidebar = layout.sidebar.expect("sidebar visible");
        assert_eq!(sidebar, Rect::new(0, 0, SIDEBAR_WIDTH, 30));
        // Main area is the remaining width.
        assert_eq!(layout.top_bar.x, SIDEBAR_WIDTH);
        assert_eq!(layout.top_bar.width, 100 - SIDEBAR_WIDTH);
        assert_eq!(layout.scrollback.x, SIDEBAR_WIDTH);
        assert_eq!(layout.status_bar.x, SIDEBAR_WIDTH);
    }
}
