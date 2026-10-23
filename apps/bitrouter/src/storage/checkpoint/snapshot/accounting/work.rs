//! Persisted session records and offline checkpoint inspection.
//! Scheduling and execution transitions are implemented by core.

use bitrouter_ai::core::evidence::NativeTokenCost;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Frozen, credential-free admission retained after the original turn retires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderAttemptSource {
    pub attempt_index: u32,
    pub route: bitrouter_ai::core::evidence::NativeRoute,
    /// Canonical output contract remains live after the original turn retires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_output_bytes: Option<u64>,
    /// Preserves the admission policy when the original turn is replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_output_version: Option<u32>,
    /// Complete report contract, including bounded metadata-rejection evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_report_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostWorkKind {
    /// Includes App transforms, routing preparation and bound request checks.
    Preparation,
    PreparationCallback,
    DecisionModel,
    ProviderAttempt,
    InputCount,
    ContextValidation,
    ContextRebuild,
    WorkspaceTool,
    MaterialFetch,
    AuthenticationPreparation,
    Authentication,
    AuthenticationRefresh,
    HttpDispatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostWorkState {
    /// Dispatch may or may not have occurred. Absence of an outcome is not zero.
    IntentRecorded,
    /// Execution evidence is durable, independently of whether money is known.
    OutcomeRecorded,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostWork {
    pub agent_id: String,
    pub agent_turn_id: String,
    /// Material fetching precedes model-step admission and has no step ID.
    pub step_id: Option<String>,
    /// SDK billing identity, never a harness material request ID.
    pub request_id: Option<String>,
    pub kind: CostWorkKind,
    pub state: CostWorkState,
    /// Provider operation time when observed, excluding checkpoint waits.
    /// Missing measurements, including preparation/tool time, remain unknown.
    pub elapsed_ms: Option<u64>,
    /// An estimate of model tokens only; never a reported or reconciled charge.
    pub token_estimate: Option<NativeTokenCost>,
    /// Canonical counters survive worker retirement even without registry prices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_usage: Option<bitrouter_ai::types::Usage>,
    /// Typed decision tokens do not provide language-model cache buckets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_usage: Option<crate::gateway::model::decision::types::DecisionUsage>,
    /// Estimate at the decision intent's frozen prices, never a settled bill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_estimate_micro_usd: Option<u64>,
    /// Total expenditure remains unknown even when a token estimate is known.
    /// A successful operation, including a denied tool, is not a billing receipt.
    pub unknown_cost_reason: String,
    #[serde(default)]
    pub provider_source: Option<ProviderAttemptSource>,
    /// Exact report commitment allows duplicate/conflict checks after retirement.
    #[serde(default)]
    pub outcome_sha256: Option<String>,
}

/// Present only for runs tracked from admission. Missing legacy run entries do
/// not establish an empty or zero-cost run. Entries survive turn/run replacement.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunCostWork {
    pub work: BTreeMap<String, CostWork>,
    /// Separate bases for each source/bill identity. Never a sum of all bases.
    #[serde(default)]
    pub charges: BTreeMap<String, crate::gateway::pipeline::native_accounting::NativeCostClaim>,
    /// Latest read uncertainty for a request, independent of retained claims.
    #[serde(default)]
    pub charge_unknown: BTreeMap<String, String>,
}
