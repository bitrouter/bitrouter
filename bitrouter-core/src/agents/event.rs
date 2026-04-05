//! Protocol-neutral agent session event types.
//!
//! These types describe the lifecycle of an interactive agent session:
//! connect, submit prompts, receive streamed events, handle permission
//! requests, and disconnect. They are transport-agnostic — protocol
//! adapters (ACP, A2A) convert their wire types into these.

/// Opaque identifier for a pending permission request.
///
/// Used to correlate a [`PermissionRequest`] with the
/// [`AgentProvider::respond_permission`](super::provider::AgentProvider::respond_permission)
/// call that resolves it.
pub type PermissionRequestId = u64;

/// Events emitted by an agent session during a prompt turn.
///
/// Delivered through the `tokio::sync::mpsc::Receiver` returned by
/// [`AgentProvider::submit`](super::provider::AgentProvider::submit).
/// The stream ends when the receiver is closed (after [`TurnDone`],
/// [`Error`], or [`Disconnected`]).
///
/// Unlike the provider-level types these replace, events do **not**
/// carry an `agent_id` — the consumer already knows which agent a
/// receiver belongs to.
///
/// [`TurnDone`]: AgentEvent::TurnDone
/// [`Error`]: AgentEvent::Error
/// [`Disconnected`]: AgentEvent::Disconnected
#[derive(Debug)]
pub enum AgentEvent {
    /// Agent connection closed cleanly.
    Disconnected,
    /// Agent-side error.
    Error { message: String },
    /// Streamed text chunk from the agent.
    MessageChunk { text: String },
    /// Non-text content from the agent (image, audio, resource link, etc.).
    NonTextContent { description: String },
    /// Agent thinking / reasoning trace chunk.
    ThoughtChunk { text: String },
    /// Agent started a tool call.
    ToolCall {
        tool_call_id: String,
        title: String,
        status: ToolCallStatus,
    },
    /// Update to an in-progress tool call.
    ToolCallUpdate {
        tool_call_id: String,
        title: Option<String>,
        status: Option<ToolCallStatus>,
    },
    /// Agent requests user permission for an action.
    ///
    /// The consumer resolves this by calling
    /// [`AgentProvider::respond_permission`](super::provider::AgentProvider::respond_permission)
    /// with the `id` from this event.
    PermissionRequest {
        id: PermissionRequestId,
        request: PermissionRequest,
    },
    /// The prompt turn completed.
    TurnDone { stop_reason: StopReason },
}

/// Status of an agent tool call.
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

// Compile-time assertions: all public types must be Send.
const _: () = {
    const fn _assert<T: Send>() {}
    _assert::<AgentEvent>();
    _assert::<PermissionRequest>();
    _assert::<PermissionResponse>();
    _assert::<ToolCallStatus>();
    _assert::<StopReason>();
};
