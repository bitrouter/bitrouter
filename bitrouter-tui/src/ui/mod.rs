pub mod layout;
mod scrollback;
mod status_bar;
mod top_bar;

use ratatui::Frame;

use crate::app::AppState;

/// Top-level render: computes the layout and delegates to each region.
pub fn render(frame: &mut Frame, state: &mut AppState) {
    let layout = layout::AppLayout::compute(frame.area());

    top_bar::render(frame, state, layout.top_bar);
    scrollback::render(frame, state, layout.scrollback);
    status_bar::render(frame, state, layout.status_bar);
}
