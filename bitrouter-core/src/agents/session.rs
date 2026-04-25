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
///
/// `supports_permissions` and `supports_thinking` are bitrouter-side
/// assumptions — the ACP wire flags for these don't exist directly.
/// `load_session` / `prompt_image` / `prompt_audio` are populated from
/// the agent's `initialize` response (`agentCapabilities.loadSession`,
/// `agentCapabilities.promptCapabilities.image`, `.audio`); they
/// default to `false` per the ACP spec.
#[derive(Debug, Clone, Default)]
pub struct AgentCapabilities {
    /// Whether the agent may send permission requests.
    pub supports_permissions: bool,
    /// Whether the agent emits thinking/reasoning trace chunks.
    pub supports_thinking: bool,
    /// Whether the agent supports `session/load` for replay-based
    /// import. Read from `agentCapabilities.loadSession` in the
    /// `initialize` response.
    pub load_session: bool,
    /// Whether the agent accepts image content blocks in prompts.
    /// From `agentCapabilities.promptCapabilities.image`.
    pub prompt_image: bool,
    /// Whether the agent accepts audio content blocks in prompts.
    /// From `agentCapabilities.promptCapabilities.audio`.
    pub prompt_audio: bool,
}
