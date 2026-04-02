use std::collections::HashMap;
use std::io::Stdout;
use std::time::Instant;

use agent_client_protocol as acp;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui_textarea::TextArea;
use tokio::sync::mpsc;

use crate::TuiConfig;
use crate::acp::connection::{AgentCommand, AgentConnection, spawn_agent};
use crate::acp::discovery::discover_agents;
use crate::error::TuiError;
use crate::event::{AppEvent, EventHandler};
use crate::input;
use crate::model::{
    ActivityEntry, AgentManagerState, AgentStatus, AutocompleteState, CommandAction,
    CommandPaletteState, ContentBlock, EntryKind, FeedState, InputTarget, Modal, ObsEvent,
    ObsEventKind, ObsLog, ObservabilityState, PaletteCommand, agent_color,
};
use crate::ui;

/// Which panel owns keyboard focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Focus {
    Feed,
    Input,
}

/// All mutable TUI state, separated from `App` so the borrow checker allows
/// passing `&mut state` into the draw closure while checking `app.running`.
pub struct AppState {
    pub focus: Focus,
    pub agents: Vec<crate::model::Agent>,
    pub default_agent: Option<String>,
    pub feed: FeedState,
    pub input: TextArea<'static>,
    pub input_target: InputTarget,
    pub autocomplete: Option<AutocompleteState>,
    pub modal: Option<Modal>,
    pub obs_log: ObsLog,
    pub config: TuiConfig,
}

pub struct App {
    pub running: bool,
    pub state: AppState,
    /// Multi-agent connections, keyed by agent name.
    agent_connections: HashMap<String, AgentConnection>,
    /// Thread handles for agent subprocesses.
    agent_handles: HashMap<String, std::thread::JoinHandle<()>>,
    /// Cloned event sender for spawning agent connections.
    event_tx: mpsc::Sender<AppEvent>,
}

impl App {
    pub fn new(config: TuiConfig, event_tx: mpsc::Sender<AppEvent>) -> Self {
        let discovered = discover_agents();
        let agents: Vec<crate::model::Agent> = discovered
            .into_iter()
            .enumerate()
            .map(|(i, old)| crate::model::Agent {
                name: old.name,
                launch: old.launch,
                status: AgentStatus::Idle,
                session_id: None,
                color: agent_color(i),
            })
            .collect();

        Self {
            running: true,
            state: AppState {
                focus: Focus::Input,
                agents,
                default_agent: None,
                feed: FeedState::new(),
                input: TextArea::default(),
                input_target: InputTarget::Default,
                autocomplete: None,
                modal: None,
                obs_log: ObsLog::new(),
                config,
            },
            agent_connections: HashMap::new(),
            agent_handles: HashMap::new(),
            event_tx,
        }
    }

    fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Key(key) => self.handle_key(key),
            AppEvent::Resize { .. } | AppEvent::Tick => {}
            AppEvent::AgentConnected {
                agent_id,
                session_id,
            } => self.handle_agent_connected(agent_id, session_id),
            AppEvent::AgentDisconnected { agent_id } => self.handle_agent_disconnected(agent_id),
            AppEvent::AgentError { agent_id, message } => {
                self.handle_agent_error(agent_id, message);
            }
            AppEvent::SessionUpdate {
                agent_id,
                notification,
            } => self.handle_session_update(agent_id, notification),
            AppEvent::PermissionRequest {
                agent_id,
                request,
                response_tx,
            } => self.handle_permission_request(agent_id, request, response_tx),
            AppEvent::PromptDone {
                agent_id,
                _stop_reason: _,
            } => self.handle_prompt_done(agent_id),
        }
    }

    // ── ACP lifecycle handlers ──────────────────────────────────────

    fn handle_agent_connected(&mut self, agent_id: String, session_id: acp::SessionId) {
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
        // Clean up connection handle.
        self.agent_connections.remove(&agent_id);
        if let Some(handle) = self.agent_handles.remove(&agent_id) {
            let _ = handle.join();
        }
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
        self.state.feed.streaming_entry.remove(&agent_id);
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
        self.state.feed.streaming_entry.remove(&agent_id);
        self.push_system_msg(&format!("[{agent_id}] Error: {message}"));
        self.state.obs_log.push(ObsEvent {
            agent_id,
            kind: ObsEventKind::Error { message },
            timestamp: Instant::now(),
        });
    }

    // ── ACP content handlers ────────────────────────────────────────

    fn handle_session_update(&mut self, agent_id: String, notif: acp::SessionNotification) {
        match notif.update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                self.apply_agent_message_chunk(&agent_id, chunk);
            }
            acp::SessionUpdate::AgentThoughtChunk(chunk) => {
                self.apply_thought_chunk(&agent_id, chunk);
            }
            acp::SessionUpdate::ToolCall(tool_call) => {
                self.apply_tool_call(&agent_id, tool_call);
            }
            acp::SessionUpdate::ToolCallUpdate(update) => {
                self.apply_tool_call_update(&agent_id, update);
            }
            _ => {
                // Plan, AvailableCommandsUpdate, CurrentModeUpdate, etc.
            }
        }
    }

    fn apply_agent_message_chunk(&mut self, agent_id: &str, chunk: acp::ContentChunk) {
        let text = match chunk.content {
            acp::ContentBlock::Text(tc) => tc.text,
            other => {
                return self.apply_non_text_content(agent_id, other);
            }
        };

        // Try to extend existing streaming entry for this agent.
        if let Some(&entry_id) = self.state.feed.streaming_entry.get(agent_id)
            && let Some(idx) = self.state.feed.index_of(entry_id)
            && let EntryKind::AgentMessage { blocks, .. } = &mut self.state.feed.entries[idx].kind
        {
            // Extend last text block or push new one.
            if let Some(ContentBlock::Text(existing)) = blocks.last_mut() {
                existing.push_str(&text);
            } else {
                blocks.push(ContentBlock::Text(text));
            }
            return;
        }

        // Finalize any previous streaming entry (e.g. a Thinking entry)
        // before starting a new AgentMessage.
        self.finalize_streaming_entry(agent_id);

        // Start a new agent message entry.
        let id = self.state.feed.next_id();
        self.state.feed.entries.push(ActivityEntry {
            id,
            kind: EntryKind::AgentMessage {
                agent_id: agent_id.to_string(),
                blocks: vec![ContentBlock::Text(text)],
                is_streaming: true,
            },
            timestamp: Instant::now(),
            collapsed: false,
        });
        self.state
            .feed
            .streaming_entry
            .insert(agent_id.to_string(), id);
    }

    fn apply_non_text_content(&mut self, agent_id: &str, block: acp::ContentBlock) {
        let desc = match block {
            acp::ContentBlock::Image(_) => "<image>".to_string(),
            acp::ContentBlock::Audio(_) => "<audio>".to_string(),
            acp::ContentBlock::ResourceLink(rl) => format!("[{}]({})", rl.name, rl.uri),
            acp::ContentBlock::Resource(_) => "<resource>".to_string(),
            _ => "<unknown>".to_string(),
        };

        // Append as an Other block to the current streaming entry, or create new.
        if let Some(&entry_id) = self.state.feed.streaming_entry.get(agent_id)
            && let Some(idx) = self.state.feed.index_of(entry_id)
            && let EntryKind::AgentMessage { blocks, .. } = &mut self.state.feed.entries[idx].kind
        {
            blocks.push(ContentBlock::Other(desc));
            return;
        }

        let id = self.state.feed.next_id();
        self.state.feed.entries.push(ActivityEntry {
            id,
            kind: EntryKind::AgentMessage {
                agent_id: agent_id.to_string(),
                blocks: vec![ContentBlock::Other(desc)],
                is_streaming: true,
            },
            timestamp: Instant::now(),
            collapsed: false,
        });
        self.state
            .feed
            .streaming_entry
            .insert(agent_id.to_string(), id);
    }

    fn apply_thought_chunk(&mut self, agent_id: &str, chunk: acp::ContentChunk) {
        let text = match chunk.content {
            acp::ContentBlock::Text(tc) => tc.text,
            _ => return,
        };

        // Try to extend existing streaming thinking entry.
        if let Some(&entry_id) = self.state.feed.streaming_entry.get(agent_id)
            && let Some(idx) = self.state.feed.index_of(entry_id)
            && let EntryKind::Thinking {
                text: existing,
                is_streaming: true,
                ..
            } = &mut self.state.feed.entries[idx].kind
        {
            existing.push_str(&text);
            return;
        }

        // Finalize any previous streaming entry (e.g. an AgentMessage)
        // before starting a new Thinking entry.
        self.finalize_streaming_entry(agent_id);

        let id = self.state.feed.next_id();
        self.state.feed.entries.push(ActivityEntry {
            id,
            kind: EntryKind::Thinking {
                agent_id: agent_id.to_string(),
                text,
                is_streaming: true,
            },
            timestamp: Instant::now(),
            collapsed: false,
        });
        self.state
            .feed
            .streaming_entry
            .insert(agent_id.to_string(), id);
    }

    fn apply_tool_call(&mut self, agent_id: &str, tool_call: acp::ToolCall) {
        let id = self.state.feed.next_id();
        self.state.feed.entries.push(ActivityEntry {
            id,
            kind: EntryKind::ToolCall {
                agent_id: agent_id.to_string(),
                tool_call_id: tool_call.tool_call_id,
                title: tool_call.title,
                status: tool_call.status,
            },
            timestamp: Instant::now(),
            collapsed: false,
        });
        self.state.obs_log.push(ObsEvent {
            agent_id: agent_id.to_string(),
            kind: ObsEventKind::ToolCall {
                title: String::new(),
            },
            timestamp: Instant::now(),
        });
        // Tool calls break the streaming cursor — next message chunk starts fresh.
        self.state.feed.streaming_entry.remove(agent_id);
    }

    fn apply_tool_call_update(&mut self, agent_id: &str, update: acp::ToolCallUpdate) {
        // Find the tool call entry by ID and update it.
        for entry in self.state.feed.entries.iter_mut().rev() {
            if let EntryKind::ToolCall {
                tool_call_id,
                title,
                status,
                agent_id: entry_agent,
            } = &mut entry.kind
                && entry_agent == agent_id
                && *tool_call_id == update.tool_call_id
            {
                if let Some(new_title) = &update.fields.title {
                    *title = new_title.clone();
                }
                if let Some(new_status) = update.fields.status {
                    *status = new_status;
                    // Auto-collapse completed/failed tool calls.
                    if matches!(
                        new_status,
                        acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed
                    ) {
                        entry.collapsed = true;
                    }
                }
                return;
            }
        }

        // If not found, create from update.
        if let Ok(tool_call) = acp::ToolCall::try_from(update) {
            self.apply_tool_call(agent_id, tool_call);
        }
    }

    fn handle_permission_request(
        &mut self,
        agent_id: String,
        request: acp::RequestPermissionRequest,
        response_tx: tokio::sync::oneshot::Sender<acp::RequestPermissionResponse>,
    ) {
        let id = self.state.feed.next_id();
        self.state.feed.entries.push(ActivityEntry {
            id,
            kind: EntryKind::PermissionRequest {
                agent_id,
                request: Box::new(request),
                response_tx: Some(response_tx),
                selected: 0,
                resolved: false,
            },
            timestamp: Instant::now(),
            collapsed: false,
        });
        // Switch focus to feed so user can respond.
        self.state.focus = Focus::Feed;
        // Set cursor to the permission entry.
        self.state.feed.cursor = Some(self.state.feed.entries.len() - 1);
    }

    fn handle_prompt_done(&mut self, agent_id: String) {
        // Mark the streaming entry as complete.
        if let Some(entry_id) = self.state.feed.streaming_entry.remove(&agent_id)
            && let Some(idx) = self.state.feed.index_of(entry_id)
        {
            match &mut self.state.feed.entries[idx].kind {
                EntryKind::AgentMessage { is_streaming, .. } => *is_streaming = false,
                EntryKind::Thinking { is_streaming, .. } => *is_streaming = false,
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

        // Focus switching.
        match key.code {
            KeyCode::Char('i') if self.state.focus == Focus::Feed => {
                self.state.focus = Focus::Input;
                return;
            }
            KeyCode::Tab if self.state.focus == Focus::Feed => {
                self.state.focus = Focus::Input;
                return;
            }
            KeyCode::Esc if self.state.focus == Focus::Input => {
                self.close_autocomplete();
                self.state.focus = Focus::Feed;
                return;
            }
            _ => {}
        }

        // Dispatch to focused panel.
        match self.state.focus {
            Focus::Feed => self.handle_feed_key(key),
            Focus::Input => self.handle_input_key(key),
        }
    }

    fn handle_feed_key(&mut self, key: KeyEvent) {
        let entry_count = self.state.feed.entries.len();
        if entry_count == 0 {
            return;
        }

        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                let cursor = self.state.feed.cursor.unwrap_or(0);
                if cursor + 1 < entry_count {
                    self.state.feed.cursor = Some(cursor + 1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let cursor = self.state.feed.cursor.unwrap_or(0);
                self.state.feed.cursor = Some(cursor.saturating_sub(1));
            }
            KeyCode::Enter => {
                self.toggle_collapse_at_cursor();
            }
            _ => {}
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
                // Alt+Enter is handled below as a newline.
                if key.modifiers.contains(KeyModifiers::ALT) {
                    self.state.input.input(key);
                    return;
                }
                self.submit_input();
            }
            KeyCode::Tab => {
                if self.state.autocomplete.is_some() {
                    self.accept_autocomplete();
                } else {
                    self.state.input.input(key);
                    self.update_autocomplete();
                }
            }
            KeyCode::Esc => {
                if self.state.autocomplete.is_some() {
                    self.close_autocomplete();
                } else {
                    self.state.focus = Focus::Feed;
                }
            }
            _ => {
                self.state.input.input(key);
                self.after_input_char();
            }
        }
    }

    // ── Permission handling ─────────────────────────────────────────

    fn has_pending_permission(&self) -> bool {
        self.state.feed.entries.iter().any(|e| {
            matches!(
                &e.kind,
                EntryKind::PermissionRequest {
                    resolved: false,
                    ..
                }
            )
        })
    }

    fn handle_permission_key(&mut self, key: KeyEvent) {
        // Find the unresolved permission entry.
        let perm_idx = match self.state.feed.entries.iter().position(|e| {
            matches!(
                &e.kind,
                EntryKind::PermissionRequest {
                    resolved: false,
                    ..
                }
            )
        }) {
            Some(idx) => idx,
            None => return,
        };

        let option_count = if let EntryKind::PermissionRequest { request, .. } =
            &self.state.feed.entries[perm_idx].kind
        {
            request.options.len()
        } else {
            return;
        };

        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if let EntryKind::PermissionRequest { selected, .. } =
                    &mut self.state.feed.entries[perm_idx].kind
                    && option_count > 0
                {
                    *selected = (*selected + 1) % option_count;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let EntryKind::PermissionRequest { selected, .. } =
                    &mut self.state.feed.entries[perm_idx].kind
                {
                    if option_count > 0 && *selected > 0 {
                        *selected -= 1;
                    } else if option_count > 0 {
                        *selected = option_count - 1;
                    }
                }
            }
            KeyCode::Enter => {
                self.resolve_permission(perm_idx, false);
            }
            KeyCode::Esc => {
                self.resolve_permission(perm_idx, true);
            }
            _ => {}
        }
    }

    fn resolve_permission(&mut self, entry_idx: usize, cancelled: bool) {
        if let EntryKind::PermissionRequest {
            request,
            response_tx,
            selected,
            resolved,
            ..
        } = &mut self.state.feed.entries[entry_idx].kind
        {
            let outcome = if cancelled {
                acp::RequestPermissionOutcome::Cancelled
            } else if let Some(opt) = request.options.get(*selected) {
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    opt.option_id.clone(),
                ))
            } else {
                acp::RequestPermissionOutcome::Cancelled
            };

            if let Some(tx) = response_tx.take() {
                let _ = tx.send(acp::RequestPermissionResponse::new(outcome));
            }
            *resolved = true;
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
                    if agent.launch.is_some() {
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
                self.state.feed = FeedState::new();
                true
            }
            None => true,
        }
    }

    // ── Input / autocomplete ────────────────────────────────────────

    fn after_input_char(&mut self) {
        // Re-parse @-mentions to update the target indicator.
        let text: String = self.state.input.lines().join("\n");
        let agent_names: Vec<String> = self.state.agents.iter().map(|a| a.name.clone()).collect();
        self.state.input_target = input::parse_mentions(&text, &agent_names);
        self.update_autocomplete();
    }

    fn update_autocomplete(&mut self) {
        let lines = self.state.input.lines();
        let (row, col) = self.state.input.cursor();
        let line = match lines.get(row) {
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
            for _ in 0..prefix_len {
                self.state
                    .input
                    .input(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
            }
            // Insert the full name + trailing space.
            for ch in name.chars() {
                self.state
                    .input
                    .input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
            }
            self.state
                .input
                .input(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        }

        self.close_autocomplete();
        self.after_input_char();
    }

    fn close_autocomplete(&mut self) {
        self.state.autocomplete = None;
    }

    fn submit_input(&mut self) {
        let raw_text: String = self.state.input.lines().join("\n");
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
                            match self.state.agents.iter().find(|a| a.launch.is_some()) {
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
                    ) && a.launch.is_some()
                })
                .map(|a| a.name.clone())
                .collect(),
        };

        if targets.is_empty() {
            self.push_system_msg("No agents to send to.");
            return;
        }

        // Push user message to feed.
        let id = self.state.feed.next_id();
        self.state.feed.entries.push(ActivityEntry {
            id,
            kind: EntryKind::UserMessage {
                text: raw_text,
                targets: targets.clone(),
            },
            timestamp: Instant::now(),
            collapsed: false,
        });

        // Clear input.
        self.state.input = TextArea::default();
        self.state.input_target = InputTarget::Default;
        self.close_autocomplete();

        // Send to each target agent.
        for agent_name in &targets {
            // Lazy-connect if needed.
            if !self.agent_connections.contains_key(agent_name) {
                self.connect_agent(agent_name);
            }
            // Reset streaming cursor for fresh response.
            self.state.feed.streaming_entry.remove(agent_name);

            // Mark as busy.
            if let Some(agent) = self.state.agents.iter_mut().find(|a| &a.name == agent_name)
                && matches!(agent.status, AgentStatus::Connected)
            {
                agent.status = AgentStatus::Busy;
            }

            // Send prompt.
            if let Some(conn) = self.agent_connections.get(agent_name) {
                let _ = conn
                    .command_tx
                    .try_send(AgentCommand::Prompt(clean_text.clone()));
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
        if self.agent_connections.contains_key(agent_id) {
            return; // Already connected or connecting.
        }

        let agent = match self.state.agents.iter_mut().find(|a| a.name == agent_id) {
            Some(a) => a,
            None => return,
        };

        let launch = match agent.launch.clone() {
            Some(l) => l,
            None => {
                self.push_system_msg(&format!(
                    "No ACP adapter found for {agent_id}. Install the adapter and ensure it's on PATH."
                ));
                return;
            }
        };

        agent.status = AgentStatus::Connecting;
        let (handle, command_tx) = spawn_agent(agent_id.to_string(), launch, self.event_tx.clone());
        self.agent_handles.insert(agent_id.to_string(), handle);
        self.agent_connections
            .insert(agent_id.to_string(), AgentConnection { command_tx });
    }

    fn disconnect_agent(&mut self, agent_id: &str) {
        // Drop the connection (closes command channel → agent thread exits).
        self.agent_connections.remove(agent_id);
        // The agent thread will send AgentDisconnected, which handles the rest.
    }

    fn rediscover_agents(&mut self) {
        let newly_discovered = discover_agents();
        for new_agent in newly_discovered {
            if !self.state.agents.iter().any(|a| a.name == new_agent.name) {
                let idx = self.state.agents.len();
                self.state.agents.push(crate::model::Agent {
                    name: new_agent.name,
                    launch: new_agent.launch,
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
        if let Some(&old_id) = self.state.feed.streaming_entry.get(agent_id)
            && let Some(idx) = self.state.feed.index_of(old_id)
        {
            match &mut self.state.feed.entries[idx].kind {
                EntryKind::AgentMessage { is_streaming, .. }
                | EntryKind::Thinking { is_streaming, .. } => *is_streaming = false,
                _ => {}
            }
        }
    }

    // ── Feed helpers ────────────────────────────────────────────────

    fn toggle_collapse_at_cursor(&mut self) {
        let cursor = match self.state.feed.cursor {
            Some(c) => c,
            None => return,
        };
        let entry = match self.state.feed.entries.get_mut(cursor) {
            Some(e) => e,
            None => return,
        };

        // Don't allow collapsing certain entry types.
        match &entry.kind {
            EntryKind::PermissionRequest { .. } | EntryKind::SystemMessage(_) => return,
            EntryKind::ToolCall { status, .. } => {
                // Only collapse completed/failed tool calls.
                if !matches!(
                    status,
                    acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed
                ) {
                    return;
                }
            }
            _ => {}
        }

        entry.collapsed = !entry.collapsed;
    }

    fn push_system_msg(&mut self, text: &str) {
        let id = self.state.feed.next_id();
        self.state.feed.entries.push(ActivityEntry {
            id,
            kind: EntryKind::SystemMessage(text.to_string()),
            timestamp: Instant::now(),
            collapsed: false,
        });
    }
}

pub async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    config: TuiConfig,
) -> Result<(), TuiError> {
    let mut events = EventHandler::new();
    let mut app = App::new(config, events.sender());

    while app.running {
        terminal.draw(|frame| ui::render(frame, &mut app.state))?;

        match events.next().await {
            Some(event) => app.handle_event(event),
            None => break,
        }
    }

    // Shutdown: drop all connections so agent threads exit cleanly.
    app.agent_connections.clear();
    for (_, handle) in app.agent_handles.drain() {
        let _ = handle.join();
    }

    Ok(())
}
