use agent_client_protocol as acp;

use crate::acp::discovery::AgentLaunch;

/// An agent harness that can be connected via ACP.
#[derive(Debug, Clone)]
pub struct Agent {
    pub name: String,
    pub launch: Option<AgentLaunch>,
    pub status: AgentStatus,
}

/// Connection status of an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Connecting,
    Running,
    Error(String),
}

/// Display-ready role for a rendered message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderedRole {
    User,
    Agent,
    System,
}

/// A single display-ready content block.
#[derive(Debug, Clone)]
pub enum RenderedBlock {
    Text(String),
    ToolCall {
        tool_call_id: acp::ToolCallId,
        title: String,
        status: acp::ToolCallStatus,
    },
}

/// A fully assembled message for display in the conversation panel.
///
/// Streaming `AgentMessageChunk` events are assembled into a single
/// `RenderedMessage` that is extended as new chunks arrive.
#[derive(Debug, Clone)]
pub struct RenderedMessage {
    pub role: RenderedRole,
    pub blocks: Vec<RenderedBlock>,
    pub is_streaming: bool,
}

/// A pending permission request from the agent, waiting for user input.
pub struct PendingPermission {
    pub request: acp::RequestPermissionRequest,
    pub response_tx: tokio::sync::oneshot::Sender<acp::RequestPermissionResponse>,
    /// Index into `request.options` for the currently highlighted option.
    pub selected: usize,
}
