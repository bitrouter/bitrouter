mod conversation;
mod input_bar;
mod layout;
mod logs;
mod sidebar;
mod status_bar;
mod tabs;

use ratatui::Frame;

use crate::app::{AppState, Tab};

/// Top-level render: computes the layout and delegates to each panel.
pub fn render(frame: &mut Frame, state: &mut AppState) {
    let layout = layout::AppLayout::compute(frame.area());

    sidebar::render(frame, state, layout.sidebar);
    tabs::render(frame, state, layout.tab_bar);

    match state.tab {
        Tab::Conversation => conversation::render(frame, state, layout.content),
        Tab::Logs => logs::render(frame, state, layout.content),
    }

    input_bar::render(frame, state, layout.input_bar);
    status_bar::render(frame, state, layout.status_bar);
}
