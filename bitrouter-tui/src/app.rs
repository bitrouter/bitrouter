use std::collections::HashMap;
use std::io::Stdout;
use std::time::Instant;

use bitrouter_providers::acp::discovery::discover_agents;
use bitrouter_providers::acp::provider::AcpAgentProvider;
use bitrouter_providers::acp::types::{
    AgentEvent, PermissionOutcome, PermissionResponse, ToolCallStatus,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use crate::TuiConfig;
use crate::error::TuiError;
use crate::event::{AppEvent, EventHandler};
use crate::input;
use crate::model::{
    ActivityEntry, AgentResponse, AgentStatus, AutocompleteState, CommandAction,
    CommandPaletteState, ContentBlock, EntryKind, InlineInput, InputTarget, Modal, ObsEvent,
    ObsEventKind, ObsLog, ObservabilityState, PaletteCommand, PermissionEntry, ScrollbackState,
    SearchState, SystemNotice, Tab, TabBadge, ThinkingEntry, ToolCallEntry, UserPrompt,
    agent_color,
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
    agent_providers: HashMap<String, AcpAgentProvider>,
    /// Cloned event sender for spawning agent connections.
    event_tx: mpsc::Sender<AppEvent>,
}

impl App {
    pub fn new(
        config: TuiConfig,
        bitrouter_config: &bitrouter_config::BitrouterConfig,
        event_tx: mpsc::Sender<AppEvent>,
    ) -> Self {
        // Load configured agents (source of truth).
        let mut agents: Vec<crate::model::Agent> = bitrouter_config
            .agents
            .iter()
            .filter(|(_, ac)| ac.enabled)
            .enumerate()
            .map(|(i, (name, ac))| crate::model::Agent {
                name: name.clone(),
                config: Some(ac.clone()),
                status: AgentStatus::Idle,
                session_id: None,
                color: agent_color(i),
            })
            .collect();

        // Run discovery for agents not yet in config.
        let known = bitrouter_config::builtin_agent_defs();
        let discovered = discover_agents(&known);
        for da in &discovered {
            if !agents.iter().any(|a| a.name == da.name) {
                let idx = agents.len();
                agents.push(crate::model::Agent {
                    name: da.name.clone(),
                    config: Some(bitrouter_config::AgentConfig {
                        protocol: bitrouter_config::AgentProtocol::Acp,
                        binary: da.binary.to_string_lossy().into_owned(),
                        args: da.args.clone(),
                        enabled: true,
                    }),
                    status: AgentStatus::Idle,
                    session_id: None,
                    color: agent_color(idx),
                });
            }
        }

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
            },
            agent_providers: HashMap::new(),
            event_tx,
        }
    }

    fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Key(key) => self.handle_key(key),
            AppEvent::Resize { .. } | AppEvent::Tick => {}
            AppEvent::Agent(agent_event) => self.handle_agent_event(agent_event),
        }
    }

    // ── Tab helpers ────────────────────────────────────────────────────

    /// Find the tab index for a given agent name.
    fn tab_for_agent(&self, agent_name: &str) -> Option<usize> {
        self.state
            .tabs
            .iter()
            .position(|t| t.agent_name == agent_name)
    }

    /// Get a mutable reference to an agent's tab scrollback.
    fn scrollback_for_agent(&mut self, agent_name: &str) -> Option<&mut ScrollbackState> {
        self.state
            .tabs
            .iter_mut()
            .find(|t| t.agent_name == agent_name)
            .map(|t| &mut t.scrollback)
    }

    /// Switch to a tab by index, clearing its badge and resetting search.
    fn switch_tab(&mut self, idx: usize) {
        if idx < self.state.tabs.len() {
            self.state.active_tab = idx;
            self.state.tabs[idx].badge = TabBadge::None;
            // Search state references entries from the old tab — invalidate it.
            if self.state.search.is_some() {
                self.state.search = None;
                if self.state.mode == InputMode::Search {
                    self.state.mode = InputMode::Normal;
                }
            }
        }
    }

    /// Create a tab for an agent if one doesn't already exist. Returns the tab index.
    fn ensure_tab(&mut self, agent_name: &str) -> usize {
        if let Some(idx) = self.tab_for_agent(agent_name) {
            return idx;
        }
        self.state.tabs.push(Tab {
            agent_name: agent_name.to_string(),
            scrollback: ScrollbackState::new(),
            badge: TabBadge::None,
        });
        self.state.tabs.len() - 1
    }

    /// Increment unread badge on a background tab.
    fn badge_background_tab(&mut self, agent_name: &str) {
        if let Some(idx) = self.tab_for_agent(agent_name)
            && idx != self.state.active_tab
        {
            let tab = &mut self.state.tabs[idx];
            tab.badge = match &tab.badge {
                TabBadge::None => TabBadge::Unread(1),
                TabBadge::Unread(n) => TabBadge::Unread(n + 1),
                TabBadge::Permission => TabBadge::Permission, // Don't downgrade
            };
        }
    }

    // ── Agent event dispatcher ─────────────────────────────────────

    fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Connected {
                agent_id,
                session_id,
            } => self.handle_agent_connected(agent_id, session_id),
            AgentEvent::Disconnected { agent_id } => self.handle_agent_disconnected(agent_id),
            AgentEvent::Error { agent_id, message } => {
                self.handle_agent_error(agent_id, message);
            }
            AgentEvent::MessageChunk { agent_id, text } => {
                self.apply_agent_message_chunk(&agent_id, text);
            }
            AgentEvent::NonTextContent {
                agent_id,
                description,
            } => {
                self.apply_non_text_content(&agent_id, description);
            }
            AgentEvent::ThoughtChunk { agent_id, text } => {
                self.apply_thought_chunk(&agent_id, text);
            }
            AgentEvent::ToolCall {
                agent_id,
                tool_call_id,
                title,
                status,
            } => {
                self.apply_tool_call(&agent_id, tool_call_id, title, status);
            }
            AgentEvent::ToolCallUpdate {
                agent_id,
                tool_call_id,
                title,
                status,
            } => {
                self.apply_tool_call_update(&agent_id, tool_call_id, title, status);
            }
            AgentEvent::PermissionRequest {
                agent_id,
                request,
                response_tx,
            } => {
                self.handle_permission_request(agent_id, request, response_tx);
            }
            AgentEvent::PromptDone { agent_id, .. } => {
                self.handle_prompt_done(agent_id);
            }
        }
    }

    // ── Agent lifecycle handlers ──────────────────────────────────────

    fn handle_agent_connected(&mut self, agent_id: String, session_id: String) {
        if let Some(agent) = self.state.agents.iter_mut().find(|a| a.name == agent_id) {
            agent.status = AgentStatus::Connected;
            agent.session_id = Some(session_id);
        }
        let tab_idx = self.ensure_tab(&agent_id);
        self.push_system_msg_to_tab(tab_idx, &format!("Connected to {agent_id}"));
        self.state.obs_log.push(ObsEvent {
            agent_id,
            kind: ObsEventKind::Connected,
            timestamp: Instant::now(),
        });
    }

    fn handle_agent_disconnected(&mut self, agent_id: String) {
        // Clean up provider handle.
        self.agent_providers.remove(&agent_id);
        if let Some(agent) = self.state.agents.iter_mut().find(|a| a.name == agent_id) {
            // Only reset to Idle if not already in Error state.
            if !matches!(agent.status, AgentStatus::Error(_)) {
                agent.status = AgentStatus::Idle;
            }
            agent.session_id = None;
        }
        // Clear streaming cursor for this agent.
        if let Some(sb) = self.scrollback_for_agent(&agent_id) {
            sb.streaming_entry.remove(&agent_id);
        }

        if let Some(tab_idx) = self.tab_for_agent(&agent_id) {
            self.push_system_msg_to_tab(tab_idx, &format!("Disconnected from {agent_id}"));
        }
        self.state.obs_log.push(ObsEvent {
            agent_id,
            kind: ObsEventKind::Disconnected,
            timestamp: Instant::now(),
        });
    }

    fn handle_agent_error(&mut self, agent_id: String, message: String) {
        if let Some(agent) = self.state.agents.iter_mut().find(|a| a.name == agent_id) {
            agent.status = AgentStatus::Error(message.clone());
        }
        if let Some(sb) = self.scrollback_for_agent(&agent_id) {
            sb.streaming_entry.remove(&agent_id);
        }
        let tab_idx = self.ensure_tab(&agent_id);
        self.push_system_msg_to_tab(tab_idx, &format!("[{agent_id}] Error: {message}"));
        self.state.obs_log.push(ObsEvent {
            agent_id,
            kind: ObsEventKind::Error { message },
            timestamp: Instant::now(),
        });
    }

    // ── Agent content handlers ────────────────────────────────────────

    fn apply_agent_message_chunk(&mut self, agent_id: &str, text: String) {
        self.badge_background_tab(agent_id);
        let tab_idx = self.ensure_tab(agent_id);
        let sb = &mut self.state.tabs[tab_idx].scrollback;

        // Try to extend existing streaming entry for this agent.
        if let Some(&entry_id) = sb.streaming_entry.get(agent_id)
            && let Some(idx) = sb.index_of(entry_id)
            && let EntryKind::AgentResponse(resp) = &mut sb.entries[idx].kind
        {
            // Extend last text block or push new one.
            if let Some(ContentBlock::Text(existing)) = resp.blocks.last_mut() {
                existing.push_str(&text);
            } else {
                resp.blocks.push(ContentBlock::Text(text));
            }
            return;
        }

        // Finalize any previous streaming entry before starting new.
        Self::finalize_streaming_in(sb, agent_id);

        // Start a new agent message entry.
        let id = sb.next_id();
        sb.entries.push(ActivityEntry {
            id,
            kind: EntryKind::AgentResponse(AgentResponse {
                agent_id: agent_id.to_string(),
                blocks: vec![ContentBlock::Text(text)],
                is_streaming: true,
            }),
            collapsed: false,
        });
        sb.streaming_entry.insert(agent_id.to_string(), id);
    }

    fn apply_non_text_content(&mut self, agent_id: &str, desc: String) {
        self.badge_background_tab(agent_id);
        let tab_idx = self.ensure_tab(agent_id);
        let sb = &mut self.state.tabs[tab_idx].scrollback;

        // Append as an Other block to the current streaming entry, or create new.
        if let Some(&entry_id) = sb.streaming_entry.get(agent_id)
            && let Some(idx) = sb.index_of(entry_id)
            && let EntryKind::AgentResponse(resp) = &mut sb.entries[idx].kind
        {
            resp.blocks.push(ContentBlock::Other(desc));
            return;
        }

        let id = sb.next_id();
        sb.entries.push(ActivityEntry {
            id,
            kind: EntryKind::AgentResponse(AgentResponse {
                agent_id: agent_id.to_string(),
                blocks: vec![ContentBlock::Other(desc)],
                is_streaming: true,
            }),
            collapsed: false,
        });
        sb.streaming_entry.insert(agent_id.to_string(), id);
    }

    fn apply_thought_chunk(&mut self, agent_id: &str, text: String) {
        self.badge_background_tab(agent_id);
        let tab_idx = self.ensure_tab(agent_id);
        let sb = &mut self.state.tabs[tab_idx].scrollback;

        // Try to extend existing streaming thinking entry.
        if let Some(&entry_id) = sb.streaming_entry.get(agent_id)
            && let Some(idx) = sb.index_of(entry_id)
            && let EntryKind::Thinking(th) = &mut sb.entries[idx].kind
            && th.is_streaming
        {
            th.text.push_str(&text);
            return;
        }

        // Finalize any previous streaming entry before starting new.
        Self::finalize_streaming_in(sb, agent_id);

        let id = sb.next_id();
        sb.entries.push(ActivityEntry {
            id,
            kind: EntryKind::Thinking(ThinkingEntry {
                agent_id: agent_id.to_string(),
                text,
                is_streaming: true,
            }),
            collapsed: false,
        });
        sb.streaming_entry.insert(agent_id.to_string(), id);
    }

    fn apply_tool_call(
        &mut self,
        agent_id: &str,
        tool_call_id: String,
        title: String,
        status: ToolCallStatus,
    ) {
        self.badge_background_tab(agent_id);
        let tab_idx = self.ensure_tab(agent_id);
        let sb = &mut self.state.tabs[tab_idx].scrollback;

        let id = sb.next_id();
        sb.entries.push(ActivityEntry {
            id,
            kind: EntryKind::ToolCall(ToolCallEntry {
                agent_id: agent_id.to_string(),
                tool_call_id,
                title: title.clone(),
                status,
            }),
            collapsed: false,
        });
        // Tool calls break the streaming cursor — next message chunk starts fresh.
        sb.streaming_entry.remove(agent_id);

        self.state.obs_log.push(ObsEvent {
            agent_id: agent_id.to_string(),
            kind: ObsEventKind::ToolCall { title },
            timestamp: Instant::now(),
        });
    }

    fn apply_tool_call_update(
        &mut self,
        agent_id: &str,
        tool_call_id: String,
        new_title: Option<String>,
        new_status: Option<ToolCallStatus>,
    ) {
        let tab_idx = self.ensure_tab(agent_id);
        let sb = &mut self.state.tabs[tab_idx].scrollback;

        // Find the tool call entry by ID and update it.
        for entry in sb.entries.iter_mut().rev() {
            if let EntryKind::ToolCall(tc) = &mut entry.kind
                && tc.agent_id == agent_id
                && tc.tool_call_id == tool_call_id
            {
                if let Some(t) = &new_title {
                    tc.title = t.clone();
                }
                if let Some(s) = new_status {
                    tc.status = s;
                    // Auto-collapse completed/failed tool calls.
                    if matches!(s, ToolCallStatus::Completed | ToolCallStatus::Failed) {
                        entry.collapsed = true;
                    }
                }
                return;
            }
        }

        // If not found, create from update (fallback).
        // Need to drop the borrow first, then re-call through self.
        self.apply_tool_call(
            agent_id,
            tool_call_id,
            new_title.unwrap_or_default(),
            new_status.unwrap_or(ToolCallStatus::InProgress),
        );
    }

    fn handle_permission_request(
        &mut self,
        agent_id: String,
        request: bitrouter_providers::acp::types::PermissionRequest,
        response_tx: tokio::sync::oneshot::Sender<PermissionResponse>,
    ) {
        let tab_idx = self.ensure_tab(&agent_id);
        let sb = &mut self.state.tabs[tab_idx].scrollback;

        let id = sb.next_id();
        sb.entries.push(ActivityEntry {
            id,
            kind: EntryKind::Permission(PermissionEntry {
                agent_id: agent_id.clone(),
                request: Box::new(request),
                response_tx: Some(response_tx),
                resolved: false,
            }),
            collapsed: false,
        });
        // Re-pin to bottom so user sees the permission prompt.
        sb.follow = true;

        // Auto-switch only if we're not already resolving a permission on another tab.
        if self.state.mode == InputMode::Permission {
            // Already handling a permission — just badge this tab, don't switch.
            if tab_idx != self.state.active_tab {
                self.state.tabs[tab_idx].badge = TabBadge::Permission;
            }
        } else {
            if tab_idx != self.state.active_tab {
                self.state.tabs[tab_idx].badge = TabBadge::Permission;
                self.switch_tab(tab_idx);
            }
            self.state.mode = InputMode::Permission;
        }
    }

    fn handle_prompt_done(&mut self, agent_id: String) {
        if let Some(sb) = self.scrollback_for_agent(&agent_id) {
            // Mark the streaming entry as complete.
            if let Some(entry_id) = sb.streaming_entry.remove(&agent_id)
                && let Some(idx) = sb.index_of(entry_id)
            {
                match &mut sb.entries[idx].kind {
                    EntryKind::AgentResponse(resp) => resp.is_streaming = false,
                    EntryKind::Thinking(th) => th.is_streaming = false,
                    _ => {}
                }
            }
        }
        // Update agent status.
        if let Some(agent) = self.state.agents.iter_mut().find(|a| a.name == agent_id)
            && matches!(agent.status, AgentStatus::Busy)
        {
            agent.status = AgentStatus::Connected;
        }
        self.state.obs_log.push(ObsEvent {
            agent_id,
            kind: ObsEventKind::PromptDone,
            timestamp: Instant::now(),
        });
    }

    // ── Key handlers ────────────────────────────────────────────────

    fn handle_key(&mut self, key: KeyEvent) {
        // Global: Ctrl-C always exits.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.running = false;
            return;
        }

        // If a modal is open, route all keys to modal handler.
        if self.state.modal.is_some() {
            self.handle_modal_key(key);
            return;
        }

        // Global shortcuts (work in any mode except Permission).
        if self.state.mode != InputMode::Permission && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            match key.code {
                KeyCode::Char('p') => {
                    self.open_command_palette();
                    return;
                }
                KeyCode::Char('o') => {
                    self.open_observability();
                    return;
                }
                _ => {}
            }
        }

        // Alt+1..Alt+9: direct tab switch from any mode (except Permission).
        if self.state.mode != InputMode::Permission
            && key.modifiers.contains(KeyModifiers::ALT)
            && let KeyCode::Char(c @ '1'..='9') = key.code
        {
            let idx = (c as usize) - ('1' as usize);
            self.switch_tab(idx);
            if self.state.mode == InputMode::Tab {
                self.state.mode = InputMode::Normal;
            }
            return;
        }

        // '?' opens help (only in non-typing modes).
        if key.code == KeyCode::Char('?')
            && !matches!(
                self.state.mode,
                InputMode::Normal | InputMode::Search | InputMode::Permission
            )
        {
            self.state.modal = Some(Modal::Help);
            return;
        }

        // Dispatch to current mode.
        match self.state.mode {
            InputMode::Normal => self.handle_normal_key(key),
            InputMode::Scroll => self.handle_scroll_key(key),
            InputMode::Tab => self.handle_tab_mode_key(key),
            InputMode::Agent => self.handle_agent_mode_key(key),
            InputMode::Search => self.handle_search_mode_key(key),
            InputMode::Permission => self.handle_permission_key(key),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        // Alt+T enters Tab mode.
        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('t') {
            self.state.mode = InputMode::Tab;
            return;
        }
        // Alt+A enters Agent mode.
        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('a') {
            self.state.mode = InputMode::Agent;
            return;
        }

        match key.code {
            KeyCode::Enter => {
                // Check for autocomplete first.
                if self.state.autocomplete.is_some() {
                    self.accept_autocomplete();
                    return;
                }
                // Shift+Enter or Alt+Enter inserts a newline.
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT)
                {
                    self.state.input.newline();
                    return;
                }
                self.submit_input();
            }
            KeyCode::Tab => {
                if self.state.autocomplete.is_some() {
                    self.accept_autocomplete();
                }
            }
            KeyCode::Esc => {
                if self.state.autocomplete.is_some() {
                    self.close_autocomplete();
                } else {
                    // Enter scroll mode on the active tab.
                    if let Some(sb) = self.state.active_scrollback_mut() {
                        sb.follow = false;
                    }
                    self.state.mode = InputMode::Scroll;
                }
            }
            KeyCode::Backspace => {
                self.state.input.backspace();
                self.after_input_char();
            }
            KeyCode::Delete => {
                self.state.input.delete_char();
                self.after_input_char();
            }
            KeyCode::Left => self.state.input.move_left(),
            KeyCode::Right => self.state.input.move_right(),
            KeyCode::Up => self.state.input.move_up(),
            KeyCode::Down => self.state.input.move_down(),
            KeyCode::Home => self.state.input.home(),
            KeyCode::End => self.state.input.end(),
            KeyCode::Char(c) => {
                self.state.input.insert_char(c);
                self.after_input_char();
            }
            _ => {}
        }
    }

    fn handle_scroll_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    sb.scroll_offset = sb.scroll_offset.saturating_add(1);
                    sb.follow = false;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    sb.scroll_offset = sb.scroll_offset.saturating_sub(1);
                    sb.follow = false;
                }
            }
            KeyCode::PageDown => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    sb.scroll_offset = sb.scroll_offset.saturating_add(20);
                    sb.follow = false;
                }
            }
            KeyCode::PageUp => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    sb.scroll_offset = sb.scroll_offset.saturating_sub(20);
                    sb.follow = false;
                }
            }
            KeyCode::Char('G') => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    sb.follow = true;
                }
                self.state.mode = InputMode::Normal;
            }
            KeyCode::Char('i') => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    sb.follow = true;
                }
                self.state.mode = InputMode::Normal;
            }
            KeyCode::Char('/') => {
                self.state.search = Some(SearchState {
                    query: String::new(),
                    matches: Vec::new(),
                    current_match: 0,
                });
                self.state.mode = InputMode::Search;
            }
            KeyCode::Esc => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    sb.follow = true;
                }
                self.state.mode = InputMode::Normal;
            }
            _ => {
                // Any printable char returns to Normal mode.
                if let KeyCode::Char(c) = key.code {
                    if let Some(sb) = self.state.active_scrollback_mut() {
                        sb.follow = true;
                    }
                    self.state.mode = InputMode::Normal;
                    self.state.input.insert_char(c);
                    self.after_input_char();
                }
            }
        }
    }

    fn handle_tab_mode_key(&mut self, key: KeyEvent) {
        let tab_count = self.state.tabs.len();
        match key.code {
            KeyCode::Char('h') | KeyCode::Left => {
                if tab_count > 0 && self.state.active_tab > 0 {
                    let idx = self.state.active_tab - 1;
                    self.switch_tab(idx);
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                if tab_count > 0 && self.state.active_tab + 1 < tab_count {
                    let idx = self.state.active_tab + 1;
                    self.switch_tab(idx);
                }
            }
            KeyCode::Char(c @ '1'..='9') => {
                let idx = (c as usize) - ('1' as usize);
                self.switch_tab(idx);
                self.state.mode = InputMode::Normal;
            }
            KeyCode::Char('n') => {
                // New tab → enter Agent mode to pick agent.
                self.state.mode = InputMode::Agent;
            }
            KeyCode::Char('x') => {
                if tab_count > 0 {
                    self.close_current_tab();
                }
                self.state.mode = InputMode::Normal;
            }
            KeyCode::Esc => {
                self.state.mode = InputMode::Normal;
            }
            _ => {}
        }
    }

    fn handle_agent_mode_key(&mut self, key: KeyEvent) {
        let agent_count = self.state.agents.len();
        if agent_count == 0 {
            if key.code == KeyCode::Esc {
                self.state.mode = InputMode::Normal;
            }
            return;
        }

        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.state.agent_list_selected = (self.state.agent_list_selected + 1) % agent_count;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.state.agent_list_selected > 0 {
                    self.state.agent_list_selected -= 1;
                } else {
                    self.state.agent_list_selected = agent_count - 1;
                }
            }
            KeyCode::Enter | KeyCode::Char('c') => {
                let selected = self.state.agent_list_selected;
                if let Some(agent) = self.state.agents.get(selected) {
                    let name = agent.name.clone();
                    if !self.agent_providers.contains_key(&name) {
                        self.connect_agent(&name);
                    }
                    // Switch to the agent's tab.
                    let tab_idx = self.ensure_tab(&name);
                    self.switch_tab(tab_idx);
                    self.state.mode = InputMode::Normal;
                }
            }
            KeyCode::Char('d') => {
                let selected = self.state.agent_list_selected;
                if let Some(agent) = self.state.agents.get(selected) {
                    let name = agent.name.clone();
                    self.disconnect_agent(&name);
                }
            }
            KeyCode::Char('r') => {
                self.rediscover_agents();
            }
            KeyCode::Esc => {
                self.state.mode = InputMode::Normal;
            }
            _ => {}
        }
    }

    fn handle_search_mode_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                // Jump to next match.
                if let Some(search) = &mut self.state.search
                    && !search.matches.is_empty()
                {
                    search.current_match = (search.current_match + 1) % search.matches.len();
                }
                self.scroll_to_search_match();
            }
            KeyCode::Backspace => {
                if let Some(search) = &mut self.state.search {
                    search.query.pop();
                }
                self.recompute_search();
            }
            KeyCode::Char(c) => {
                if let Some(search) = &mut self.state.search {
                    search.query.push(c);
                }
                self.recompute_search();
            }
            KeyCode::Esc => {
                self.state.search = None;
                self.state.mode = InputMode::Scroll;
            }
            _ => {}
        }
    }

    fn recompute_search(&mut self) {
        let query = match &self.state.search {
            Some(s) if !s.query.is_empty() => s.query.to_lowercase(),
            _ => {
                if let Some(search) = &mut self.state.search {
                    search.matches.clear();
                    search.current_match = 0;
                }
                return;
            }
        };

        let matches: Vec<u64> = if let Some(sb) = self.state.active_scrollback() {
            sb.entries
                .iter()
                .filter(|e| entry_contains_text(&e.kind, &query))
                .map(|e| e.id)
                .collect()
        } else {
            Vec::new()
        };

        if let Some(search) = &mut self.state.search {
            search.matches = matches;
            search.current_match = 0;
        }
    }

    fn scroll_to_search_match(&mut self) {
        let target_id = match &self.state.search {
            Some(s) => s.matches.get(s.current_match).copied(),
            None => None,
        };
        let Some(target_id) = target_id else { return };

        if let Some(sb) = self.state.active_scrollback_mut() {
            // Find approximate line position of the entry.
            let mut line_pos = 0usize;
            for entry in &sb.entries {
                if entry.id == target_id {
                    sb.scroll_offset = line_pos.saturating_sub(3);
                    sb.follow = false;
                    break;
                }
                // Rough estimate: each entry ~3 lines.
                line_pos += 3;
            }
        }
    }

    // ── Permission handling ─────────────────────────────────────────

    fn handle_permission_key(&mut self, key: KeyEvent) {
        // Find the unresolved permission entry in the active tab.
        let perm_idx = self.state.active_scrollback().and_then(|sb| {
            sb.entries
                .iter()
                .position(|e| matches!(&e.kind, EntryKind::Permission(p) if !p.resolved))
        });

        let Some(perm_idx) = perm_idx else {
            // No pending permission in active tab — return to Normal.
            self.state.mode = InputMode::Normal;
            return;
        };

        match key.code {
            KeyCode::Char('y') => self.resolve_permission(perm_idx, PermissionChoice::Yes),
            KeyCode::Char('n') => self.resolve_permission(perm_idx, PermissionChoice::No),
            KeyCode::Char('a') => self.resolve_permission(perm_idx, PermissionChoice::Always),
            _ => {} // Ignore all other keys during permission.
        }
    }

    fn resolve_permission(&mut self, entry_idx: usize, choice: PermissionChoice) {
        let sb = match self.state.active_scrollback_mut() {
            Some(sb) => sb,
            None => return,
        };

        if let EntryKind::Permission(perm) = &mut sb.entries[entry_idx].kind {
            let outcome = match choice {
                PermissionChoice::Yes => {
                    if let Some(opt) = perm.request.options.first() {
                        PermissionOutcome::Allowed {
                            selected_option: opt.id.clone(),
                        }
                    } else {
                        PermissionOutcome::Denied
                    }
                }
                PermissionChoice::Always => {
                    // Pick the "always" option if it exists, else first option.
                    let always_opt = perm
                        .request
                        .options
                        .iter()
                        .find(|o| o.id.to_lowercase().contains("always"));
                    if let Some(opt) = always_opt.or(perm.request.options.first()) {
                        PermissionOutcome::Allowed {
                            selected_option: opt.id.clone(),
                        }
                    } else {
                        PermissionOutcome::Denied
                    }
                }
                PermissionChoice::No => PermissionOutcome::Denied,
            };

            if let Some(tx) = perm.response_tx.take() {
                let _ = tx.send(PermissionResponse { outcome });
            }
            perm.resolved = true;
        }

        // Check if any other tab has a pending permission — auto-switch to it.
        let next_perm_tab = self.state.tabs.iter().enumerate().find(|(_, tab)| {
            tab.scrollback
                .entries
                .iter()
                .any(|e| matches!(&e.kind, EntryKind::Permission(p) if !p.resolved))
        });
        if let Some((idx, _)) = next_perm_tab {
            self.switch_tab(idx);
            self.state.mode = InputMode::Permission;
        } else {
            self.state.mode = InputMode::Normal;
        }
    }

    // ── Modal handlers ──────────────────────────────────────────────

    fn handle_modal_key(&mut self, key: KeyEvent) {
        let modal_kind = match &self.state.modal {
            Some(Modal::Observability(_)) => 0,
            Some(Modal::CommandPalette(_)) => 1,
            Some(Modal::Help) => 2,
            None => return,
        };

        match modal_kind {
            0 => self.handle_observability_key(key),
            1 => self.handle_command_palette_key(key),
            2 => {
                if key.code == KeyCode::Esc || key.code == KeyCode::Char('?') {
                    self.state.modal = None;
                }
            }
            _ => {}
        }
    }

    fn handle_observability_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(Modal::Observability(s)) = &mut self.state.modal {
                    s.scroll_offset = s.scroll_offset.saturating_add(1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(Modal::Observability(s)) = &mut self.state.modal {
                    s.scroll_offset = s.scroll_offset.saturating_sub(1);
                }
            }
            KeyCode::Esc => {
                self.state.modal = None;
            }
            _ => {}
        }
    }

    fn handle_command_palette_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down => {
                if let Some(Modal::CommandPalette(s)) = &mut self.state.modal
                    && !s.filtered.is_empty()
                {
                    s.selected = (s.selected + 1) % s.filtered.len();
                }
            }
            KeyCode::Up => {
                if let Some(Modal::CommandPalette(s)) = &mut self.state.modal {
                    if !s.filtered.is_empty() && s.selected > 0 {
                        s.selected -= 1;
                    } else if !s.filtered.is_empty() {
                        s.selected = s.filtered.len() - 1;
                    }
                }
            }
            KeyCode::Enter => {
                let should_close = self.execute_palette_command();
                if should_close {
                    self.state.modal = None;
                }
            }
            KeyCode::Backspace => {
                if let Some(Modal::CommandPalette(s)) = &mut self.state.modal {
                    s.query.pop();
                    self.refilter_palette();
                }
            }
            KeyCode::Esc => {
                self.state.modal = None;
            }
            KeyCode::Char(c) => {
                if let Some(Modal::CommandPalette(s)) = &mut self.state.modal {
                    s.query.push(c);
                    self.refilter_palette();
                }
            }
            _ => {}
        }
    }

    // ── Modal openers ───────────────────────────────────────────────

    fn open_command_palette(&mut self) {
        let commands = self.build_palette_commands();
        let filtered: Vec<usize> = (0..commands.len()).collect();
        self.state.modal = Some(Modal::CommandPalette(CommandPaletteState {
            query: String::new(),
            all_commands: commands,
            filtered,
            selected: 0,
        }));
    }

    fn open_observability(&mut self) {
        self.state.modal = Some(Modal::Observability(ObservabilityState {
            scroll_offset: 0,
        }));
    }

    fn build_palette_commands(&self) -> Vec<PaletteCommand> {
        let mut cmds = Vec::new();

        for agent in &self.state.agents {
            match agent.status {
                AgentStatus::Idle | AgentStatus::Error(_) => {
                    if agent.config.is_some() {
                        cmds.push(PaletteCommand {
                            label: format!("Connect {}", agent.name),
                            action: CommandAction::ConnectAgent(agent.name.clone()),
                        });
                    }
                }
                AgentStatus::Connected | AgentStatus::Busy => {
                    cmds.push(PaletteCommand {
                        label: format!("Disconnect {}", agent.name),
                        action: CommandAction::DisconnectAgent(agent.name.clone()),
                    });
                }
                AgentStatus::Connecting => {}
            }
        }

        // Tab commands.
        for tab in &self.state.tabs {
            cmds.push(PaletteCommand {
                label: format!("Switch to tab: {}", tab.agent_name),
                action: CommandAction::SwitchTab(tab.agent_name.clone()),
            });
        }
        cmds.push(PaletteCommand {
            label: "New tab...".to_string(),
            action: CommandAction::NewTab,
        });
        if !self.state.tabs.is_empty() {
            cmds.push(PaletteCommand {
                label: "Close current tab".to_string(),
                action: CommandAction::CloseTab,
            });
        }

        cmds.push(PaletteCommand {
            label: "Toggle observability".to_string(),
            action: CommandAction::ToggleObservability,
        });
        cmds.push(PaletteCommand {
            label: "Clear conversation".to_string(),
            action: CommandAction::ClearConversation,
        });
        cmds.push(PaletteCommand {
            label: "Show help".to_string(),
            action: CommandAction::ShowHelp,
        });

        cmds
    }

    fn refilter_palette(&mut self) {
        if let Some(Modal::CommandPalette(s)) = &mut self.state.modal {
            let query = s.query.to_lowercase();
            s.filtered = s
                .all_commands
                .iter()
                .enumerate()
                .filter(|(_, cmd)| cmd.label.to_lowercase().contains(&query))
                .map(|(i, _)| i)
                .collect();
            s.selected = 0;
        }
    }

    fn execute_palette_command(&mut self) -> bool {
        let action = if let Some(Modal::CommandPalette(s)) = &self.state.modal {
            s.filtered
                .get(s.selected)
                .and_then(|&idx| s.all_commands.get(idx))
                .map(|cmd| cmd.action.clone())
        } else {
            return true;
        };

        match action {
            Some(CommandAction::ToggleObservability) => {
                self.state.modal = None;
                self.open_observability();
                false
            }
            Some(CommandAction::ShowHelp) => {
                self.state.modal = Some(Modal::Help);
                false
            }
            Some(CommandAction::ConnectAgent(name)) => {
                self.connect_agent(&name);
                let tab_idx = self.ensure_tab(&name);
                self.switch_tab(tab_idx);
                true
            }
            Some(CommandAction::DisconnectAgent(name)) => {
                self.disconnect_agent(&name);
                true
            }
            Some(CommandAction::ClearConversation) => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    *sb = ScrollbackState::new();
                }
                true
            }
            Some(CommandAction::NewTab) => {
                self.state.modal = None;
                self.state.mode = InputMode::Agent;
                false
            }
            Some(CommandAction::CloseTab) => {
                self.close_current_tab();
                true
            }
            Some(CommandAction::SwitchTab(name)) => {
                if let Some(idx) = self.tab_for_agent(&name) {
                    self.switch_tab(idx);
                }
                true
            }
            None => true,
        }
    }

    // ── Tab lifecycle ──────────────────────────────────────────────

    fn close_current_tab(&mut self) {
        if self.state.tabs.is_empty() {
            return;
        }
        let idx = self.state.active_tab;
        let agent_name = self.state.tabs[idx].agent_name.clone();

        // Disconnect the agent if connected.
        self.disconnect_agent(&agent_name);

        self.state.tabs.remove(idx);
        // Immediately clamp active_tab to valid range.
        self.state.active_tab = if self.state.tabs.is_empty() {
            0
        } else {
            idx.min(self.state.tabs.len() - 1)
        };
    }

    // ── Input / autocomplete ────────────────────────────────────────

    fn after_input_char(&mut self) {
        // Re-parse @-mentions to update the target indicator.
        let text = self.state.input.text();
        let agent_names: Vec<String> = self.state.agents.iter().map(|a| a.name.clone()).collect();
        self.state.input_target = input::parse_mentions(&text, &agent_names);
        self.update_autocomplete();
    }

    fn update_autocomplete(&mut self) {
        let (row, col) = self.state.input.cursor;
        let line = match self.state.input.lines.get(row) {
            Some(l) => l.as_str(),
            None => {
                self.state.autocomplete = None;
                return;
            }
        };

        let prefix = match input::autocomplete_prefix(line, col) {
            Some(p) => p,
            None => {
                self.state.autocomplete = None;
                return;
            }
        };

        let agent_names: Vec<String> = self.state.agents.iter().map(|a| a.name.clone()).collect();
        let candidates = input::filter_candidates(&prefix, &agent_names);
        if candidates.is_empty() {
            self.state.autocomplete = None;
        } else {
            self.state.autocomplete = Some(AutocompleteState {
                prefix,
                candidates,
                selected: 0,
            });
        }
    }

    fn accept_autocomplete(&mut self) {
        let chosen = match &self.state.autocomplete {
            Some(ac) => ac.candidates.get(ac.selected).cloned(),
            None => None,
        };
        let prefix_len = self
            .state
            .autocomplete
            .as_ref()
            .map_or(0, |ac| ac.prefix.len());

        if let Some(name) = chosen {
            self.state.input.delete_before(prefix_len);
            self.state.input.insert_str(&name);
            self.state.input.insert_char(' ');
        }

        self.close_autocomplete();
        self.after_input_char();
    }

    fn close_autocomplete(&mut self) {
        self.state.autocomplete = None;
    }

    fn submit_input(&mut self) {
        let raw_text = self.state.input.text();
        if raw_text.trim().is_empty() {
            return;
        }

        let agent_names: Vec<String> = self.state.agents.iter().map(|a| a.name.clone()).collect();
        let target = input::parse_mentions(&raw_text, &agent_names);
        let clean_text = input::strip_mentions(&raw_text);

        if clean_text.trim().is_empty() {
            return;
        }

        // Resolve target agent(s).
        let targets: Vec<String> = match &target {
            InputTarget::Default => {
                // Route to active tab's agent, or find first available.
                if let Some(name) = self.state.active_agent_name() {
                    vec![name.to_string()]
                } else {
                    // No active tab — try first connected agent.
                    match self
                        .state
                        .agents
                        .iter()
                        .find(|a| matches!(a.status, AgentStatus::Connected | AgentStatus::Busy))
                    {
                        Some(a) => vec![a.name.clone()],
                        None => {
                            // Try first available agent (will lazy-connect).
                            match self.state.agents.iter().find(|a| a.config.is_some()) {
                                Some(a) => vec![a.name.clone()],
                                None => {
                                    self.push_system_msg("No agents available. Install an ACP agent and ensure it's on PATH.");
                                    return;
                                }
                            }
                        }
                    }
                }
            }
            InputTarget::Specific(names) => names.clone(),
            InputTarget::All => self
                .state
                .agents
                .iter()
                .filter(|a| {
                    matches!(
                        a.status,
                        AgentStatus::Connected | AgentStatus::Busy | AgentStatus::Idle
                    ) && a.config.is_some()
                })
                .map(|a| a.name.clone())
                .collect(),
        };

        if targets.is_empty() {
            self.push_system_msg("No agents to send to.");
            return;
        }

        // Push user prompt to each target tab's scrollback.
        for agent_name in &targets {
            let tab_idx = self.ensure_tab(agent_name);
            let sb = &mut self.state.tabs[tab_idx].scrollback;
            let id = sb.next_id();
            sb.entries.push(ActivityEntry {
                id,
                kind: EntryKind::UserPrompt(UserPrompt {
                    text: raw_text.clone(),
                    targets: targets.clone(),
                }),
                collapsed: false,
            });
        }

        // Clear input.
        self.state.input.clear();
        self.state.input_target = InputTarget::Default;
        self.close_autocomplete();

        // Switch to the first target's tab.
        if let Some(first_target) = targets.first()
            && let Some(tab_idx) = self.tab_for_agent(first_target)
        {
            self.switch_tab(tab_idx);
            if let Some(sb) = self.state.active_scrollback_mut() {
                sb.follow = true;
            }
        }

        // Send to each target agent.
        for agent_name in &targets {
            // Lazy-connect if needed.
            if !self.agent_providers.contains_key(agent_name) {
                self.connect_agent(agent_name);
            }
            // Reset streaming cursor for fresh response.
            if let Some(sb) = self.scrollback_for_agent(agent_name) {
                sb.streaming_entry.remove(agent_name);
            }

            // Mark as busy.
            if let Some(agent) = self.state.agents.iter_mut().find(|a| &a.name == agent_name)
                && matches!(agent.status, AgentStatus::Connected)
            {
                agent.status = AgentStatus::Busy;
            }

            // Send prompt via provider.
            if let Some(provider) = self.agent_providers.get(agent_name) {
                provider.try_prompt(clean_text.clone());
            }
            self.state.obs_log.push(ObsEvent {
                agent_id: agent_name.clone(),
                kind: ObsEventKind::PromptSent,
                timestamp: Instant::now(),
            });
        }
    }

    // ── Agent lifecycle ─────────────────────────────────────────────

    fn connect_agent(&mut self, agent_id: &str) {
        if self.agent_providers.contains_key(agent_id) {
            return; // Already connected or connecting.
        }

        let agent = match self.state.agents.iter_mut().find(|a| a.name == agent_id) {
            Some(a) => a,
            None => return,
        };

        let config = match &agent.config {
            Some(c) => c.clone(),
            None => {
                let msg = format!(
                    "No ACP adapter found for {agent_id}. Install the adapter and ensure it's on PATH."
                );
                self.push_system_msg(&msg);
                return;
            }
        };

        agent.status = AgentStatus::Connecting;

        // Ensure a tab exists for this agent.
        self.ensure_tab(agent_id);

        // Create event channel for this agent and forward to the main event loop.
        let (agent_event_tx, mut agent_event_rx) = mpsc::channel::<AgentEvent>(256);
        let app_event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            while let Some(evt) = agent_event_rx.recv().await {
                if app_event_tx.send(AppEvent::Agent(evt)).await.is_err() {
                    break;
                }
            }
        });

        let provider = AcpAgentProvider::spawn(agent_id.to_string(), &config, agent_event_tx);
        self.agent_providers.insert(agent_id.to_string(), provider);
    }

    fn disconnect_agent(&mut self, agent_id: &str) {
        // Drop the provider (closes command channel → agent thread exits).
        self.agent_providers.remove(agent_id);
        // The agent thread will send AgentDisconnected, which handles the rest.
    }

    fn rediscover_agents(&mut self) {
        let known = bitrouter_config::builtin_agent_defs();
        let discovered = discover_agents(&known);
        for da in discovered {
            if !self.state.agents.iter().any(|a| a.name == da.name) {
                let idx = self.state.agents.len();
                self.state.agents.push(crate::model::Agent {
                    name: da.name,
                    config: Some(bitrouter_config::AgentConfig {
                        protocol: bitrouter_config::AgentProtocol::Acp,
                        binary: da.binary.to_string_lossy().into_owned(),
                        args: da.args,
                        enabled: true,
                    }),
                    status: AgentStatus::Idle,
                    session_id: None,
                    color: agent_color(idx),
                });
            }
        }
    }

    /// Mark the current streaming entry for an agent as no longer streaming.
    fn finalize_streaming_in(sb: &mut ScrollbackState, agent_id: &str) {
        if let Some(&old_id) = sb.streaming_entry.get(agent_id)
            && let Some(idx) = sb.index_of(old_id)
        {
            match &mut sb.entries[idx].kind {
                EntryKind::AgentResponse(resp) => resp.is_streaming = false,
                EntryKind::Thinking(th) => th.is_streaming = false,
                _ => {}
            }
        }
    }

    // ── Helpers ─────────────────────────────────────────────────────

    /// Push a system message to a specific tab.
    fn push_system_msg_to_tab(&mut self, tab_idx: usize, text: &str) {
        if let Some(tab) = self.state.tabs.get_mut(tab_idx) {
            let id = tab.scrollback.next_id();
            tab.scrollback.entries.push(ActivityEntry {
                id,
                kind: EntryKind::System(SystemNotice {
                    text: text.to_string(),
                }),
                collapsed: false,
            });
        }
    }

    /// Push a system message to the active tab (no-op if no tabs).
    fn push_system_msg(&mut self, text: &str) {
        let idx = self.state.active_tab;
        self.push_system_msg_to_tab(idx, text);
    }
}

/// Permission response choice (single key).
enum PermissionChoice {
    Yes,
    No,
    Always,
}

/// Check if an entry's text content contains the query string.
fn entry_contains_text(kind: &EntryKind, query: &str) -> bool {
    match kind {
        EntryKind::UserPrompt(p) => p.text.to_lowercase().contains(query),
        EntryKind::AgentResponse(r) => r.blocks.iter().any(|b| match b {
            ContentBlock::Text(t) => t.to_lowercase().contains(query),
            ContentBlock::Other(d) => d.to_lowercase().contains(query),
        }),
        EntryKind::ToolCall(tc) => tc.title.to_lowercase().contains(query),
        EntryKind::Thinking(th) => th.text.to_lowercase().contains(query),
        EntryKind::Permission(p) => p.request.title.to_lowercase().contains(query),
        EntryKind::System(s) => s.text.to_lowercase().contains(query),
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
