//! Persisted session records and offline checkpoint inspection.
//! Scheduling and execution transitions are implemented by core.

use self::evidence::EvidenceBlock;
use super::SessionSnapshot;
use crate::gateway::model::decision::policy::{DecisionPolicy, DecisionPricing};
use crate::gateway::model::decision::types::{DecisionError, DecisionRequest, DecisionResponse};
use crate::gateway::pipeline::native::NativePlan;
use bitrouter_ai::core::protocol::{CoreError, ErrorCode};
use bitrouter_ai::types::Content;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
pub mod action;
pub mod evidence;
pub mod inspection;
pub(crate) mod validation;
pub mod view;

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
    pub views: BTreeMap<String, crate::storage::checkpoint::snapshot::context::view::ContextView>,
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
    pub status: crate::storage::checkpoint::snapshot::AgentStatus,
    pub result_evidence: Vec<String>,
}

/// An acknowledged decision intent and its independently acknowledged outcome.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecisionReceipt {
    pub decision_id: String,
    pub run_id: String,
    pub task_id: String,
    pub agent_id: String,
    pub source: crate::storage::checkpoint::snapshot::context::view::SourceRevision,
    pub candidates: crate::storage::checkpoint::snapshot::context::view::CandidateSet,
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
    pub routing: Option<crate::gateway::routing::plan::Selection>,
}

/// Explicit protocol negotiation is required before legacy history can be viewed.
pub const FEATURE: &str = "context_views_v1";

/// Native adapters return JSON-encoded SDK tool values with bounded envelopes.
pub const NATIVE_TOOLS: &str = "native_tool_values_v1";

/// Owner permits an authenticated Responses reasoning projection on model change.
pub const PORTABLE_RESPONSES_REASONING: &str = "portable_responses_reasoning_v1";

pub(crate) fn invalid(message: impl Into<String>) -> CoreError {
    CoreError::rejected(ErrorCode::CheckpointConflict, message)
}

pub(crate) fn digest(value: &impl Serialize) -> Result<String, CoreError> {
    serde_json::to_vec(value)
        .map(|bytes| crate::storage::checkpoint::sha256(&bytes))
        .map_err(|error| invalid(error.to_string()))
}

pub(crate) fn enabled(state: &SessionSnapshot) -> bool {
    state
        .manifest
        .required_features
        .iter()
        .any(|feature| feature == FEATURE)
}

/// Opaque provider continuations stay with their originating conversation.
/// Related tasks can share plain text and complete ordinary tool exchanges.
pub(crate) fn portable(block: &EvidenceBlock) -> bool {
    block
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .all(|content| match content {
            Content::Text {
                provider_metadata, ..
            } => provider_metadata.is_empty(),
            Content::ToolCall {
                provider_metadata,
                provider_executed,
                dynamic,
                ..
            } => provider_metadata.is_empty() && !provider_executed && !dynamic,
            Content::ToolResult {
                provider_metadata,
                dynamic,
                ..
            } => provider_metadata.is_empty() && !dynamic,
            _ => false,
        })
}
