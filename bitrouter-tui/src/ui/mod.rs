mod welcome;

use ratatui::Frame;

use crate::app::App;

pub fn render(frame: &mut Frame, app: &App) {
    welcome::render(frame, app);
}
