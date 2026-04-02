use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use agent_client_protocol as acp;
use ratatui::style::Color;

use crate::acp::discovery::AgentLaunch;

// ── Agent color palette ─────────────────────────────────────────────────

const AGENT_PALETTE: &[Color] = &[
    Color::Green,
    Color::Cyan,
    Color::Yellow,
    Color::Magenta,
    Color::Blue,
    Color::LightRed,
    Color::LightGreen,
    Color::LightCyan,
];

/// Pick a color from the palette by index.
pub fn agent_color(index: usize) -> Color {
    AGENT_PALETTE[index % AGENT_PALETTE.len()]
}

// ── Agent ───────────────────────────────────────────────────────────────

/// An agent harness that can be connected via ACP.
#[derive(Debug, Clone)]
pub struct Agent {
    pub name: String,
    pub launch: Option<AgentLaunch>,
    pub status: AgentStatus,
    pub session_id: Option<acp::SessionId>,
    pub color: Color,
}

/// Connection lifecycle of an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    /// Discovered on PATH, not connected.
    Idle,
    /// Thread spawned, awaiting `AgentConnected`.
    Connecting,
    /// Session active, ready for prompts.
    Connected,
    /// Processing a prompt (awaiting `PromptDone`).
    Busy,
    /// Connection failed or crashed.
    Error(String),
}

// ── Activity feed ───────────────────────────────────────────────────────

/// Monotonic entry identifier.
pub type EntryId = u64;

/// A single entry in the unified activity feed.
pub struct ActivityEntry {
    pub id: EntryId,
    pub kind: EntryKind,
    pub timestamp: Instant,
    /// Whether this entry is visually collapsed in the feed.
    pub collapsed: bool,
}

/// The payload of an activity entry.
pub enum EntryKind {
    /// A message sent by the user.
    UserMessage { text: String, targets: Vec<String> },
    /// A streamed response from an agent.
    AgentMessage {
        agent_id: String,
        blocks: Vec<ContentBlock>,
        is_streaming: bool,
    },
    /// A tool invocation by an agent.
    ToolCall {
        agent_id: String,
        tool_call_id: acp::ToolCallId,
        title: String,
        status: acp::ToolCallStatus,
    },
    /// Agent thinking / reasoning trace.
    Thinking {
        agent_id: String,
        text: String,
        is_streaming: bool,
    },
    /// Agent requesting user permission for a tool call.
    PermissionRequest {
        agent_id: String,
        request: Box<acp::RequestPermissionRequest>,
        response_tx: Option<tokio::sync::oneshot::Sender<acp::RequestPermissionResponse>>,
        selected: usize,
        resolved: bool,
    },
    /// System-level notice (errors, connect/disconnect).
    SystemMessage(String),
}

/// A content block inside an agent message.
#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text(String),
    Other(String),
}

// ── Feed state ──────────────────────────────────────────────────────────

/// State of the unified activity feed.
pub struct FeedState {
    pub entries: Vec<ActivityEntry>,
    pub scroll_offset: usize,
    /// Which entry has keyboard focus when in Feed mode.
    pub cursor: Option<usize>,
    /// Recomputed each render pass for scroll clamping.
    pub total_rendered_lines: usize,
    /// Counter for generating unique `EntryId`s.
    pub next_entry_id: u64,
    /// Per-agent streaming cursor: maps agent_id → EntryId of the entry
    /// currently being streamed into.
    pub streaming_entry: HashMap<String, EntryId>,
}

impl FeedState {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            scroll_offset: 0,
            cursor: None,
            total_rendered_lines: 0,
            next_entry_id: 0,
            streaming_entry: HashMap::new(),
        }
    }

    /// Allocate a new `EntryId`.
    pub fn next_id(&mut self) -> EntryId {
        let id = self.next_entry_id;
        self.next_entry_id += 1;
        id
    }

    /// Find the index of an entry by its `EntryId`.
    pub fn index_of(&self, id: EntryId) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }
}

// ── Input ───────────────────────────────────────────────────────────────

/// Resolved target(s) for the current input message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputTarget {
    /// No @-mention — route to default agent.
    Default,
    /// One or more specific agents.
    Specific(Vec<String>),
    /// Broadcast to all connected agents.
    All,
}

/// Autocomplete state for @-mentions in the input bar.
pub struct AutocompleteState {
    /// The prefix typed after `@` (e.g. `"cl"` for `@cl`).
    pub prefix: String,
    /// Matching agent names (filtered).
    pub candidates: Vec<String>,
    /// Index of the highlighted candidate.
    pub selected: usize,
}

// ── Modals ──────────────────────────────────────────────────────────────

/// At most one modal overlay is open at a time.
pub enum Modal {
    AgentManager(AgentManagerState),
    Observability(ObservabilityState),
    CommandPalette(CommandPaletteState),
    Help,
}

pub struct AgentManagerState {
    pub selected: usize,
}

pub struct ObservabilityState {
    pub scroll_offset: usize,
}

/// A recorded event for the observability panel.
pub struct ObsEvent {
    pub agent_id: String,
    pub kind: ObsEventKind,
    pub timestamp: Instant,
}

pub enum ObsEventKind {
    Connected,
    Disconnected,
    PromptSent,
    PromptDone,
    ToolCall { title: String },
    Error { message: String },
}

/// Capped observability event log.
pub struct ObsLog {
    pub events: VecDeque<ObsEvent>,
}

impl ObsLog {
    const MAX_EVENTS: usize = 1000;

    pub fn new() -> Self {
        Self {
            events: VecDeque::new(),
        }
    }

    pub fn push(&mut self, event: ObsEvent) {
        if self.events.len() >= Self::MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }
}

// ── Command palette ─────────────────────────────────────────────────────

pub struct CommandPaletteState {
    pub query: String,
    pub all_commands: Vec<PaletteCommand>,
    /// Indices into `all_commands` that match the current query.
    pub filtered: Vec<usize>,
    pub selected: usize,
}

pub struct PaletteCommand {
    pub label: String,
    pub action: CommandAction,
}

#[derive(Debug, Clone)]
pub enum CommandAction {
    ConnectAgent(String),
    DisconnectAgent(String),
    SetDefaultAgent(String),
    ToggleObservability,
    ClearConversation,
    ShowHelp,
}
