//! Task-specific views over immutable evidence. A view changes a prompt, never
//! the source conversation. Decision output is advisory to this compiler.

pub mod evidence;
pub mod inspection;
pub mod models;
pub mod planner;
pub(crate) mod tasks;
pub mod tools;
pub(crate) mod validation;

use std::collections::BTreeMap;

use bitrouter_sdk::decision_model::policy::{DecisionPolicy, DecisionPricing};
use bitrouter_sdk::decision_model::types::{DecisionError, DecisionRequest, DecisionResponse};
use bitrouter_sdk::language_model::native::NativePlan;
use serde::{Deserialize, Serialize};

use super::protocol::{CoreError, ErrorCode};

/// Explicit protocol negotiation is required before legacy history can be viewed.
pub const FEATURE: &str = "context_views_v1";

/// Native adapters return JSON-encoded SDK tool values with bounded envelopes.
pub const NATIVE_TOOLS: &str = "native_tool_values_v1";

/// Durable task and evidence state, independent of scheduler worker identity.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ContextStore {
    pub work: BTreeMap<String, WorkUnit>,
    pub evidence: BTreeMap<String, evidence::EvidenceBlock>,
    pub derived: BTreeMap<String, evidence::DerivedArtifact>,
    pub extracts: BTreeMap<String, evidence::EvidenceExtract>,
    #[serde(default)]
    pub artifacts: BTreeMap<String, evidence::EvidenceArtifact>,
    pub decisions: BTreeMap<String, DecisionReceipt>,
    pub views: BTreeMap<String, planner::ContextView>,
    pub executions: BTreeMap<String, ExecutionPlan>,
}

/// A task survives worker reuse and keeps its original evidence references.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkUnit {
    pub task_id: String,
    pub run_id: String,
    pub agent_id: String,
    pub parent_task_id: Option<String>,
    pub text: String,
    pub acceptance_criteria: Vec<String>,
    pub instructions: Vec<String>,
    pub evidence: Vec<String>,
    /// Admitted references from related tasks, materialized only for a view.
    #[serde(default)]
    pub shared_evidence: Vec<String>,
    pub recalled: Vec<String>,
    pub last_view_id: Option<String>,
    pub status: super::session::AgentStatus,
    pub result_evidence: Vec<String>,
}

/// An acknowledged decision intent and its independently acknowledged outcome.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecisionReceipt {
    pub decision_id: String,
    pub run_id: String,
    pub task_id: String,
    pub agent_id: String,
    pub source: planner::SourceRevision,
    pub candidates: planner::CandidateSet,
    pub request: DecisionRequest,
    pub request_sha256: String,
    pub policy: DecisionPolicy,
    pub pricing: Option<DecisionPricing>,
    pub intent_state_revision: u64,
    pub response_limit_bytes: usize,
    pub outcome: Option<Result<DecisionResponse, DecisionError>>,
    pub elapsed_ms: Option<u64>,
    pub stale: bool,
    pub view_ids: Vec<String>,
}

/// A view and the actual language-model plan selected for its execution.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionPlan {
    pub step_id: String,
    pub task_id: String,
    pub agent_id: String,
    pub view_id: String,
    pub decision_id: Option<String>,
    /// Filled by the SDK admission boundary, before any provider dispatch.
    pub model: Option<NativePlan>,
    /// Finite model/view optimization and its explicit local cost assumptions.
    #[serde(default)]
    pub routing: Option<models::Selection>,
}

pub(super) fn invalid(message: impl Into<String>) -> CoreError {
    CoreError::rejected(ErrorCode::CheckpointConflict, message)
}

pub(super) fn digest(value: &impl Serialize) -> Result<String, CoreError> {
    serde_json::to_vec(value)
        .map(|bytes| super::checkpoint::sha256(&bytes))
        .map_err(|error| invalid(error.to_string()))
}
