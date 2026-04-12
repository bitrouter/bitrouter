mod agent_events;
mod agent_lifecycle;
mod helpers;
mod input_ops;
mod key_handlers;
mod modals;
mod mouse;
mod search;
mod streaming;
mod tabs;

use std::collections::HashMap;
use std::io::Stdout;
use std::sync::Arc;

use bitrouter_providers::acp::discovery::discover_agents;
use bitrouter_providers::acp::provider::AcpAgentProvider;
use bitrouter_providers::acp::types::AgentAvailability;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use crate::TuiConfig;
use crate::error::TuiError;
use crate::event::{AppEvent, EventHandler};
use crate::model::{
    AgentStatus, AutocompleteState, InlineInput, InputTarget, Modal, ObsLog, ScrollbackState,
    SearchState, Tab, agent_color,
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
    /// Tab mode: switching/managing tabs.
    Tab,
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
    /// Tabs: one per active agent session.
    pub tabs: Vec<Tab>,
    /// Index of the currently focused tab.
    pub active_tab: usize,
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
    /// Cached layout from the last render pass (for mouse hit-testing).
    pub last_layout: Option<crate::ui::layout::AppLayout>,
}

impl AppState {
    /// Get the active tab's scrollback, if any tab exists.
    pub fn active_scrollback(&self) -> Option<&ScrollbackState> {
        self.tabs.get(self.active_tab).map(|t| &t.scrollback)
    }

    /// Get the active tab's scrollback mutably, if any tab exists.
    pub fn active_scrollback_mut(&mut self) -> Option<&mut ScrollbackState> {
        self.tabs
            .get_mut(self.active_tab)
            .map(|t| &mut t.scrollback)
    }

    /// Get the active tab's agent name, if any tab exists.
    pub fn active_agent_name(&self) -> Option<&str> {
        self.tabs
            .get(self.active_tab)
            .map(|t| t.agent_name.as_str())
    }
}

pub struct App {
    pub running: bool,
    pub state: AppState,
    /// Active agent providers, keyed by agent name.
    agent_providers: HashMap<String, Arc<AcpAgentProvider>>,
    /// Cloned event sender for spawning agent connections.
    event_tx: mpsc::Sender<AppEvent>,
    /// Routing context for resolving agent env vars.
    routing_ctx: bitrouter_config::RoutingContext,
}

impl App {
    pub fn new(
        config: TuiConfig,
        bitrouter_config: &bitrouter_config::BitrouterConfig,
        event_tx: mpsc::Sender<AppEvent>,
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
                    session_id: None,
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
                        routing: known_config.and_then(|c| c.routing.clone()),
                    }),
                    status,
                    session_id: None,
                    color: agent_color(idx),
                });
            }
        }

        // Build routing context for agent env var injection.
        let provider_keys = bitrouter_config::extract_provider_keys(&bitrouter_config.providers);
        let listen_str = bitrouter_config.server.listen.to_string();
        let routing_ctx = bitrouter_config::RoutingContext::new(&listen_str, &provider_keys);

        Self {
            running: true,
            state: AppState {
                mode: InputMode::Normal,
                agents,
                tabs: Vec::new(),
                active_tab: 0,
                input: InlineInput::new(),
                input_target: InputTarget::Default,
                autocomplete: None,
                modal: None,
                obs_log: ObsLog::new(),
                config,
                agent_list_selected: 0,
                search: None,
                last_layout: None,
            },
            agent_providers: HashMap::new(),
            event_tx,
            routing_ctx,
        }
    }

    fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Key(key) => self.handle_key(key),
            AppEvent::Mouse(mouse_event) => self.handle_mouse(mouse_event),
            AppEvent::Resize { .. } | AppEvent::Tick => {}
            AppEvent::Agent(agent_id, agent_event) => {
                self.handle_agent_event(agent_id, agent_event);
            }
            AppEvent::AgentConnected {
                agent_id,
                session_id,
            } => {
                self.handle_agent_connected(agent_id, session_id);
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
        }
    }
}

pub async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    config: TuiConfig,
    bitrouter_config: &bitrouter_config::BitrouterConfig,
) -> Result<(), TuiError> {
    let mut events = EventHandler::new();
    let mut app = App::new(config, bitrouter_config, events.sender());

    while app.running {
        terminal.draw(|frame| ui::render(frame, &mut app.state))?;

        match events.next().await {
            Some(event) => app.handle_event(event),
            None => break,
        }
    }

    // Shutdown: drop all providers so agent threads exit cleanly.
    app.agent_providers.clear();

    Ok(())
}
