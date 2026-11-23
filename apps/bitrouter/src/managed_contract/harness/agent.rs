//! Application-owned agent configuration and tool permissions.

use bitrouter_ai::types::ReasoningEffort;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The tool permissions selected for a turn.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolMode {
    #[default]
    Coding,
    ReadOnly,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct EstimateRates {
    /// Estimated micro-USD per million prompt tokens.
    pub prompt: u64,
    /// Estimated micro-USD per million completion tokens.
    pub completion: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub model: String,
    #[serde(default, skip_serializing_if = "fixed_model_mode")]
    pub model_mode: ModelMode,
    pub effort: Option<ReasoningEffort>,
    /// Per-step output reservation, retained with the Thread configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Explicit scoped constraints retained with this native Thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<crate::managed_contract::harness::skills::SkillConstraints>,
    pub instructions: String,
    pub max_steps: u32,
    pub max_tool_calls: u32,
    pub max_duration: Duration,
    pub max_context_bytes: usize,
    pub max_spend_microusd: Option<u64>,
    pub estimate_rates: Option<EstimateRates>,
    pub tool_mode: ToolMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Completed,
    Failed,
    Cancelled,
    BoundExceeded,
}

fn fixed_model_mode(mode: &ModelMode) -> bool {
    *mode == ModelMode::Fixed
}

redacted_contract_debug!(AgentConfig);

/// Select a fixed model or the configured routing Policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelMode {
    #[default]
    Fixed,
    Policy,
}
