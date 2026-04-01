use std::io::Stdout;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::ListState;
use ratatui_textarea::TextArea;

use crate::TuiConfig;
use crate::error::TuiError;
use crate::event::{AppEvent, EventHandler};
use crate::model::{self, ContentBlock, Message, Role, Session};
use crate::ui;

/// Which panel owns keyboard focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Conversation,
    Input,
}

/// Which tab is active in the main content area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Conversation,
    Logs,
}

impl Tab {
    pub fn next(self) -> Self {
        match self {
            Self::Conversation => Self::Logs,
            Self::Logs => Self::Conversation,
        }
    }
}

/// Agent sidebar state.
pub struct SidebarState {
    pub agents: Vec<model::Agent>,
    pub list_state: ListState,
}

/// Conversation panel state.
pub struct ConversationState {
    pub session: Option<Session>,
    pub scroll_offset: usize,
    /// Recomputed each render pass so scroll clamping is accurate.
    pub total_lines: usize,
}

/// Log panel state.
pub struct LogsState {
    pub lines: Vec<String>,
}

/// All mutable TUI state, separated from `App` so the borrow checker allows
/// passing `&mut state` into the draw closure while checking `app.running`.
pub struct AppState {
    pub focus: Focus,
    pub tab: Tab,
    pub sidebar: SidebarState,
    pub conversation: ConversationState,
    pub logs: LogsState,
    pub input: TextArea<'static>,
    pub config: TuiConfig,
}

pub struct App {
    pub running: bool,
    pub state: AppState,
}

impl App {
    pub fn new(config: TuiConfig) -> Self {
        let mut sidebar = SidebarState {
            agents: model::mock_agents(),
            list_state: ListState::default(),
        };
        sidebar.list_state.select(Some(0));

        let initial_session = model::mock_session("session-1", "claude-code");

        Self {
            running: true,
            state: AppState {
                focus: Focus::Sidebar,
                tab: Tab::Conversation,
                sidebar,
                conversation: ConversationState {
                    session: Some(initial_session),
                    scroll_offset: 0,
                    total_lines: 0,
                },
                logs: LogsState { lines: Vec::new() },
                input: TextArea::default(),
                config,
            },
        }
    }

    fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Key(key) => self.handle_key(key),
            AppEvent::Resize { .. } | AppEvent::Tick => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Global quit: Ctrl-C always exits.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.running = false;
            return;
        }

        // `q` quits unless we're typing in the input bar.
        if key.code == KeyCode::Char('q') && self.state.focus != Focus::Input {
            self.running = false;
            return;
        }

        // Backtab (Shift+Tab) switches tabs in main area.
        if key.code == KeyCode::BackTab {
            self.state.tab = self.state.tab.next();
            return;
        }

        // Tab cycles focus between panels.
        if key.code == KeyCode::Tab {
            self.state.focus = match self.state.focus {
                Focus::Sidebar => Focus::Conversation,
                Focus::Conversation => Focus::Input,
                Focus::Input => Focus::Sidebar,
            };
            return;
        }

        // Esc from input returns focus to sidebar.
        if key.code == KeyCode::Esc && self.state.focus == Focus::Input {
            self.state.focus = Focus::Sidebar;
            return;
        }

        // Dispatch to focused panel.
        match self.state.focus {
            Focus::Sidebar => self.handle_sidebar_key(key),
            Focus::Conversation => self.handle_conversation_key(key),
            Focus::Input => self.handle_input_key(key),
        }
    }

    fn handle_sidebar_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.state.sidebar.list_state.select_next();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.state.sidebar.list_state.select_previous();
            }
            _ => {}
        }
    }

    fn handle_conversation_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.state.conversation.scroll_offset =
                    self.state.conversation.scroll_offset.saturating_add(1);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.state.conversation.scroll_offset =
                    self.state.conversation.scroll_offset.saturating_sub(1);
            }
            _ => {}
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => self.submit_input(),
            _ => {
                self.state.input.input(key);
            }
        }
    }

    fn submit_input(&mut self) {
        let text: String = self.state.input.lines().join("\n");
        if text.trim().is_empty() {
            return;
        }
        let msg = Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text(text)],
        };
        if let Some(session) = self.state.conversation.session.as_mut() {
            session.messages.push(msg);
        }
        self.state.input = TextArea::default();
    }
}

pub async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut app: App,
) -> Result<(), TuiError> {
    let mut events = EventHandler::new();

    while app.running {
        terminal.draw(|frame| ui::render(frame, &mut app.state))?;

        match events.next().await {
            Some(event) => app.handle_event(event),
            None => break, // event channel closed (terminal detached)
        }
    }

    Ok(())
}
