mod agent_events;
mod agent_lifecycle;
mod helpers;
mod input_ops;
mod key_handlers;
mod modals;
mod mouse;
mod search;
pub(crate) mod session_store;
mod session_system;
mod sessions;
mod slash;
mod streaming;

use session_store::SessionStore;
use session_system::SessionSystem;

use std::io::Stdout;
use std::path::PathBuf;

use bitrouter_providers::acp::discovery::discover_agents;
use bitrouter_providers::acp::types::AgentAvailability;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use crate::TuiConfig;
use crate::error::TuiError;
use crate::event::{AppEvent, EventHandler};
use crate::model::{
    AgentStatus, AutocompleteState, InlineInput, InputTarget, Modal, ObsLog, ScrollbackState,
    SearchState, SessionSearchState, agent_color,
};
use crate::ui;

// ── Input mode (Zellij-style) ──────────────────────────────────────────

/// Which mode the TUI is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputMode {
    /// Normal mode: inline prompt has focus, scrollback auto-follows.
    Normal,
    /// Scroll mode: user is browsing scrollback history.
    Scroll,
    /// Session mode: switching/managing sessions in the sidebar.
    /// Renamed from `Tab` in PR 6 — the mode's purpose visibly broadens
    /// once `/` enters [`Self::SessionSearch`].
    Session,
    /// Sidebar incremental filter — typing builds a query, j/k navigate
    /// the filtered list, Enter selects, Esc cancels back to `Session`.
    SessionSearch,
    /// Agent mode: inline agent list for connect/disconnect.
    Agent,
    /// Search mode: incremental scrollback search.
    Search,
    /// Permission mode: awaiting y/n/a for a permission request.
    Permission,
}

/// All mutable TUI state, separated from `App` so the borrow checker allows
/// passing `&mut state` into the draw closure while checking `app.running`.
pub struct AppState {
    pub mode: InputMode,
    /// Agent registry: all known/discovered agents (not necessarily connected).
    pub agents: Vec<crate::model::Agent>,
    /// All sessions (active + id allocator). Callers usually touch
    /// `session_store.active` directly for iteration/indexed access.
    pub session_store: SessionStore,
    /// Index into `session_store.active` of the currently focused session.
    pub active_session: usize,
    /// Whether the threads sidebar is drawn.
    pub sidebar_visible: bool,
    /// Global input bar.
    pub input: InlineInput,
    pub input_target: InputTarget,
    pub autocomplete: Option<AutocompleteState>,
    /// Modal overlays (Help, Observability, CommandPalette).
    pub modal: Option<Modal>,
    pub obs_log: ObsLog,
    pub config: TuiConfig,
    /// Cursor position in Agent mode's inline list.
    pub agent_list_selected: usize,
    /// Incremental search state.
    pub search: Option<SearchState>,
    /// Sidebar incremental filter — `Some` while in
    /// [`InputMode::SessionSearch`], `None` otherwise. The sidebar reads
    /// this to render only matching sessions.
    pub session_search: Option<SessionSearchState>,
    /// Position in [`SessionStore::focus_history`] during an active
    /// `Ctrl-Tab` cycle. `None` when not cycling. Reset by any non-cycle
    /// key, at which point the current session is recorded as
    /// most-recent so the next cycle starts fresh.
    pub cycle_pos: Option<usize>,
    /// Cached layout from the last render pass (for mouse hit-testing).
    pub last_layout: Option<crate::ui::layout::AppLayout>,
}

impl AppState {
    /// Get the active session's scrollback, if any session exists.
    pub fn active_scrollback(&self) -> Option<&ScrollbackState> {
        self.session_store
            .active
            .get(self.active_session)
            .map(|s| &s.scrollback)
    }

    /// Get the active session's scrollback mutably, if any session exists.
    pub fn active_scrollback_mut(&mut self) -> Option<&mut ScrollbackState> {
        self.session_store
            .active
            .get_mut(self.active_session)
            .map(|s| &mut s.scrollback)
    }

    /// Get the active session's agent id, if any session exists.
    pub fn active_agent_name(&self) -> Option<&str> {
        self.session_store
            .active
            .get(self.active_session)
            .map(|s| s.agent_id.as_str())
    }
}

pub struct App {
    pub running: bool,
    pub state: AppState,
    /// ACP session lifecycle: owns providers, launch cwd, and the four
    /// spawn sites previously scattered across `agent_lifecycle.rs`.
    pub(super) session_system: SessionSystem,
    /// Cloned event sender for non-session work (install, misc background tasks).
    event_tx: mpsc::Sender<AppEvent>,
    /// Snapshot of the BitRouter config at TUI startup, used by slash
    /// commands that need provider/registry metadata.
    bitrouter_config: bitrouter_config::BitrouterConfig,
}

