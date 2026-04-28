mod app;
mod config;
mod error;
mod event;
mod input;
mod model;
mod render;
mod ui;

use std::io::{self, stdout};
use std::path::PathBuf;

use crossterm::{
    ExecutableCommand,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

pub use config::TuiConfig;
pub use error::TuiError;

/// Run the TUI. Blocks until the user quits.
///
/// `launch_cwd` is the absolute working directory that sessions spawned
/// from this TUI process will be rooted at. Import flows (PR 9+) may
/// override it per-session, but the default is the cwd the user ran
/// `bitrouter` from.
pub async fn run(
    config: TuiConfig,
    bitrouter_config: &bitrouter_config::BitrouterConfig,
    launch_cwd: PathBuf,
) -> Result<(), TuiError> {
    enable_raw_mode()?;

    // From this point on, restore_terminal must always run — even if the
    // remaining setup steps fail.
    let result = run_inner(config, bitrouter_config, launch_cwd).await;
    restore_terminal();
    result
}

async fn run_inner(
    config: TuiConfig,
    bitrouter_config: &bitrouter_config::BitrouterConfig,
    launch_cwd: PathBuf,
) -> Result<(), TuiError> {
    // Mouse capture is intentionally NOT enabled: it would intercept
    // click+drag and break the terminal's native text selection. The
    // TUI is fully keyboard-driven (see `specs/bitrouter-tui-product.md`
    // §6), matching the Codex TUI's approach.
    stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    app::run_loop(&mut terminal, config, bitrouter_config, launch_cwd).await
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = io::stdout().execute(LeaveAlternateScreen);
}
