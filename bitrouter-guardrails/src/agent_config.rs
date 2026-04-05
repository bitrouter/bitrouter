use serde::{Deserialize, Serialize};

/// Agent guardrail configuration, embedded under `guardrails.agents`.
///
/// Currently a structural placeholder — no inspection logic is implemented.
/// The `enabled` flag exists to support future zero-cost pass-through.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentGuardrailConfig {
    /// Master switch. When `false` the agent guardrail is a no-op.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Default for AgentGuardrailConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}
