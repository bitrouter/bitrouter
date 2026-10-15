//! Persisted session records and offline checkpoint inspection.
//! Scheduling and execution transitions are implemented by core.

use crate::gateway::pipeline::native_projection::ProjectionRecord;
use bitrouter_ai::core::protocol::CoreError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebuildRecord {
    pub strategy: String,
    pub source_context_revision: u64,
    pub source_history_sha256: String,
    pub rebuilt_step_id: Option<String>,
    pub removed_history_messages: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_settled_history_messages: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portable_reasoning: Option<PortableReasoningRecord>,
    pub elapsed_ms: u64,
    pub error: Option<CoreError>,
}

/// Both the retained execution history and the exact prepared view are bound
/// to their immutable sources in the rejected step and evidence store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortableReasoningRecord {
    pub history: ProjectionRecord,
    pub prompt: ProjectionRecord,
}
