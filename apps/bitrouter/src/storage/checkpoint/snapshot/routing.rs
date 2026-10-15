//! Persisted session records and offline checkpoint inspection.
//! Scheduling and execution transitions are implemented by core.

use bitrouter_ai::core::evidence::NativeAttemptReport;
use bitrouter_ai::core::protocol::{CoreError, MaterialRef, RoutingSettings};
use bitrouter_ai::types::{ReasoningEffort, UsageOrigin};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextManifest {
    pub context_id: String,
    pub revision: u64,
    pub agent_id: String,
    pub agent_turn_id: String,
    pub signal_revision: u64,
    pub permission_revision: u64,
    pub workspace_revision: Option<String>,
    pub tool_manifest_digest: String,
    pub required_instructions: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub system_sha256: String,
    /// Ordered message commitments preserve both role and call/result identity.
    pub message_sha256: Vec<String>,
    /// References retain provenance; bodies live in the step's material snapshot.
    pub materials: Vec<MaterialRef>,
    pub prompt_bytes: u64,
    pub estimated_input_tokens: Option<u64>,
    pub token_estimate_source: String,
    pub output_allowance: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingDecision {
    pub decision_id: String,
    /// The committed task allocation is shared by every step in this turn.
    pub allocation_id: Option<String>,
    pub policy_id: String,
    pub source: String,
    pub input_state_revision: u64,
    pub modes: RoutingSettings,
    pub context: ContextManifest,
    pub candidate_ids: Vec<String>,
    pub selected_candidate_id: String,
    pub selected_model: String,
    pub selected_effort: Option<ReasoningEffort>,
    pub reason_codes: Vec<String>,
    /// Per-provider assessment after shared model selection. Unknown facts
    /// permit an attempt but never establish a verified capacity fit.
    pub routes: Vec<RouteFeasibility>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteFeasibility {
    pub route_index: u32,
    pub rejection_reasons: Vec<String>,
    pub unverified_constraints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationDisposition {
    Applied,
    Rejected,
    Stale,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionApplied {
    pub decision_id: String,
    pub agent_turn_id: String,
    pub step_id: String,
    pub state_revision: u64,
    pub disposition: ApplicationDisposition,
    pub reason: Option<CoreError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionReceipt {
    pub decision_id: String,
    pub attempt_id: String,
    pub report: NativeAttemptReport,
    /// Canonical serving request effort, not a provider-observed effort value.
    pub requested_effort: Option<ReasoningEffort>,
    pub usage_origin: Option<UsageOrigin>,
    /// Configured model-token estimate only, not an authoritative charge.
    /// A missing price or usage stays unknown; auxiliary fees are not included.
    pub cost_micro_usd: Option<u64>,
    pub cost_source: String,
    /// Usage without raw cache counters does not prove cache savings.
    pub cache_observation_source: String,
}
