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
    ActivityEntry, AgentManagerState, AgentResponse, AgentStatus, AutocompleteState, CommandAction,
    CommandPaletteState, ContentBlock, EntryKind, InlineInput, InputTarget, Modal, ObsEvent,
    ObsEventKind, ObsLog, ObservabilityState, PaletteCommand, PermissionEntry, ScrollbackState,
    SystemNotice, ThinkingEntry, ToolCallEntry, UserPrompt, agent_color,
};
use crate::ui;

/// Which mode the TUI is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Focus {
    /// Normal mode: inline prompt has focus, scrollback auto-follows.
    Input,
    /// Scroll mode: user is browsing scrollback history.
    Scroll,
}

/// All mutable TUI state, separated from `App` so the borrow checker allows
/// passing `&mut state` into the draw closure while checking `app.running`.
pub struct AppState {
    pub focus: Focus,
    pub agents: Vec<crate::model::Agent>,
    pub default_agent: Option<String>,
    pub scrollback: ScrollbackState,
    pub input: InlineInput,
    pub input_target: InputTarget,
    pub autocomplete: Option<AutocompleteState>,
    pub modal: Option<Modal>,
    pub obs_log: ObsLog,
    pub config: TuiConfig,
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
                focus: Focus::Input,
                agents,
                default_agent: None,
                scrollback: ScrollbackState::new(),
                input: InlineInput::new(),
                input_target: InputTarget::Default,
                autocomplete: None,
                modal: None,
                obs_log: ObsLog::new(),
                config,
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
        // Auto-set default if none exists.
        if self.state.default_agent.is_none() {
            self.state.default_agent = Some(agent_id.clone());
        }
        self.push_system_msg(&format!("Connected to {agent_id}"));
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
        // Reassign default if this was the default agent.
        if self.state.default_agent.as_deref() == Some(&agent_id) {
            self.state.default_agent = self
                .state
                .agents
                .iter()
                .find(|a| matches!(a.status, AgentStatus::Connected | AgentStatus::Busy))
                .map(|a| a.name.clone());
        }
        // Clear streaming cursor for this agent.
        self.state.scrollback.streaming_entry.remove(&agent_id);
        // Remove from active focus.
        self.state
            .scrollback
            .agent_focus
            .active
            .retain(|a| a != &agent_id);
        if self.state.scrollback.agent_focus.focused.as_deref() == Some(&agent_id) {
            self.state.scrollback.agent_focus.focused =
                self.state.scrollback.agent_focus.active.first().cloned();
        }

