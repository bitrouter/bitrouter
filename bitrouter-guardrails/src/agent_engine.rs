use crate::agent_config::AgentGuardrailConfig;

/// The agent guardrail engine.
///
/// Structural placeholder parallel to [`Guardrail`](crate::engine::Guardrail)
/// and [`ToolGuardrail`](crate::tool_engine::ToolGuardrail). No content
/// inspection is implemented — the engine exists so that
/// [`GuardedAgentRouter`](crate::agent_router::GuardedAgentRouter) can follow
/// the same hot-reload pattern as the tool guardrail layer.
pub struct AgentGuardrail {
    config: AgentGuardrailConfig,
}

impl AgentGuardrail {
    /// Create a new agent guardrail engine from the given configuration.
    pub fn new(config: AgentGuardrailConfig) -> Self {
        Self { config }
    }

    /// Returns `true` when the agent guardrail is disabled and will skip
    /// all checks.
    pub fn is_disabled(&self) -> bool {
        !self.config.enabled
    }
}
