//! Public harness agent contracts. Execution and storage remain application-owned.

use crate::types::ReasoningEffort;
use serde::{Deserialize, Serialize};
use std::time::Duration;

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
    pub model_mode: crate::core::protocol::ModelMode,
    pub effort: Option<ReasoningEffort>,
    /// Per-step output reservation, retained with the Thread configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    pub instructions: String,
    pub max_steps: u32,
    pub max_tool_calls: u32,
    pub max_duration: Duration,
    pub max_context_bytes: usize,
    pub max_spend_microusd: Option<u64>,
    pub estimate_rates: Option<EstimateRates>,
    pub tool_mode: ToolMode,
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolMode {
    #[default]
    Coding,
    ReadOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Completed,
    Failed,
    Cancelled,
    BoundExceeded,
}

fn fixed_model_mode(mode: &crate::core::protocol::ModelMode) -> bool {
    *mode == crate::core::protocol::ModelMode::Fixed
}

redacted_contract_debug!(AgentConfig);
