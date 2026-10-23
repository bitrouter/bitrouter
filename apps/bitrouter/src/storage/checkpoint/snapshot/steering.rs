//! Persisted session records and offline checkpoint inspection.
//! Scheduling and execution transitions are implemented by core.

use bitrouter_ai::core::checkpoint::ToolStartFence;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteeringDisposition {
    Received,
    Applied,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SteeringRecord {
    pub operation_id: String,
    pub run_id: String,
    pub agent_id: String,
    pub agent_turn_id: String,
    pub text: String,
    pub received_state_revision: u64,
    pub disposition: SteeringDisposition,
    pub resolved_state_revision: Option<u64>,
    pub tool_start_fences: Vec<ToolStartFence>,
}
