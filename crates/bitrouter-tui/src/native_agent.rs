//! Small terminal projection for BRO task events. The application supplies
//! strings and commands; this crate owns drawing and the shared editor.

use std::io::{self, IsTerminal};

use crossterm::event::Event;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::editor::{Edit, Editor};

#[derive(Default)]
pub struct NativeState {
    pub model: String,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub status: String,
    pub verification: String,
    pub lines: Vec<String>,
    pub live: Option<String>,
    pub pending_input_id: Option<String>,
    pub editor: Editor,
}

impl NativeState {
    pub fn push(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
        if self.lines.len() > 500 {
            self.lines.drain(..self.lines.len() - 500);
        }
    }

    pub fn edit(&mut self, event: &Event) -> Edit {
        match event {
            Event::Key(key) => self.editor.apply(*key),
            Event::Paste(text) => {
                self.editor.paste(text);
                Edit::Changed
            }
            _ => Edit::Ignored,
        }
    }
}

pub struct NativeView {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl NativeView {
    pub fn open() -> io::Result<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(io::Error::other(
                "bro code requires an interactive terminal",
            ));
        }
        crate::lifecycle::install_panic_restore();
        crate::lifecycle::enter_raw()?;
        if let Err(error) = crate::lifecycle::enter_alternate_screen() {
            crate::lifecycle::restore();
            return Err(error);
        }
        match Terminal::new(CrosstermBackend::new(io::stdout())) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                crate::lifecycle::restore();
                Err(error)
            }
        }
    }

    pub fn draw(&mut self, state: &NativeState) -> io::Result<()> {
        self.terminal.draw(|frame| {
            let area = frame.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(3),
                    Constraint::Length(4),
                ])
                .split(area);
            let task_label = state
                .thread_id
                .as_deref()
                .map(|id| id.get(..8).unwrap_or(id))
                .unwrap_or("new");
            let title = format!(
                "BRO  model: {}  thread: {}  status: {}  check: {}",
                if state.model.is_empty() {
                    "choose model"
                } else {
                    &state.model
                },
                task_label,
                state.status,
                state.verification
            );
            frame.render_widget(
                Paragraph::new("Ctrl-C cancel · Ctrl-D detach · Ctrl-Enter steer · Ctrl-R resume · y/n approval")
                    .block(Block::default().title(title).borders(Borders::ALL)),
                chunks[0],
            );
            let height = chunks[1].height.saturating_sub(2) as usize;
            let mut lines = state.lines.clone();
            if let Some(live) = &state.live {
                lines.push(live.clone());
            }
            let visible = lines
                .iter()
                .rev()
                .take(height)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            frame.render_widget(
                Paragraph::new(visible).wrap(Wrap { trim: false }).block(
                    Block::default()
                        .title("Task events (newest first)")
                        .borders(Borders::ALL),
                ),
                chunks[1],
            );
            let instruction = if state.model.is_empty() {
                "Enter a routed model ID"
            } else if state.pending_input_id.is_some() {
                "Press y to approve or n to deny"
            } else {
                "Enter a coding task"
            };
            frame.render_widget(
                Paragraph::new(state.editor.text())
                    .wrap(Wrap { trim: false })
                    .block(Block::default().title(instruction).borders(Borders::ALL)),
                chunks[2],
            );
        })?;
        Ok(())
    }
}

impl Drop for NativeView {
    fn drop(&mut self) {
        crate::lifecycle::restore();
    }
}
