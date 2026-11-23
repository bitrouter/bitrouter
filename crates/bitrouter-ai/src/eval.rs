//! Evaluation exchange contracts. Authority, evidence admission, digest
//! construction and persistence are service-owned; these types perform no I/O.

use crate::policy::RouteClass;
use crate::tenancy::{
    configuration::ResourceRef,
    identity::{ExecutionId, InstallationId, RevisionId, TenantId},
};
use crate::types::ReasoningEffort;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Current evaluation exchange and frozen-evidence contract.
pub const SPEC_VERSION: &str = "eval/1.0.0-beta.1";

/// Closed discriminator for evaluation exchange documents.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpecVersion {
    /// The model-routing evaluation exchange.
    #[serde(rename = "eval/1.0.0-beta.1")]
    Beta1,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalScope {
    Request,
    Episode,
    Task,
}

/// One semantic policy action declared by the immutable routing snapshot.
///
/// Probabilities describe the logging policy before deterministic tool or
/// continuation guards. The effective action remains
/// [`EvalDecisionRef::selected_tier`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteActionCandidate {
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
    pub route_projection: RouteClass,
    pub request_key: RouteClass,
    pub selected_tier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_effort: Option<ReasoningEffort>,
    pub baseline_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_effort: Option<ReasoningEffort>,
    pub policy_digest: String,
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

/// Explicit association with an actual immutable execution receipt.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalExecutionRef {
    /// Invocation being evaluated, in the enclosing tenant namespace.
    pub execution_id: ExecutionId,
    /// Configuration revision used by that invocation.
    pub configuration_revision: RevisionId,
}

/// Closed identity profiles. Digests and policy names alone grant no identity.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "profile", rename_all = "snake_case", deny_unknown_fields)]
pub enum EvalSubjectIdentity {
    /// Model-policy evidence for one exact immutable route binding.
    TenantModelPolicy {
        /// Persistent service installation.
        installation_id: InstallationId,
        /// Authenticated tenant namespace.
        tenant_id: TenantId,
        /// Immutable configuration containing the bound route.
        configuration_revision: RevisionId,
        /// Full owner/kind/resource/revision identity of the selected Policy.
        policy: ResourceRef,
        /// Literal selected name in that Policy document.
        name: String,
        /// Optional source evidence; never a resource revision or authority.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_digest: Option<String>,
        /// Optional classifier evidence, distinct from the source digest.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        classifier_digest: Option<String>,
        /// Optional actual invocation; the service verifies its receipt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution: Option<EvalExecutionRef>,
    },
    /// Policy-free tenant evidence, never eligible as model-policy evidence.
    TenantGeneric {
        /// Persistent service installation.
        installation_id: InstallationId,
        /// Authenticated tenant namespace.
        tenant_id: TenantId,
        /// Optional actual invocation, including its configuration revision.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution: Option<EvalExecutionRef>,
    },
    /// Gateway evidence using the gateway's independent authentication authority.
    GatewayGeneric {},
}

impl EvalSubjectIdentity {
    /// Tenant namespace claimed by this profile; never an authorization proof.
    pub fn tenant_namespace(&self) -> Option<(&InstallationId, &TenantId)> {
        match self {
            Self::TenantModelPolicy {
                installation_id,
                tenant_id,
                ..
            }
            | Self::TenantGeneric {
                installation_id,
                tenant_id,
                ..
            } => Some((installation_id, tenant_id)),
            Self::GatewayGeneric {} => None,
        }
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalSubject {
    /// Exact evaluation exchange contract.
    pub spec_version: SpecVersion,
    /// Exact service-validated profile and resource ownership.
    pub identity: EvalSubjectIdentity,
    pub eval_id: String,
    pub scope: EvalScope,
    pub subject_id: String,
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
    /// Exact evaluation exchange contract.
    pub spec_version: SpecVersion,
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
#[serde(deny_unknown_fields)]
pub struct EvalSnapshot {
    /// Exact evaluation exchange contract.
    pub spec_version: SpecVersion,
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
