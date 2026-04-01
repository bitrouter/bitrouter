use std::io::Stdout;

use agent_client_protocol as acp;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::ListState;
use ratatui_textarea::TextArea;
use tokio::sync::mpsc;

use crate::TuiConfig;
use crate::acp::connection::{AgentCommand, AgentConnection, spawn_agent};
use crate::acp::discovery::discover_agents;
use crate::error::TuiError;
use crate::event::{AppEvent, EventHandler};
use crate::model::{
    Agent, AgentStatus, PendingPermission, RenderedBlock, RenderedMessage, RenderedRole,
};
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
    pub agents: Vec<Agent>,
    pub list_state: ListState,
}

/// Conversation panel state.
pub struct ConversationState {
    pub session_id: Option<acp::SessionId>,
    pub agent_name: Option<String>,
    pub messages: Vec<RenderedMessage>,
    pub scroll_offset: usize,
    /// Recomputed each render pass so scroll clamping is accurate.
    pub total_lines: usize,
    /// Index of the current agent message being streamed (into `messages`).
    pub streaming_msg_idx: Option<usize>,
    /// Pending permission request from the agent.
    pub pending_permission: Option<PendingPermission>,
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
    /// Handle to the running agent connection (lazy-spawned on first prompt).
    agent_conn: Option<AgentConnection>,
    /// JoinHandle for the agent thread.
    agent_handle: Option<std::thread::JoinHandle<()>>,
    /// Cloned event sender for spawning agent connections.
    event_tx: mpsc::Sender<AppEvent>,
}

impl App {
    pub fn new(config: TuiConfig, event_tx: mpsc::Sender<AppEvent>) -> Self {
        let agents = discover_agents();
        let mut sidebar = SidebarState {
            agents,
            list_state: ListState::default(),
        };
        if !sidebar.agents.is_empty() {
            sidebar.list_state.select(Some(0));
        }

        Self {
            running: true,
            state: AppState {
                focus: Focus::Input,
                tab: Tab::Conversation,
                sidebar,
                conversation: ConversationState {
                    session_id: None,
                    agent_name: None,
                    messages: Vec::new(),
                    scroll_offset: 0,
                    total_lines: 0,
                    streaming_msg_idx: None,
                    pending_permission: None,
                },
                logs: LogsState { lines: Vec::new() },
                input: TextArea::default(),
                config,
            },
            agent_conn: None,
            agent_handle: None,
            event_tx,
        }
    }

    fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Key(key) => self.handle_key(key),
            AppEvent::Resize { .. } | AppEvent::Tick => {}
            AppEvent::AgentConnected { name } => self.handle_agent_connected(name),
            AppEvent::AgentError { name, message } => self.handle_agent_error(name, message),
            AppEvent::SessionUpdate(notif) => self.handle_session_update(notif),
            AppEvent::PermissionRequest {
                request,
                response_tx,
            } => self.handle_permission_request(request, response_tx),
            AppEvent::PromptDone { _stop_reason: _ } => self.handle_prompt_done(),
        }
    }

    // ── ACP event handlers ──────────────────────────────────────────────

    fn handle_agent_connected(&mut self, name: String) {
        // Update sidebar status
        if let Some(agent) = self
            .state
            .sidebar
            .agents
            .iter_mut()
            .find(|a| a.name == name)
        {
            agent.status = AgentStatus::Running;
        }
        // Reset conversation state for the new session
        self.state.conversation.agent_name = Some(name);
        self.state.conversation.streaming_msg_idx = None;
        self.state.conversation.session_id = None;
    }

    fn handle_agent_error(&mut self, name: String, message: String) {
        if let Some(agent) = self
            .state
            .sidebar
            .agents
            .iter_mut()
            .find(|a| a.name == name)
        {
            agent.status = AgentStatus::Error(message.clone());
        }
        // Show error in conversation as a system message
        self.state.conversation.messages.push(RenderedMessage {
            role: RenderedRole::System,
            blocks: vec![RenderedBlock::Text(format!("Error: {message}"))],
            is_streaming: false,
        });
        self.state.conversation.streaming_msg_idx = None;
        // Clear the dead connection so the next prompt triggers a fresh spawn
        self.agent_conn = None;
    }

    fn handle_session_update(&mut self, notif: acp::SessionNotification) {
        // Store session ID if we don't have one yet
        if self.state.conversation.session_id.is_none() {
            self.state.conversation.session_id = Some(notif.session_id.clone());
        }

        match notif.update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                self.apply_agent_message_chunk(chunk);
            }
            acp::SessionUpdate::AgentThoughtChunk(chunk) => {
                // Render thoughts as dimmed agent text
                self.apply_agent_message_chunk(chunk);
            }
            acp::SessionUpdate::ToolCall(tool_call) => {
                self.apply_tool_call(tool_call);
            }
            acp::SessionUpdate::ToolCallUpdate(update) => {
                self.apply_tool_call_update(update);
            }
            _ => {
                // Plan, AvailableCommandsUpdate, CurrentModeUpdate, etc. — ignore for now
            }
        }
    }

    fn apply_agent_message_chunk(&mut self, chunk: acp::ContentChunk) {
        let text = match chunk.content {
            acp::ContentBlock::Text(tc) => tc.text,
            acp::ContentBlock::Image(_) => "<image>".to_string(),
            acp::ContentBlock::Audio(_) => "<audio>".to_string(),
            acp::ContentBlock::ResourceLink(rl) => format!("[{}]({})", rl.name, rl.uri),
            acp::ContentBlock::Resource(_) => "<resource>".to_string(),
            _ => "<unknown>".to_string(),
        };

        let messages = &mut self.state.conversation.messages;

        // If we have a streaming agent message, extend it
        if let Some(idx) = self.state.conversation.streaming_msg_idx
            && let Some(msg) = messages.get_mut(idx)
        {
            // Try to extend the last text block
            if let Some(RenderedBlock::Text(existing)) = msg.blocks.last_mut() {
                existing.push_str(&text);
            } else {
                msg.blocks.push(RenderedBlock::Text(text));
            }
            return;
        }

        // Start a new agent message
        let idx = messages.len();
        messages.push(RenderedMessage {
            role: RenderedRole::Agent,
            blocks: vec![RenderedBlock::Text(text)],
            is_streaming: true,
        });
        self.state.conversation.streaming_msg_idx = Some(idx);
    }

    fn apply_tool_call(&mut self, tool_call: acp::ToolCall) {
        let messages = &mut self.state.conversation.messages;

        // Ensure there's a current agent message to attach the tool call to
        let idx = if let Some(idx) = self.state.conversation.streaming_msg_idx {
            idx
        } else {
            let idx = messages.len();
            messages.push(RenderedMessage {
                role: RenderedRole::Agent,
                blocks: Vec::new(),
                is_streaming: true,
            });
            self.state.conversation.streaming_msg_idx = Some(idx);
            idx
        };

        if let Some(msg) = messages.get_mut(idx) {
            msg.blocks.push(RenderedBlock::ToolCall {
                tool_call_id: tool_call.tool_call_id,
                title: tool_call.title,
                status: tool_call.status,
            });
        }
    }

    fn apply_tool_call_update(&mut self, update: acp::ToolCallUpdate) {
        let messages = &mut self.state.conversation.messages;

        // Find the tool call block by ID and update it
        for msg in messages.iter_mut().rev() {
            for block in &mut msg.blocks {
                if let RenderedBlock::ToolCall {
                    tool_call_id,
                    title,
                    status,
                } = block
                    && *tool_call_id == update.tool_call_id
                {
                    if let Some(new_title) = &update.fields.title {
                        *title = new_title.clone();
                    }
                    if let Some(new_status) = update.fields.status {
                        *status = new_status;
                    }
                    return;
                }
            }
        }

        // If we didn't find the tool call, try to create one from the update
        if let Ok(tool_call) = acp::ToolCall::try_from(update) {
            self.apply_tool_call(tool_call);
        }
    }

    fn handle_permission_request(
        &mut self,
        request: acp::RequestPermissionRequest,
        response_tx: tokio::sync::oneshot::Sender<acp::RequestPermissionResponse>,
    ) {
        self.state.conversation.pending_permission = Some(PendingPermission {
            request,
            response_tx,
            selected: 0,
        });
        // Switch focus to conversation so user can see and respond to the prompt
        self.state.focus = Focus::Conversation;
    }

    fn handle_prompt_done(&mut self) {
        // Mark the current streaming message as complete
        if let Some(idx) = self.state.conversation.streaming_msg_idx.take()
            && let Some(msg) = self.state.conversation.messages.get_mut(idx)
        {
            msg.is_streaming = false;
        }
    }

    // ── Key handlers ────────────────────────────────────────────────────

    fn handle_key(&mut self, key: KeyEvent) {
        // Global quit: Ctrl-C always exits.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.running = false;
            return;
        }

        // If a permission prompt is active, handle it specially
        if self.state.conversation.pending_permission.is_some() {
            self.handle_permission_key(key);
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

    fn handle_permission_key(&mut self, key: KeyEvent) {
        let perm = match self.state.conversation.pending_permission.as_mut() {
            Some(p) => p,
            None => return,
        };
        let option_count = perm.request.options.len();

        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if option_count > 0 {
                    perm.selected = (perm.selected + 1) % option_count;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if option_count > 0 {
                    perm.selected = perm.selected.checked_sub(1).unwrap_or(option_count - 1);
                }
            }
            KeyCode::Enter => {
                self.resolve_permission(false);
            }
            KeyCode::Esc => {
                self.resolve_permission(true);
            }
            _ => {}
        }
    }

    fn resolve_permission(&mut self, cancelled: bool) {
        if let Some(perm) = self.state.conversation.pending_permission.take() {
            let outcome = if cancelled {
                acp::RequestPermissionOutcome::Cancelled
            } else if let Some(opt) = perm.request.options.get(perm.selected) {
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    opt.option_id.clone(),
                ))
            } else {
                acp::RequestPermissionOutcome::Cancelled
            };

            let _ = perm
                .response_tx
                .send(acp::RequestPermissionResponse::new(outcome));
        }
    }

    fn submit_input(&mut self) {
        let text: String = self.state.input.lines().join("\n");
        if text.trim().is_empty() {
            return;
        }

        // Add user message to conversation
        self.state.conversation.messages.push(RenderedMessage {
            role: RenderedRole::User,
            blocks: vec![RenderedBlock::Text(text.clone())],
            is_streaming: false,
        });

        // Reset streaming index for next agent response
        self.state.conversation.streaming_msg_idx = None;

        // Clear input
        self.state.input = TextArea::default();

        // Lazy-spawn agent connection if not yet running
        if self.agent_conn.is_none() {
            let Some(selected) = self.state.sidebar.list_state.selected() else {
                self.push_system_msg(
                    "Select an agent from the sidebar first (Tab → j/k → Tab back).",
                );
                return;
            };

            if let Some(agent) = self.state.sidebar.agents.get_mut(selected) {
                if let Some(launch) = agent.launch.clone() {
                    agent.status = AgentStatus::Connecting;
                    let (handle, command_tx) =
                        spawn_agent(agent.name.clone(), launch, self.event_tx.clone());
                    self.agent_handle = Some(handle);
                    self.agent_conn = Some(AgentConnection { command_tx });
                } else {
                    self.push_system_msg("No ACP adapter found for this agent. Install the adapter (e.g. npm i -g @agentclientprotocol/claude-agent-acp) and ensure it's on PATH.");
                    return;
                }
            } else {
                self.push_system_msg(
                    "No agents discovered. Install an ACP agent and ensure it's on PATH.",
                );
                return;
            }
        }

        // Send prompt to agent
        if let Some(conn) = &self.agent_conn
            && conn
                .command_tx
                .try_send(AgentCommand::Prompt(text))
                .is_err()
        {
            self.push_system_msg("Agent is busy. Please wait for the current response.");
        }
    }

    fn push_system_msg(&mut self, text: &str) {
        self.state.conversation.messages.push(RenderedMessage {
            role: RenderedRole::System,
            blocks: vec![RenderedBlock::Text(text.to_string())],
            is_streaming: false,
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
            None => break, // event channel closed (terminal detached)
        }
    }

    // Shutdown: drop the command channel first so the agent task exits cleanly,
    // then join the agent thread.
    drop(app.agent_conn.take());
    if let Some(handle) = app.agent_handle.take() {
        let _ = handle.join();
    }

    Ok(())
}
