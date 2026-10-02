//! Fixed rubric weights, post-hoc applicability, evidence checks and missingness.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::evidence::{EVIDENCE_VERSION, EvidencePacket};
use crate::acp_trajectory::checkpoint::types::{
    AssessmentContent, CriterionScore, EvidenceCitation,
};

pub const PPM: u32 = 1_000_000;
pub const RUBRIC_VERSION: &str = "coding-checkpoint-rubric-v2";

#[derive(Debug, Clone, Serialize)]
pub struct Criterion {
    pub id: &'static str,
    pub weight: u32,
    pub mandatory: bool,
    pub applicability: &'static str,
    pub anchors: &'static str,
}

/// Templates describe how to evaluate evidence; they are not pre-session goals.
pub fn library() -> Vec<Criterion> {
    vec![
        Criterion {
            id: "delivery",
            weight: 4,
            mandatory: true,
            applicability: "Always. Infer the user's actual request from the recorded prefix; do not invent requirements.",
            anchors: "0: observed failure to deliver; 0.5: partial delivery with identified omissions; 1: recorded evidence supports each requested behavior and deliverable. Trace obligations to the final recorded artifact, including input distinctions and boundary cases implied by the request. Passing examples or syntactic edits alone do not establish full semantic coverage. A review-only report can be the complete deliverable. Unknown if the result cannot be assessed.",
        },
        Criterion {
            id: "constraints",
            weight: 3,
            mandatory: true,
            applicability: "Always. Evaluate constraints on how work is performed, such as scope, prohibited changes and permitted execution, visible at the relevant time. Task delivery itself is scored under delivery, not counted a second time here.",
            anchors: "0: material explicit constraint violated; 0.5: partial compliance; 1: supported compliance. Not delivering work does not by itself prove a constraint violation. Unknown if evidence is insufficient; a capture gap cannot establish complete compliance. New requirements do not retroactively change earlier constraints.",
        },
        Criterion {
            id: "verification",
            weight: 3,
            mandatory: false,
            applicability: "Applicable to permitted executable validation required by the user or by the requested code changes. Absence of test events does not remove an obligation. If execution was explicitly forbidden or deferred by the user for this checkpoint, exclude that execution obligation; assess any requested static inspection under delivery. A review-only task with merely optional checks does not create an executable-validation obligation.",
            anchors: "First identify the required and permitted check, whether it was attempted, its actual result, and the artifact version it covers. 0: an actionable required check was demonstrably omitted, or execution established a failure of the final artifact. 0.5: partial executable validation or checks stale for the final artifact. 1: relevant executed checks validate the final recorded artifact. Unknown: a required check could not establish correctness because of missing dependencies, credentials, unavailable services or other environmental blockers; record that cause without treating it as a code failure. Agent claims and successful file reads are not positive executable verification. Tests preceding later edits do not validate those edits. A backend error before work is not proof the agent chose to omit an actionable check.",
        },
        Criterion {
            id: "review_resolution",
            weight: 2,
            mandatory: false,
            applicability: "Applicable only when this actor was responsible for resolving review findings or obtaining review acceptance as part of delivery. Reading a review or reporting a finding alone does not create a repair obligation. For review-only work where fixes are outside scope, exclude this criterion and assess the review report under delivery. The agent diagnosing its own implementation bug is not a separate review obligation. Missing review events do not cancel an explicitly requested resolution or acceptance obligation.",
            anchors: "0: material findings within the actor's resolution obligation remain unresolved; 0.5: partial resolution; 1: obligated findings are resolved or required review supports acceptance. Unknown when an applicable resolution or acceptance outcome is unobservable. Do not wait for another actor's later repair of a review-only finding.",
        },
        Criterion {
            id: "pr_delivery",
            weight: 1,
            mandatory: false,
            applicability: "Applicable only when the user requested a PR or a PR is part of the recorded delivery obligation. Missing PR events do not cancel that obligation.",
            anchors: "0: recorded failure of the requested PR delivery; 0.5: partial delivery; 1: recorded state satisfies the request. PR creation or merge does not prove code correctness.",
        },
        Criterion {
            id: "user_acceptance",
            weight: 1,
            mandatory: false,
            applicability: "Applicable when the user gives outcome feedback. Silence is not acceptance; distinguish new requirements from rejection of prior work.",
            anchors: "0: explicit rejection of delivered work; 0.5: qualified acceptance; 1: explicit acceptance. This is preference evidence, not executable verification.",
        },
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Applicability {
    Applicable,
    NotApplicable,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RubricItem {
    pub criterion_id: String,
    pub applicability: Applicability,
    pub selection_reason: String,
    pub score: CriterionScore,
    pub evidence: Vec<EvidenceCitation>,
    pub explanation: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticRole {
    Introduced,
    Discovered,
    Repaired,
    Inherited,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    pub criterion_id: String,
    pub role: DiagnosticRole,
    pub evidence: Vec<EvidenceCitation>,
    pub explanation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RubricEvaluation {
    pub rubric_version: String,
    pub items: Vec<RubricItem>,
    pub diagnostics: Vec<Diagnostic>,
    /// A supported material violation, never inferred from cost or tool count.
    pub severe_violation: bool,
    pub violation_evidence: Vec<EvidenceCitation>,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityBounds {
    pub lower_ppm: u32,
    pub upper_ppm: u32,
    pub coverage_ppm: u32,
    pub applicable_weight: u32,
    pub unknown_weight: u32,
    pub required_unknown: bool,
    pub severe_violation: bool,
    pub capture_gaps: Vec<String>,
}

impl QualityBounds {
    pub fn complete_score(&self) -> Option<f64> {
        (self.unknown_weight == 0 && !self.required_unknown && self.capture_gaps.is_empty())
            .then_some(f64::from(self.lower_ppm) / f64::from(PPM))
    }
}

pub fn digest(value: &impl Serialize) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(value)?)))
}

impl RubricEvaluation {
    pub fn aggregate(&self, packet: &EvidencePacket) -> Result<QualityBounds> {
        ensure!(
            packet.projection_version == EVIDENCE_VERSION,
            "unsupported evidence projection; prepare the checkpoint with the current evaluator"
        );
        ensure!(
            self.rubric_version == RUBRIC_VERSION,
            "unsupported rubric version"
        );
        ensure!(
            !self.summary.trim().is_empty(),
            "evaluation summary is required"
        );
        let templates = library();
        ensure!(
            self.items.len() == templates.len(),
            "all rubric items must be explicitly selected"
        );
        let mut seen = BTreeSet::new();
        let mut weighted = 0_u64;
        let mut total = 0_u32;
        let mut unknown = 0_u32;
        let mut required_unknown = false;
        for item in &self.items {
            ensure!(
                seen.insert(&item.criterion_id),
                "duplicate rubric criterion"
            );
            let template = templates
                .iter()
                .find(|t| t.id == item.criterion_id)
                .ok_or_else(|| {
                    anyhow::anyhow!("unknown rubric criterion: {}", item.criterion_id)
                })?;
            ensure!(
                !item.selection_reason.trim().is_empty() && !item.explanation.trim().is_empty(),
                "selection and scoring explanations are required"
            );
            packet.validate_citations(&item.evidence)?;
            match (item.applicability, &item.score) {
                (Applicability::NotApplicable, CriterionScore::NotApplicable) => {
                    ensure!(!template.mandatory, "mandatory rubric cannot be excluded");
                }
                (Applicability::Applicable, CriterionScore::Scored { value_ppm }) => {
                    ensure!(*value_ppm <= PPM, "rubric score is outside [0, 1]");
                    ensure!(
                        !item.evidence.is_empty(),
                        "a scored criterion needs original evidence"
                    );
                    if template.id == "verification" && *value_ppm > 0 {
                        ensure!(
                            packet.has_tool_observation(&item.evidence),
                            "positive verification needs a recorded tool result, not an agent claim"
                        );
                    }
                    total += template.weight;
                    weighted += u64::from(template.weight) * u64::from(*value_ppm);
                }
                (Applicability::Applicable | Applicability::Unknown, CriterionScore::Unknown) => {
                    total += template.weight;
                    unknown += template.weight;
                    required_unknown |=
                        template.mandatory || item.applicability == Applicability::Unknown;
                }
                _ => anyhow::bail!("applicability and score status disagree"),
            }
        }
        for diagnostic in &self.diagnostics {
            ensure!(
                seen.contains(&diagnostic.criterion_id),
                "diagnostic references an unknown rubric"
            );
            ensure!(
                !diagnostic.explanation.trim().is_empty(),
                "diagnostic explanation is required"
            );
            ensure!(
                !diagnostic.evidence.is_empty(),
                "diagnostic needs original evidence"
            );
            packet.validate_citations(&diagnostic.evidence)?;
        }
        packet.validate_citations(&self.violation_evidence)?;
        ensure!(
            !self.severe_violation || !self.violation_evidence.is_empty(),
            "severe violation requires evidence"
        );
        ensure!(total > 0, "no applicable rubric weight");
        Ok(QualityBounds {
            lower_ppm: (weighted / u64::from(total)) as u32,
            upper_ppm: ((weighted + u64::from(unknown) * u64::from(PPM)) / u64::from(total)) as u32,
            coverage_ppm: ((u64::from(total - unknown) * u64::from(PPM)) / u64::from(total)) as u32,
            applicable_weight: total,
            unknown_weight: unknown,
            required_unknown,
            severe_violation: self.severe_violation,
            capture_gaps: packet.gaps.clone(),
        })
    }

    pub fn assessment(
        &self,
        packet: &EvidencePacket,
        pipeline_digest: &str,
    ) -> Result<AssessmentContent> {
        self.aggregate(packet)?;
        let selection: Vec<_> = self
            .items
            .iter()
            .map(|item| {
                (
                    &item.criterion_id,
                    item.applicability,
                    &item.selection_reason,
                )
            })
            .collect();
        let mut evidence = BTreeMap::new();
        for citation in self
            .items
            .iter()
            .flat_map(|i| &i.evidence)
            .chain(self.diagnostics.iter().flat_map(|d| &d.evidence))
            .chain(&self.violation_evidence)
        {
            evidence.insert(citation.node_id.clone(), citation.clone());
        }
        Ok(AssessmentContent {
            pipeline_config_digest: pipeline_digest.into(),
            selection_digest: digest(&(RUBRIC_VERSION, selection))?,
            scores: self
                .items
                .iter()
                .map(|i| (i.criterion_id.clone(), i.score.clone()))
                .collect(),
            evidence: evidence.into_values().collect(),
            // The full structured rubric remains in the revision rather than an
            // independent cache that could outlive deletion of its source.
            explanation: serde_json::to_string(self)?,
        })
    }
}
