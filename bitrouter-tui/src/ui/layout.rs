use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Pre-computed layout rectangles for the TUI.
pub struct AppLayout {
    pub sidebar: Rect,
    pub tab_bar: Rect,
    pub content: Rect,
    pub input_bar: Rect,
    pub status_bar: Rect,
}

impl AppLayout {
    /// Compute the layout from the terminal area.
    pub fn compute(area: Rect) -> Self {
        let outer = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(24), Constraint::Min(0)])
            .split(area);

        let sidebar = outer[0];
        let main_area = outer[1];

        let main_col = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // tab bar
                Constraint::Min(0),    // content
                Constraint::Length(3), // input bar
                Constraint::Length(1), // status bar
            ])
            .split(main_area);

        Self {
            sidebar,
            tab_bar: main_col[0],
            content: main_col[1],
            input_bar: main_col[2],
            status_bar: main_col[3],
        }
    }
}
