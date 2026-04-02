//! Protocol-neutral agent types.
//!
//! These types abstract over ACP specifics so consumers (like the TUI)
//! don't depend on `agent-client-protocol` directly.
//!
//! Candidates for promotion to `bitrouter-core` once the `AgentProvider`
//! trait is defined.

use std::path::PathBuf;

/// Events emitted by an agent connection to its consumer.
///
/// All variants are `Send` — ACP-specific types are translated before
/// crossing the thread boundary.
#[derive(Debug)]
pub enum AgentEvent {
    /// Agent subprocess connected and session created.
    Connected {
        agent_id: String,
        session_id: String,
    },
    /// Agent connection closed cleanly.
    Disconnected { agent_id: String },
    /// Agent-side error.
    Error { agent_id: String, message: String },
    /// Streamed text chunk from the agent.
    MessageChunk { agent_id: String, text: String },
    /// Non-text content from the agent (image, audio, resource link, etc.).
    NonTextContent {
        agent_id: String,
        description: String,
    },
    /// Agent thinking / reasoning trace chunk.
    ThoughtChunk { agent_id: String, text: String },
    /// Agent started a tool call.
    ToolCall {
        agent_id: String,
        tool_call_id: String,
        title: String,
        status: ToolCallStatus,
    },
    /// Update to an in-progress tool call.
    ToolCallUpdate {
        agent_id: String,
        tool_call_id: String,
        title: Option<String>,
        status: Option<ToolCallStatus>,
    },
    /// Agent requests user permission for an action.
    PermissionRequest {
        agent_id: String,
        request: PermissionRequest,
        response_tx: tokio::sync::oneshot::Sender<PermissionResponse>,
    },
    /// The prompt turn completed.
    PromptDone {
        agent_id: String,
        stop_reason: StopReason,
    },
}

/// Status of a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// Reason a prompt turn completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    Other(String),
}

/// A permission request from an agent.
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    /// Display title for the permission dialog.
    pub title: String,
    /// Detailed description of what the agent wants to do.
    pub description: String,
    /// Available choices for the user.
    pub options: Vec<PermissionOption>,
}

/// A single option in a permission request.
#[derive(Debug, Clone)]
pub struct PermissionOption {
    /// Unique identifier for this option.
    pub id: String,
    /// Human-readable label.
    pub title: String,
    /// Optional description.
    pub description: String,
}

/// User's response to a permission request.
#[derive(Debug, Clone)]
pub struct PermissionResponse {
    pub outcome: PermissionOutcome,
}

/// Outcome of a permission request.
#[derive(Debug, Clone)]
pub enum PermissionOutcome {
    /// User selected an option.
    Allowed { selected_option: String },
    /// User denied the request.
    Denied,
}

/// Commands that can be sent to an agent.
pub enum AgentCommand {
    Prompt(String),
}

/// An agent binary discovered on PATH (not yet confirmed in config).
#[derive(Debug, Clone)]
pub struct DiscoveredAgent {
    pub name: String,
    pub binary: PathBuf,
    pub args: Vec<String>,
}

// Compile-time assertions: all public types must be Send.
const _: () = {
    const fn _assert<T: Send>() {}
    _assert::<AgentEvent>();
    _assert::<AgentCommand>();
    _assert::<DiscoveredAgent>();
    _assert::<PermissionRequest>();
    _assert::<PermissionResponse>();
};
