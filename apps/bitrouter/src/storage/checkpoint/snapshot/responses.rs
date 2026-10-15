//! Persisted session records and offline checkpoint inspection.
//! Scheduling and execution transitions are implemented by core.

use super::RunStatus;
use bitrouter_ai::core::checkpoint::DurableEvent;
use bitrouter_ai::core::protocol::{TaskInput, ToolExecute, ToolResult};
use bitrouter_ai::types::Message;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseState {
    pub latest: Option<String>,
    pub exchanges: BTreeMap<String, ResponseExchange>,
}

/// Retained core data for projection by a Responses adapter. Provider metadata
/// is not a public wire item and must never be relabeled as encrypted state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseOutput {
    pub event_seq: u64,
    pub agent_id: String,
    pub agent_name: String,
    pub agent_turn_id: String,
    pub step_id: String,
    pub message: Message,
    #[serde(default)]
    pub call_ids: BTreeMap<String, String>,
}

/// Replayable attributed core collaboration event; never provider opaque state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseEvent {
    pub agent_name: String,
    pub event: DurableEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseExchange {
    pub response_id: String,
    pub operation_id: String,
    pub run_id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub created_at: Option<u64>,
    #[serde(default)]
    pub input: Option<TaskInput>,
    #[serde(default)]
    pub final_answer: Option<String>,
    pub previous_response_id: Option<String>,
    pub created_state_revision: u64,
    pub completed_state_revision: Option<u64>,
    pub run_status: Option<RunStatus>,
    pub output: Vec<ResponseOutput>,
    #[serde(default)]
    pub events: Vec<ResponseEvent>,
    /// Public call IDs map to the exact retained invocation/attempt. This is
    /// the exchange's terminal view, not a mutable view of later tool results.
    pub pending: BTreeMap<String, ToolExecute>,
}

/// A client function output names the same durable operation on both transports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseToolResult {
    pub operation_id: String,
    pub call_id: String,
    pub result: ToolResult,
}

impl ResponseState {
    pub fn is_empty(&self) -> bool {
        self.latest.is_none() && self.exchanges.is_empty()
    }
}
