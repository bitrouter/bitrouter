mod command_palette;
mod help;
mod import;
mod observability;

use ratatui::Frame;

use crate::app::AppState;
use crate::model::Modal;

/// Render the active modal overlay, if any.
pub fn render_modal(frame: &mut Frame, state: &AppState) {
    let modal = match &state.modal {
        Some(m) => m,
        None => return,
    };

    match modal {
        Modal::Observability(s) => observability::render(frame, state, s),
        Modal::CommandPalette(s) => command_palette::render(frame, state, s),
        Modal::Help => help::render(frame),
        Modal::ImportThreads(s) => import::render(frame, s),
    }
}
