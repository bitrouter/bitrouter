use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Pre-computed layout rectangles for the TUI.
///
/// Layout (top → bottom):
/// 1. Top bar (1 row) — session tabs.
/// 2. Scrollback (fills) — message history, empty-state hints, the
///    cwd label / divider, and the inline input bar all flow inside
///    this region. The input floats with content: with no entries it
///    sits right under the welcome banner; as entries accumulate it
///    moves down; once the entries fill the area it stays at the
///    bottom while older entries scroll above. Floating popups
///    (autocomplete, picker) overlay this region.
/// 3. Status bar (1 row) — `/ commands · ? help` left, agent · model right.
#[derive(Clone, Copy)]
pub struct AppLayout {
    pub top_bar: Rect,
    pub scrollback: Rect,
    pub status_bar: Rect,
}

impl AppLayout {
    pub fn compute(area: Rect) -> Self {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // top bar (session tabs)
                Constraint::Min(0),    // scrollback (entries + inline input)
                Constraint::Length(1), // status bar
            ])
            .split(area);

        Self {
            top_bar: rows[0],
            scrollback: rows[1],
            status_bar: rows[2],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_row_layout() {
        let area = Rect::new(0, 0, 100, 30);
        let layout = AppLayout::compute(area);
        assert_eq!(layout.top_bar, Rect::new(0, 0, 100, 1));
        assert_eq!(layout.scrollback, Rect::new(0, 1, 100, 28));
        assert_eq!(layout.status_bar, Rect::new(0, 29, 100, 1));
    }
}
