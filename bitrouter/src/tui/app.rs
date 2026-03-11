use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::Stdout;

use crate::tui::TuiConfig;
use crate::tui::event::EventHandler;
use crate::tui::ui;

pub struct App {
    pub running: bool,
    pub config: TuiConfig,
}

impl App {
    pub fn new(config: TuiConfig) -> Self {
        Self {
            running: true,
            config,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.running = false,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.running = false;
            }
            _ => {}
        }
    }
}

pub async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut app: App,
) -> anyhow::Result<()> {
    let mut events = EventHandler::new();

    while app.running {
        terminal.draw(|frame| ui::render(frame, &app))?;

        if let Some(event) = events.next().await {
            match event {
                Event::Key(key) => app.handle_key(key),
                Event::Resize(_, _) => {} // redraw handled by loop
                _ => {}
            }
        }
    }

    Ok(())
}
