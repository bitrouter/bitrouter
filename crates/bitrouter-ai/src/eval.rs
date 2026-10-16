//! Evaluation exchange contracts. Authority, evidence admission, digest
//! construction and persistence are service-owned; these types perform no I/O.

use crate::types::ReasoningEffort;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const EVAL_SCHEMA_VERSION: u32 = 1;
pub const ROUTE_MEASUREMENT_SCHEMA_VERSION: u32 = 2;

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalScope {
    Request,
    Episode,
    Task,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentArm {
    Control,
    Challenger,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentAssignmentUnit {
    Task,
    Episode,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalExperimentRef {
    pub experiment_id: String,
    pub arm: ExperimentArm,
    pub assignment_unit: ExperimentAssignmentUnit,
    pub assignment_id_digest: String,
    pub challenger_propensity_ppm: u32,
}

/// One semantic policy action declared by the immutable routing snapshot.
///
/// Probabilities describe the logging policy before deterministic tool,
/// progress, or continuation guards. The effective action remains
/// [`EvalDecisionRef::selected_tier`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteActionCandidate {
    pub context: crate::routing::ContextStrategy,
    pub tier: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    pub logging_probability_ppm: u32,
}

/// Replayable evidence for the semantic action distribution seen by one
/// policy decision. This is diagnostic evidence and never controls routing.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteDecisionMeasurement {
    pub schema_version: u32,
    pub logging_action_tier: String,
    pub logging_action_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging_action_effort: Option<ReasoningEffort>,
    pub logging_action_probability_ppm: u32,
    pub candidate_set_digest: String,
    pub candidates: Vec<RouteActionCandidate>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalDecisionRef {
    pub decision_id: String,
    pub policy: String,
    pub route_projection: String,
    pub request_key: String,
    pub selected_tier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_effort: Option<ReasoningEffort>,
    pub baseline_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_effort: Option<ReasoningEffort>,
    pub policy_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment: Option<EvalExperimentRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_measurement: Option<RouteDecisionMeasurement>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceItem {
    pub evidence_id: String,
    pub kind: String,
    pub digest: String,
    pub redacted: bool,
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalSubject {
    pub schema_version: u32,
    pub eval_id: String,
    pub scope: EvalScope,
    pub subject_id: String,
    pub policy_digest: String,
    pub preset: Option<String>,
    pub cohort: Option<String>,
    pub holdout: bool,
    #[serde(default)]
    pub decisions: Vec<EvalDecisionRef>,
    #[serde(default)]
    pub requested_dimensions: BTreeSet<String>,
    #[serde(default)]
    pub evidence: Vec<EvidenceItem>,
    pub evidence_digest: String,
    pub observed_at: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluatorKind {
    TaskNative,
    Human,
    Enterprise,
    Agentic,
    Generic,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluatorIdentity {
    pub authority_id: String,
    pub evaluator_id: String,
    pub kind: EvaluatorKind,
    pub version: String,
    pub config_digest: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalVerdict {
    Pass,
    Fail,
    Inconclusive,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricUnit {
    Boolean,
    Ppm,
    MicroUsd,
    Milliseconds,
    Count,
    ScalarMicros,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricValue {
    pub value: i64,
    pub unit: MetricUnit,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionCredit {
    pub weight_ppm: i64,
    #[serde(default)]
    pub metric_ids: BTreeSet<String>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationResult {
    pub schema_version: u32,
    pub eval_id: String,
    pub evidence_digest: String,
    pub evaluator: EvaluatorIdentity,
    pub verdict: EvalVerdict,
    #[serde(default)]
    pub metrics: BTreeMap<String, MetricValue>,
    #[serde(default)]
    pub hard_violations: Vec<String>,
    pub confidence_ppm: Option<i64>,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    #[serde(default)]
    pub decision_credit: BTreeMap<String, DecisionCredit>,
    pub idempotency_key: String,
    pub submitted_at: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionStatus {
    Admitted,
    Rejected,
    HeldOut,
    Disputed,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectInsertOutcome {
    Inserted,
    Duplicate,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSnapshotEntry {
    pub result_id: String,
    pub content_digest: String,
    pub subject_content_digest: String,
    pub eval_id: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSnapshot {
    pub evidence_root: String,
    pub frozen_at: String,
    pub entries: Vec<EvalSnapshotEntry>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionOutcome {
    pub result_id: String,
    pub status: AdmissionStatus,
    pub reason: String,
    pub duplicate: bool,
}

impl MetricValue {
    pub fn new(value: i64, unit: MetricUnit) -> Self {
        Self { value, unit }
    }

    pub fn validate(&self) -> Result<(), MetricValidationError> {
        match self.unit {
            MetricUnit::Boolean if !matches!(self.value, 0 | 1) => {
                Err(MetricValidationError::Boolean)
            }
            MetricUnit::Ppm if !(0..=1_000_000).contains(&self.value) => {
                Err(MetricValidationError::Ppm)
            }
            MetricUnit::MicroUsd | MetricUnit::Milliseconds | MetricUnit::Count
                if self.value < 0 =>
            {
                Err(MetricValidationError::Unsigned)
            }
            _ => Ok(()),
        }
    }
}

/// A metric value outside its declared unit's intrinsic range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MetricValidationError {
    #[error("boolean metric must be 0 or 1")]
    Boolean,
    #[error("ppm metric must be between 0 and 1000000")]
    Ppm,
    #[error("unsigned metric unit cannot be negative")]
    Unsigned,
}

/// Receipt for inserting an evaluation subject without changing its content.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutSubjectReport {
    pub eval_id: String,
    pub outcome: SubjectInsertOutcome,
}

/// Optional snapshot time; absence uses the service's clock.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreezeRequest {
    pub frozen_at: Option<String>,
}

/// Counts in the authenticated caller's evidence ledger.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    pub subjects: usize,
    pub results: usize,
    /// Existing status keys include `heldout`, distinct from `AdmissionStatus`'s
    /// `held_out` serialization in result receipts.
    pub admission: BTreeMap<String, usize>,
}

/// Existing evaluation HTTP error body; status and headers belong to transport.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub error: String,
}

redacted_contract_debug!(ErrorEnvelope);
