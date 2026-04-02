mod feed;
mod input_bar;
pub mod layout;
mod modals;
mod status_bar;
mod top_bar;

use ratatui::Frame;

use crate::app::AppState;

/// Top-level render: computes the layout and delegates to each panel.
pub fn render(frame: &mut Frame, state: &mut AppState) {
    let layout = layout::AppLayout::compute(frame.area());

    top_bar::render(frame, state, layout.top_bar);
    feed::render(frame, state, layout.feed);
    input_bar::render(frame, state, layout.input_bar);
    status_bar::render(frame, state, layout.status_bar);

    // Modals render last (on top of everything).
    modals::render_modal(frame, state);
}