impl App {
    pub fn new(
        config: TuiConfig,
        bitrouter_config: &bitrouter_config::BitrouterConfig,
        event_tx: mpsc::Sender<AppEvent>,
        launch_cwd: PathBuf,
    ) -> Self {
        // Discover all agents (on PATH + distributable).
        let discovered = discover_agents(&bitrouter_config.agents);

        // Build agent list from config, using discovery to determine status.
        let mut agents: Vec<crate::model::Agent> = bitrouter_config
            .agents
            .iter()
            .filter(|(_, ac)| ac.enabled)
            .enumerate()
            .map(|(i, (name, ac))| {
                let status = discovered
                    .iter()
                    .find(|da| da.name == *name)
                    .map(|da| match &da.availability {
                        AgentAvailability::OnPath(_) => AgentStatus::Idle,
                        AgentAvailability::Distributable => AgentStatus::Available,
                    })
                    // Not discovered at all = no binary, no distribution.
                    .unwrap_or_else(|| {
                        if ac.distribution.is_empty() {
                            AgentStatus::Idle // Legacy: assume user knows what they configured
                        } else {
                            AgentStatus::Available
                        }
                    });
                crate::model::Agent {
                    name: name.clone(),
                    config: Some(ac.clone()),
                    status,
                    color: agent_color(i),
                }
            })
            .collect();

        // Add any discovered agents not already in config.
        for da in &discovered {
            if !agents.iter().any(|a| a.name == da.name) {
                let idx = agents.len();
                let status = match &da.availability {
                    AgentAvailability::OnPath(_) => AgentStatus::Idle,
                    AgentAvailability::Distributable => AgentStatus::Available,
                };
                let known_config = bitrouter_config.agents.get(&da.name);
                let distribution = known_config
                    .map(|c| c.distribution.clone())
                    .unwrap_or_default();
                agents.push(crate::model::Agent {
                    name: da.name.clone(),
                    config: Some(bitrouter_config::AgentConfig {
                        protocol: bitrouter_config::AgentProtocol::Acp,
                        binary: da.binary.to_string_lossy().into_owned(),
                        args: da.args.clone(),
                        enabled: true,
                        distribution,
                        session: None,
                        a2a: None,
                    }),
                    status,
                    color: agent_color(idx),
                });
            }
        }

        Self {
            running: true,
            state: AppState {
                mode: InputMode::Normal,
                agents,
                session_store: SessionStore::new(),
                active_session: 0,
                sidebar_visible: true,
                input: InlineInput::new(),
                input_target: InputTarget::Default,
                autocomplete: None,
                modal: None,
                obs_log: ObsLog::new(),
                config,
                agent_list_selected: 0,
                search: None,
                session_search: None,
                cycle_pos: None,
                last_layout: None,
            },
            session_system: SessionSystem::new(event_tx.clone(), launch_cwd),
            event_tx,
            bitrouter_config: bitrouter_config.clone(),
        }
    }

    fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Key(key) => self.handle_key(key),
            AppEvent::Mouse(mouse_event) => self.handle_mouse(mouse_event),
            AppEvent::Resize { .. } | AppEvent::Tick => {}
            AppEvent::Session {
                session_id,
                agent_id,
                event,
            } => {
                self.handle_session_event(session_id, agent_id, event);
            }
            AppEvent::SessionConnected {
                session_id,
                agent_id,
                acp_session_id,
            } => {
                self.handle_session_connected(session_id, agent_id, acp_session_id);
            }
            AppEvent::InstallProgress { agent_id, percent } => {
                if let Some(agent) = self.state.agents.iter_mut().find(|a| a.name == agent_id) {
                    agent.status = AgentStatus::Installing { percent };
                }
            }
            AppEvent::InstallComplete {
                agent_id,
                binary_path,
            } => {
                self.handle_install_complete(&agent_id, binary_path);
            }
            AppEvent::InstallFailed { agent_id, message } => {
                if let Some(agent) = self.state.agents.iter_mut().find(|a| a.name == agent_id) {
                    agent.status = AgentStatus::Error(message.clone());
                }
                self.push_system_msg(&format!("[{agent_id}] Install failed: {message}"));
            }
            AppEvent::SystemMessage { text } => {
                self.push_system_msg(&text);
            }
        }
    }
}

pub async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    config: TuiConfig,
    bitrouter_config: &bitrouter_config::BitrouterConfig,
    launch_cwd: PathBuf,
) -> Result<(), TuiError> {
    let mut events = EventHandler::new();
    let mut app = App::new(config, bitrouter_config, events.sender(), launch_cwd);

    while app.running {
        terminal.draw(|frame| ui::render(frame, &mut app.state))?;

        match events.next().await {
            Some(event) => app.handle_event(event),
            None => break,
        }
    }

    // Shutdown: drop all providers so agent threads exit cleanly.
    app.session_system.shutdown();

    Ok(())
}