        self.push_system_msg(&format!("Disconnected from {agent_id}"));
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
        self.state.scrollback.streaming_entry.remove(&agent_id);
        self.push_system_msg(&format!("[{agent_id}] Error: {message}"));
        self.state.obs_log.push(ObsEvent {
            agent_id,
            kind: ObsEventKind::Error { message },
            timestamp: Instant::now(),
        });
    }

    // ── Agent content handlers ────────────────────────────────────────

    fn ensure_agent_active(&mut self, agent_id: &str) {
        let focus = &mut self.state.scrollback.agent_focus;
        if !focus.active.iter().any(|a| a == agent_id) {
            focus.active.push(agent_id.to_string());
        }
        if focus.focused.is_none() {
            focus.focused = Some(agent_id.to_string());
        }
    }

    fn apply_agent_message_chunk(&mut self, agent_id: &str, text: String) {
        self.ensure_agent_active(agent_id);

        // Try to extend existing streaming entry for this agent.
        if let Some(&entry_id) = self.state.scrollback.streaming_entry.get(agent_id)
            && let Some(idx) = self.state.scrollback.index_of(entry_id)
            && let EntryKind::AgentResponse(resp) = &mut self.state.scrollback.entries[idx].kind
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
        self.finalize_streaming_entry(agent_id);

        // Start a new agent message entry.
        let id = self.state.scrollback.next_id();
        self.state.scrollback.entries.push(ActivityEntry {
            id,
            kind: EntryKind::AgentResponse(AgentResponse {
                agent_id: agent_id.to_string(),
                blocks: vec![ContentBlock::Text(text)],
                is_streaming: true,
            }),
            timestamp: Instant::now(),
            collapsed: false,
        });
        self.state
            .scrollback
            .streaming_entry
            .insert(agent_id.to_string(), id);
    }

    fn apply_non_text_content(&mut self, agent_id: &str, desc: String) {
        self.ensure_agent_active(agent_id);

        // Append as an Other block to the current streaming entry, or create new.
        if let Some(&entry_id) = self.state.scrollback.streaming_entry.get(agent_id)
            && let Some(idx) = self.state.scrollback.index_of(entry_id)
            && let EntryKind::AgentResponse(resp) = &mut self.state.scrollback.entries[idx].kind
        {
            resp.blocks.push(ContentBlock::Other(desc));
            return;
        }

        let id = self.state.scrollback.next_id();
        self.state.scrollback.entries.push(ActivityEntry {
            id,
            kind: EntryKind::AgentResponse(AgentResponse {
                agent_id: agent_id.to_string(),
                blocks: vec![ContentBlock::Other(desc)],
                is_streaming: true,
            }),
            timestamp: Instant::now(),
            collapsed: false,
        });
        self.state
            .scrollback
            .streaming_entry
            .insert(agent_id.to_string(), id);
    }

    fn apply_thought_chunk(&mut self, agent_id: &str, text: String) {
        self.ensure_agent_active(agent_id);

        // Try to extend existing streaming thinking entry.
        if let Some(&entry_id) = self.state.scrollback.streaming_entry.get(agent_id)
            && let Some(idx) = self.state.scrollback.index_of(entry_id)
            && let EntryKind::Thinking(th) = &mut self.state.scrollback.entries[idx].kind
            && th.is_streaming
        {
            th.text.push_str(&text);
            return;
        }

        // Finalize any previous streaming entry before starting new.
        self.finalize_streaming_entry(agent_id);

        let id = self.state.scrollback.next_id();
        self.state.scrollback.entries.push(ActivityEntry {
            id,
            kind: EntryKind::Thinking(ThinkingEntry {
                agent_id: agent_id.to_string(),
                text,
                is_streaming: true,
            }),
            timestamp: Instant::now(),
            collapsed: false,
        });
        self.state
            .scrollback
            .streaming_entry
            .insert(agent_id.to_string(), id);
    }

    fn apply_tool_call(
        &mut self,
        agent_id: &str,
        tool_call_id: String,
        title: String,
        status: ToolCallStatus,
    ) {
        self.ensure_agent_active(agent_id);

        let id = self.state.scrollback.next_id();
        self.state.scrollback.entries.push(ActivityEntry {
            id,
            kind: EntryKind::ToolCall(ToolCallEntry {
                agent_id: agent_id.to_string(),
                tool_call_id,
                title: title.clone(),
                status,
            }),
            timestamp: Instant::now(),
            collapsed: false,
        });
        self.state.obs_log.push(ObsEvent {
            agent_id: agent_id.to_string(),
            kind: ObsEventKind::ToolCall { title },
            timestamp: Instant::now(),
        });
        // Tool calls break the streaming cursor — next message chunk starts fresh.
        self.state.scrollback.streaming_entry.remove(agent_id);
    }

    fn apply_tool_call_update(
        &mut self,
        agent_id: &str,
        tool_call_id: String,
        new_title: Option<String>,
        new_status: Option<ToolCallStatus>,
    ) {
        // Find the tool call entry by ID and update it.
        for entry in self.state.scrollback.entries.iter_mut().rev() {
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
        let id = self.state.scrollback.next_id();
        self.state.scrollback.entries.push(ActivityEntry {
            id,
            kind: EntryKind::Permission(PermissionEntry {
                agent_id,
                request: Box::new(request),
                response_tx: Some(response_tx),
                resolved: false,
            }),
            timestamp: Instant::now(),
            collapsed: false,
        });
        // Re-pin to bottom so user sees the permission prompt.
        self.state.scrollback.follow = true;
    }

    fn handle_prompt_done(&mut self, agent_id: String) {
        // Mark the streaming entry as complete.
        if let Some(entry_id) = self.state.scrollback.streaming_entry.remove(&agent_id)
            && let Some(idx) = self.state.scrollback.index_of(entry_id)
        {
            match &mut self.state.scrollback.entries[idx].kind {
                EntryKind::AgentResponse(resp) => resp.is_streaming = false,
                EntryKind::Thinking(th) => th.is_streaming = false,
                _ => {}
            }
        }
        // Update agent status.
        if let Some(agent) = self.state.agents.iter_mut().find(|a| a.name == agent_id)
            && matches!(agent.status, AgentStatus::Busy)
        {
            agent.status = AgentStatus::Connected;
        }
        self.state.obs_log.push(ObsEvent {
            agent_id: agent_id.clone(),
            kind: ObsEventKind::PromptDone,
            timestamp: Instant::now(),
        });

        // Multi-agent focus: if focused agent is done, auto-focus next active.
        let focus = &mut self.state.scrollback.agent_focus;
        if focus.focused.as_deref() == Some(&agent_id) {
            // Check if any other agent is still streaming.
            let still_active: Vec<String> = focus
                .active
                .iter()
                .filter(|a| {
                    *a != &agent_id
                        && self
                            .state
                            .agents
                            .iter()
                            .any(|ag| ag.name == **a && matches!(ag.status, AgentStatus::Busy))
                })
                .cloned()
                .collect();

            if !still_active.is_empty() {
                focus.focused = still_active.into_iter().next();
            } else {
                // All done — clear focus so all blocks are visible in stacked view.
                focus.focused = None;
                focus.active.clear();
            }
        }
        // If no agents are busy anymore, clear the active list entirely.
        let any_busy = self
            .state
            .agents
            .iter()
            .any(|a| matches!(a.status, AgentStatus::Busy));
        if !any_busy {
            self.state.scrollback.agent_focus.focused = None;
            self.state.scrollback.agent_focus.active.clear();
        }
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

        // Check for pending permission request — it takes priority.
        if self.has_pending_permission() {
            self.handle_permission_key(key);
            return;
        }

        // Global shortcuts (work in any focus).
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('g') => {
                    self.open_agent_manager();
                    return;
                }
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

        // '?' opens help (only when not typing).
        if key.code == KeyCode::Char('?') && self.state.focus != Focus::Input {
            self.state.modal = Some(Modal::Help);
            return;
        }

        // Dispatch to focused mode.
        match self.state.focus {
            Focus::Scroll => self.handle_scroll_key(key),
            Focus::Input => self.handle_input_key(key),
        }
    }

    fn handle_scroll_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.state.scrollback.scroll_offset =
                    self.state.scrollback.scroll_offset.saturating_add(1);
                self.state.scrollback.follow = false;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.state.scrollback.scroll_offset =
                    self.state.scrollback.scroll_offset.saturating_sub(1);
                self.state.scrollback.follow = false;
            }
            KeyCode::PageDown => {
                self.state.scrollback.scroll_offset =
                    self.state.scrollback.scroll_offset.saturating_add(20);
                self.state.scrollback.follow = false;
            }
            KeyCode::PageUp => {
                self.state.scrollback.scroll_offset =
                    self.state.scrollback.scroll_offset.saturating_sub(20);
                self.state.scrollback.follow = false;
            }
            KeyCode::Char('G') => {
                // Jump to bottom, re-enter Input mode.
                self.state.scrollback.follow = true;
                self.state.focus = Focus::Input;
            }
            KeyCode::Char('i') => {
                self.state.scrollback.follow = true;
                self.state.focus = Focus::Input;
            }
            KeyCode::Tab => {
                self.state.scrollback.agent_focus.cycle();
            }
            _ => {
                // Any printable char returns to input mode.
                if let KeyCode::Char(c) = key.code {
                    self.state.scrollback.follow = true;
                    self.state.focus = Focus::Input;
                    self.state.input.insert_char(c);
                    self.after_input_char();
                }
            }
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent) {
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
                } else if self.state.scrollback.agent_focus.active.len() > 1 {
                    self.state.scrollback.agent_focus.cycle();
                }
            }
            KeyCode::Esc => {
                if self.state.autocomplete.is_some() {
                    self.close_autocomplete();
                } else {
                    self.state.scrollback.follow = false;
                    self.state.focus = Focus::Scroll;
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

    // ── Permission handling ─────────────────────────────────────────

    fn has_pending_permission(&self) -> bool {
        self.state.scrollback.entries.iter().any(|e| {
            matches!(
                &e.kind,
                EntryKind::Permission(p) if !p.resolved
            )
        })
    }

    fn handle_permission_key(&mut self, key: KeyEvent) {
        // Find the unresolved permission entry.
        let perm_idx = match self
            .state
            .scrollback
            .entries
            .iter()
            .position(|e| matches!(&e.kind, EntryKind::Permission(p) if !p.resolved))
        {
            Some(idx) => idx,
            None => return,
        };

        match key.code {
            KeyCode::Char('y') => self.resolve_permission(perm_idx, PermissionChoice::Yes),
            KeyCode::Char('n') => self.resolve_permission(perm_idx, PermissionChoice::No),
            KeyCode::Char('a') => self.resolve_permission(perm_idx, PermissionChoice::Always),
            _ => {} // Ignore all other keys during permission.
        }
    }

    fn resolve_permission(&mut self, entry_idx: usize, choice: PermissionChoice) {
        if let EntryKind::Permission(perm) = &mut self.state.scrollback.entries[entry_idx].kind {
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
    }

    // ── Modal handlers ──────────────────────────────────────────────

    fn handle_modal_key(&mut self, key: KeyEvent) {
        // Clone the modal enum discriminant to avoid borrow issues.
        let modal_kind = match &self.state.modal {
            Some(Modal::AgentManager(_)) => 0,
            Some(Modal::Observability(_)) => 1,
            Some(Modal::CommandPalette(_)) => 2,
            Some(Modal::Help) => 3,
            None => return,
        };

        match modal_kind {
            0 => self.handle_agent_manager_key(key),
            1 => self.handle_observability_key(key),
            2 => self.handle_command_palette_key(key),
            3 => {
                if key.code == KeyCode::Esc || key.code == KeyCode::Char('?') {
                    self.state.modal = None;
                }
            }
            _ => {}
        }
    }

    fn handle_agent_manager_key(&mut self, key: KeyEvent) {
        let agent_count = self.state.agents.len();
        if agent_count == 0 {
            if key.code == KeyCode::Esc {
                self.state.modal = None;
            }
            return;
        }

        let selected = match &self.state.modal {
            Some(Modal::AgentManager(s)) => s.selected,
            _ => return,
        };

        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(Modal::AgentManager(s)) = &mut self.state.modal {
                    s.selected = (s.selected + 1) % agent_count;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(Modal::AgentManager(s)) = &mut self.state.modal {
                    if s.selected > 0 {
                        s.selected -= 1;
                    } else {
                        s.selected = agent_count - 1;
                    }
                }
            }
            KeyCode::Char('c') => {
                if let Some(agent) = self.state.agents.get(selected) {
                    let name = agent.name.clone();
                    self.connect_agent(&name);
                }
            }
            KeyCode::Char('d') => {
                if let Some(agent) = self.state.agents.get(selected) {
                    let name = agent.name.clone();
                    self.disconnect_agent(&name);
                }
            }
            KeyCode::Char('s') => {
                if let Some(agent) = self.state.agents.get(selected)
                    && matches!(agent.status, AgentStatus::Connected | AgentStatus::Busy)
                {
                    self.state.default_agent = Some(agent.name.clone());
                }
            }
            KeyCode::Char('r') => {
                self.rediscover_agents();
            }
            KeyCode::Esc => {
                self.state.modal = None;
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

    fn open_agent_manager(&mut self) {
        self.state.modal = Some(Modal::AgentManager(AgentManagerState { selected: 0 }));
    }

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
                    cmds.push(PaletteCommand {
                        label: format!("Set {} as default", agent.name),
                        action: CommandAction::SetDefaultAgent(agent.name.clone()),
                    });
                }
                AgentStatus::Connecting => {}
            }
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

    /// Execute the selected palette command. Returns `true` if the palette
    /// modal should be closed afterwards, `false` if the action replaced it
    /// with a different modal.
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
                false // Observability modal is now open.
            }
            Some(CommandAction::ShowHelp) => {
                self.state.modal = Some(Modal::Help);
                false // Help modal is now open.
            }
            Some(CommandAction::ConnectAgent(name)) => {
                self.connect_agent(&name);
                true
            }
            Some(CommandAction::DisconnectAgent(name)) => {
                self.disconnect_agent(&name);
                true
            }
            Some(CommandAction::SetDefaultAgent(name)) => {
                self.state.default_agent = Some(name);
                true
            }
            Some(CommandAction::ClearConversation) => {
                self.state.scrollback = ScrollbackState::new();
                true
            }
            None => true,
        }
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
            // Delete the prefix characters that were typed after '@'.
            self.state.input.delete_before(prefix_len);
            // Insert the full name + trailing space.
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
                if let Some(default) = &self.state.default_agent {
                    vec![default.clone()]
                } else {
                    // Try first connected agent.
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

        // Push user prompt to scrollback.
        let id = self.state.scrollback.next_id();
        self.state.scrollback.entries.push(ActivityEntry {
            id,
            kind: EntryKind::UserPrompt(UserPrompt {
                text: raw_text,
                targets: targets.clone(),
            }),
            timestamp: Instant::now(),
            collapsed: false,
        });

        // Clear input.
        self.state.input.clear();
        self.state.input_target = InputTarget::Default;
        self.close_autocomplete();
        self.state.scrollback.follow = true;

        // Reset agent focus for new prompt round.
        self.state.scrollback.agent_focus.focused = None;
        self.state.scrollback.agent_focus.active.clear();

        // Send to each target agent.
        for agent_name in &targets {
            // Lazy-connect if needed.
            if !self.agent_providers.contains_key(agent_name) {
                self.connect_agent(agent_name);
            }
            // Reset streaming cursor for fresh response.
            self.state.scrollback.streaming_entry.remove(agent_name);

            // Mark as busy.
            if let Some(agent) = self.state.agents.iter_mut().find(|a| &a.name == agent_name)
                && matches!(agent.status, AgentStatus::Connected)
            {
                agent.status = AgentStatus::Busy;
            }

            // Send prompt via provider (try_send avoids needing .await).
            if let Some(provider) = self.agent_providers.get(agent_name) {
                provider.try_prompt(clean_text.clone());
            }
            self.state.obs_log.push(ObsEvent {
                agent_id: agent_name.clone(),
                kind: ObsEventKind::PromptSent,
                timestamp: Instant::now(),
            });
        }

        // Auto-set default if not set.
        if self.state.default_agent.is_none() {
            self.state.default_agent = targets.into_iter().next();
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
                self.push_system_msg(&format!(
                    "No ACP adapter found for {agent_id}. Install the adapter and ensure it's on PATH."
                ));
                return;
            }
        };

        agent.status = AgentStatus::Connecting;

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
    /// Call this before switching to a different entry kind (e.g. thought → message).
    fn finalize_streaming_entry(&mut self, agent_id: &str) {
        if let Some(&old_id) = self.state.scrollback.streaming_entry.get(agent_id)
            && let Some(idx) = self.state.scrollback.index_of(old_id)
        {
            match &mut self.state.scrollback.entries[idx].kind {
                EntryKind::AgentResponse(resp) => resp.is_streaming = false,
                EntryKind::Thinking(th) => th.is_streaming = false,
                _ => {}
            }
        }
    }

    // ── Helpers ─────────────────────────────────────────────────────

    fn push_system_msg(&mut self, text: &str) {
        let id = self.state.scrollback.next_id();
        self.state.scrollback.entries.push(ActivityEntry {
            id,
            kind: EntryKind::System(SystemNotice {
                text: text.to_string(),
            }),
            timestamp: Instant::now(),
            collapsed: false,
        });
    }
}

/// Permission response choice (single key).
enum PermissionChoice {
    Yes,
    No,
    Always,
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
