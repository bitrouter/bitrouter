mod acp;
mod app;
mod config;
mod error;
mod event;
mod model;
mod ui;

use std::io::{self, stdout};

use crossterm::{
    ExecutableCommand,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

pub use config::TuiConfig;
pub use error::TuiError;

/// Run the TUI. Blocks until the user quits.
pub async fn run(config: TuiConfig) -> Result<(), TuiError> {
    enable_raw_mode()?;

    // From this point on, restore_terminal must always run — even if the
    // remaining setup steps fail.
    let result = run_inner(config).await;
    restore_terminal();
    result
}

async fn run_inner(config: TuiConfig) -> Result<(), TuiError> {
    stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    app::run_loop(&mut terminal, config).await
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = io::stdout().execute(LeaveAlternateScreen);
}
