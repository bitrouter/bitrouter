//! Persisted attempt observations shared by gateway accounting and learning.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionHop {
    pub provider: String,
    pub model: String,
    pub account: Option<String>,
    pub protocol: String,
    pub status: String,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutcome {
    Completed,
    Failed,
    ClientDisconnected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionSettlement {
    pub settled_at: String,
    pub hops: Vec<ExecutionHop>,
    pub final_model: String,
    pub final_provider: String,
    pub error_code: Option<String>,
    pub duration_ms: u64,
    /// Filled by the terminal observer, including disconnects without an error.
    pub outcome: Option<ExecutionOutcome>,
    /// Metering's final observation; multiple attempts may have additional cost.
    pub final_metered_cost_micro_usd: Option<u64>,
    /// Complete request cost only when every attempted generation is covered.
    pub total_cost_micro_usd: Option<u64>,
}
