mod app;
mod event;
mod ui;

use std::io::{self, stdout};
use std::net::SocketAddr;

use crossterm::{
    ExecutableCommand,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use app::App;

#[derive(Debug, Clone)]
pub struct TuiConfig {
    pub listen_addr: SocketAddr,
    pub providers: Vec<String>,
    pub route_count: usize,
    pub daemon_pid: Option<u32>,
}

pub async fn run(config: TuiConfig) -> anyhow::Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    // Run the app
    let result = app::run_loop(&mut terminal, App::new(config)).await;

    // Restore terminal (always, even on error)
    restore_terminal();

    result
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = io::stdout().execute(LeaveAlternateScreen);
}
