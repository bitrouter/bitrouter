//! Agent session metadata returned after connecting.

/// Information about an established agent session.
///
/// Returned by [`AgentProvider::connect`](super::provider::AgentProvider::connect)
/// after the agent subprocess or remote endpoint is ready.
#[derive(Debug, Clone)]
pub struct AgentSessionInfo {
    /// Protocol-assigned session identifier (ACP session ID, A2A task ID, etc.).
    pub session_id: String,
    /// Human-readable agent name.
    pub agent_name: String,
    /// Capabilities the agent declared during handshake.
    pub capabilities: AgentCapabilities,
}

/// Feature flags declared by an agent during connection.
#[derive(Debug, Clone, Default)]
pub struct AgentCapabilities {
    /// Whether the agent may send permission requests.
    pub supports_permissions: bool,
    /// Whether the agent emits thinking/reasoning trace chunks.
    pub supports_thinking: bool,
}
