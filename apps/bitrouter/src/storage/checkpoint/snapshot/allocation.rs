//! Persisted session records and offline checkpoint inspection.
//! Scheduling and execution transitions are implemented by core.

use super::collaboration::Work;
use bitrouter_ai::core::protocol::{CoreError, MaterialRef, TaskInput};
use serde::{Deserialize, Serialize};

/// Facts on which retained history depends. Keep all distinct sources across
/// turns: a later model step cannot certify older evidence as newly observed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextSource {
    pub permission_revision: u64,
    pub workspace_revision: Option<String>,
    pub tool_manifest_digest: String,
    pub materials: Vec<MaterialRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextKind {
    Reuse,
    Fresh,
    Inherited,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextCandidate {
    pub candidate_id: String,
    pub kind: ContextKind,
    pub agent_id: Option<String>,
    pub context_revision: Option<u64>,
    /// Empty means eligible for allocation. Model feasibility remains a
    /// separate, mandatory gate before any provider attempt.
    pub rejection_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextAllocation {
    pub allocation_id: String,
    pub policy_id: String,
    pub source: String,
    pub input_state_revision: u64,
    pub signal_revision: u64,
    pub actor_id: String,
    pub actor_context_revision: u64,
    pub assignment_id: String,
    pub task: Work,
    pub input: TaskInput,
    pub candidates: Vec<ContextCandidate>,
    pub selected_candidate_id: Option<String>,
    pub selected_agent_id: Option<String>,
    pub error: Option<CoreError>,
    pub applied_state_revision: Option<u64>,
    pub application_error: Option<CoreError>,
}
