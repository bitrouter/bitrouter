pub mod layout;
mod modals;
mod scrollback;
mod sidebar;
mod status_bar;
mod top_bar;

use ratatui::Frame;

use crate::app::AppState;

/// Top-level render: computes the layout and delegates to each panel.
pub fn render(frame: &mut Frame, state: &mut AppState) {
    let layout = layout::AppLayout::compute(frame.area(), state.sidebar_visible);
    state.last_layout = Some(layout);

    if let Some(sidebar_rect) = layout.sidebar {
        sidebar::render(frame, state, sidebar_rect);
    }
    top_bar::render(frame, state, layout.top_bar);
    scrollback::render(frame, state, layout.scrollback);
    status_bar::render(frame, state, layout.status_bar);

    // Modals render last (on top of everything).
    modals::render_modal(frame, state);
}
