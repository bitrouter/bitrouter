mod welcome;

use ratatui::Frame;

use crate::tui::app::App;

pub fn render(frame: &mut Frame, app: &App) {
    welcome::render(frame, app);
}
