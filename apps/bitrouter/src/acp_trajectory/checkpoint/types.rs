//! Versioned checkpoint and assessment contracts. No scoring model is invoked.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::super::{CanonicalEvent, RequestAssociation, SessionIdentity};

/// An immutable raw observation reference, including its original content digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventReference {
    pub connection_id: String,
    pub sequence: i64,
    pub session_sequence: Option<i64>,
    pub digest: String,
}

impl EventReference {
    pub fn node_id(&self) -> String {
        format!("{}:{}", self.connection_id, self.sequence)
    }
}

/// A native session's own prefix; inherited prefixes remain separate references.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrefixSegment {
    pub identity: SessionIdentity,
    pub watermark: i64,
    pub parent_key: Option<String>,
    pub parent_watermark: Option<i64>,
    pub events: Vec<EventReference>,
    pub setup: Vec<EventReference>,
    pub connections: Vec<ConnectionObservation>,
    pub boundary_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionObservation {
    pub connection_id: String,
    pub state: String,
    pub controller_instance_id: Option<String>,
    pub route_scope_id: Option<String>,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub schema_version: u32,
    pub canonical_version: u32,
    pub checkpoint_id: String,
    pub identity: SessionIdentity,
    pub watermark: i64,
    pub prefix_digest: String,
    pub previous_checkpoint_id: Option<String>,
    pub family_id: String,
    /// Ordered from the oldest inherited prefix to this session's own prefix.
    pub segments: Vec<PrefixSegment>,
    pub gaps: Vec<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct CheckpointContent {
    pub checkpoint: Checkpoint,
    pub events: Vec<CanonicalEvent>,
    pub setup: Vec<CanonicalEvent>,
}

/// A separate immutable observation of locally recorded metering/route evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceObservation {
    pub observation_id: String,
    pub checkpoint_id: String,
    pub revision: i64,
    pub previous_observation_id: Option<String>,
    pub observed_at: String,
    pub requests: Vec<RequestAssociation>,
    /// Session correlation exists, but membership in this fixed prefix is unknown.
    pub unassigned_request_ids: Vec<String>,
    pub known_cost_micro_usd: i64,
    pub unpriced_requests: usize,
    pub metering_complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssessmentSource {
    Human,
    Agentic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum CriterionScore {
    Scored { value_ppm: u32 },
    Unknown,
    NotApplicable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceCitation {
    pub node_id: String,
    pub digest: String,
}

/// Imported labels retain their evaluator identity. Templates and aggregation
/// are deliberately owned by the scoring layer, not this persistence layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssessmentContent {
    pub pipeline_config_digest: String,
    pub selection_digest: String,
    pub scores: BTreeMap<String, CriterionScore>,
    pub evidence: Vec<EvidenceCitation>,
    pub explanation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevisionInput {
    pub submission_id: String,
    pub checkpoint_id: String,
    #[serde(deserialize_with = "required_nullable")]
    pub expected_revision: Option<String>,
    pub source: AssessmentSource,
    pub evaluator_id: String,
    pub evaluator_version: String,
    /// None explicitly retracts the current assessment; old labels never revive.
    #[serde(deserialize_with = "required_nullable")]
    pub assessment: Option<AssessmentContent>,
    pub reason: String,
}

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssessmentRevision {
    pub revision_id: String,
    pub input_digest: String,
    pub input: RevisionInput,
    pub supersedes: Option<String>,
    pub selected_on_submission: bool,
    pub selection_reason: String,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct EffectiveAssessment {
    pub identity: SessionIdentity,
    pub current_watermark: i64,
    pub current_revision: Option<String>,
    pub assessment: Option<AssessmentRevision>,
    pub checkpoint: Option<Checkpoint>,
    pub stale: bool,
    pub reasons: Vec<String>,
    pub resource: Option<ResourceObservation>,
}

#[derive(Debug, Serialize)]
pub struct FamilyView {
    pub family_id: String,
    pub sessions: Vec<EffectiveAssessment>,
    /// Labels without newer content, not a quality or independence guarantee.
    pub current_assessments: usize,
    pub requests: Vec<RequestAssociation>,
    pub conflicting_request_ids: Vec<String>,
    pub known_cost_micro_usd: i64,
    pub unpriced_requests: usize,
    pub metering_complete: bool,
}

#[derive(Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum CheckpointReport {
    Checkpoint(Checkpoint),
    Checkpoints(Vec<Checkpoint>),
    Content(Box<CheckpointContent>),
    Resources(ResourceObservation),
    ResourceHistory(Vec<ResourceObservation>),
    Revision(AssessmentRevision),
    History(Vec<AssessmentRevision>),
    Effective(Box<EffectiveAssessment>),
    Family(Box<FamilyView>),
}

impl crate::output::CliReport for CheckpointReport {
    fn render(&self, h: &mut crate::output::human::Human<'_>) -> std::io::Result<()> {
        // The detailed manifest is also useful as a human-readable audit export.
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        h.line(&text)
    }
}
