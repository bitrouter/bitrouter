//! Persisted session records and offline checkpoint inspection.
//! Scheduling and execution transitions are implemented by core.

use bitrouter_ai::core::protocol::{Limits, TaskInput};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedRootInput {
    pub operation_id: String,
    pub run_id: String,
    pub agent_turn_id: String,
    pub input: TaskInput,
    pub limits: Limits,
    /// Harness requirements can grow after acceptance without changing the
    /// caller's bounded input envelope or its original operation fingerprint.
    pub required_materials: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RootQueue {
    pub pending: VecDeque<QueuedRootInput>,
    pub paused: bool,
}
