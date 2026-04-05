mod app;
mod config;
mod error;
mod event;
mod input;
mod model;
mod render;
mod ui;

use std::io::{self, stdout};

use crossterm::{
    ExecutableCommand,
    event::{DisableMouseCapture, EnableMouseCapture},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

pub use config::TuiConfig;
pub use error::TuiError;

/// Run the TUI. Blocks until the user quits.
pub async fn run(
    config: TuiConfig,
    bitrouter_config: &bitrouter_config::BitrouterConfig,
) -> Result<(), TuiError> {
    enable_raw_mode()?;

    // From this point on, restore_terminal must always run — even if the
    // remaining setup steps fail.
    let result = run_inner(config, bitrouter_config).await;
    restore_terminal();
    result
}

async fn run_inner(
    config: TuiConfig,
    bitrouter_config: &bitrouter_config::BitrouterConfig,
) -> Result<(), TuiError> {
    stdout().execute(EnterAlternateScreen)?;
    stdout().execute(EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    app::run_loop(&mut terminal, config, bitrouter_config).await
}

fn restore_terminal() {
    let _ = io::stdout().execute(DisableMouseCapture);
    let _ = disable_raw_mode();
    let _ = io::stdout().execute(LeaveAlternateScreen);
}
