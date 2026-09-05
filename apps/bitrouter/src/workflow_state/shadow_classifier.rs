//! Auditable evidence contract for classifiers that never control live routing.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::eval::types::canonical_digest;

pub const SHADOW_CLASSIFIER_SCHEMA_VERSION: u32 = 1;
pub const ROUTE_CONTEXT_SCHEMA_VERSION: u32 = 3;
pub const PROBABILITY_TOTAL_PPM: u32 = 1_000_000;

pub const TASK_FAMILY_LABELS: &[&str] = &[
    "agent:general",
    "agent:memory_operations",
    "agent:multi_step_planning",
    "agent:web_research",
    "agent:workflow_execution",
    "code:debugging",
    "code:devops_config",
    "code:frontend_ui",
    "code:generation",
    "code:repository_analysis",
    "code:review",
    "code:sql_database",
    "unknown",
];
pub const ROLE_LABELS: &[&str] = &[
    "finalize",
    "implement",
    "mechanical",
    "orchestrate",
    "unknown",
    "verify",
];
pub const PROGRESS_LABELS: &[&str] = &[
    "near_done",
    "opening",
    "progressing",
    "recovering",
    "stalled",
    "unknown",
];
pub const RISK_LABELS: &[&str] = &["context", "guarded", "normal"];
pub const CAPABILITY_LABELS: &[&str] = &[
    "code_reasoning",
    "context_retention",
    "planning",
    "precision",
    "research",
    "tool_execution",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowPredictorKind {
    DeterministicScorecard,
    EmbeddingPrototype,
    FrozenEncoderLinear,
    FrozenEncoderMlp,
    DistilledEncoder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowConfidenceKind {
    HeuristicMargin,
    CalibratedProbability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskAuthority {
    DeterministicRules,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceMeasurements {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub median_latency_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p95_latency_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measurement_environment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowPredictorProvenance {
    pub kind: ShadowPredictorKind,
    pub name: String,
    pub version: String,
    pub artifact_digest: String,
    pub feature_digest: String,
    pub confidence_kind: ShadowConfidenceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration_split_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub training_split_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceMeasurements>,
}

impl ShadowPredictorProvenance {
    pub fn validate(&self) -> Result<()> {
        validate_identifier(&self.name, "predictor.name")?;
        validate_identifier(&self.version, "predictor.version")?;
        validate_digest(&self.artifact_digest, "predictor.artifact_digest")?;
        validate_digest(&self.feature_digest, "predictor.feature_digest")?;
        validate_optional_digest(
            self.training_split_digest.as_deref(),
            "predictor.training_split_digest",
        )?;
        match self.confidence_kind {
            ShadowConfidenceKind::HeuristicMargin => {
                if self.calibration_digest.is_some() || self.calibration_split_digest.is_some() {
                    anyhow::bail!("heuristic confidence cannot declare calibration provenance")
                }
            }
            ShadowConfidenceKind::CalibratedProbability => {
                validate_digest(
                    self.calibration_digest.as_deref().ok_or_else(|| {
                        anyhow::anyhow!("calibrated probability requires a calibration digest")
                    })?,
                    "predictor.calibration_digest",
                )?;
                validate_digest(
                    self.calibration_split_digest.as_deref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "calibrated probability requires a calibration split digest"
                        )
                    })?,
                    "predictor.calibration_split_digest",
                )?;
            }
        }
        if let Some(resources) = &self.resources {
            resources.validate()?;
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        canonical_digest(&("bitrouter.shadow-classifier-predictor.v1", self))
    }
}

impl ResourceMeasurements {
    fn validate(&self) -> Result<()> {
        if self
            .p95_latency_micros
            .zip(self.median_latency_micros)
            .is_some_and(|(p95, median)| p95 < median)
        {
            anyhow::bail!("p95 latency must be at least median latency")
        }
        if let Some(environment) = &self.measurement_environment {
            validate_identifier(environment, "resources.measurement_environment")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassProbability {
    pub label: String,
    pub probability_ppm: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoricalHead {
    pub predicted_label: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub probabilities: Vec<ClassProbability>,
}

impl CategoricalHead {
    fn validate(&self, labels: &[&str], calibrated: bool, field: &str) -> Result<()> {
        if !labels.contains(&self.predicted_label.as_str()) {
            anyhow::bail!("{field}.predicted_label is not in the declared label space")
        }
        if !calibrated {
            if !self.probabilities.is_empty() {
                anyhow::bail!("{field} heuristic output cannot contain probabilities")
            }
            return Ok(());
        }
        if self.probabilities.len() != labels.len() {
            anyhow::bail!("{field} must contain the complete calibrated label space")
        }
        let actual = self
            .probabilities
            .iter()
            .map(|entry| entry.label.as_str())
            .collect::<Vec<_>>();
        if actual != labels {
            anyhow::bail!("{field} probabilities must use canonical label order")
        }
        let total = self
            .probabilities
            .iter()
            .try_fold(0_u32, |sum, entry| sum.checked_add(entry.probability_ppm))
            .ok_or_else(|| anyhow::anyhow!("{field} probability total overflow"))?;
        if total != PROBABILITY_TOTAL_PPM {
            anyhow::bail!("{field} probabilities must total {PROBABILITY_TOTAL_PPM} ppm")
        }
        let maximum = self
            .probabilities
            .iter()
            .map(|entry| entry.probability_ppm)
            .max()
            .unwrap_or_default();
        if !self
            .probabilities
            .iter()
            .any(|entry| entry.label == self.predicted_label && entry.probability_ppm == maximum)
        {
            anyhow::bail!("{field}.predicted_label must be a maximum-probability label")
        }
        Ok(())
    }

    pub fn top_probability_ppm(&self) -> Option<u32> {
        self.probabilities
            .iter()
            .map(|entry| entry.probability_ppm)
            .max()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityScore {
    pub label: String,
    pub score_ppm: u32,
    pub threshold_ppm: u32,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OodAssessment {
    pub score_ppm: u32,
    pub threshold_ppm: u32,
    pub is_ood: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteContextV3 {
    pub schema_version: u32,
    pub task_family: CategoricalHead,
    pub next_step_role: CategoricalHead,
    pub progress_state: CategoricalHead,
    pub route_risk: CategoricalHead,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<CapabilityScore>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ood: Option<OodAssessment>,
    pub abstained: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abstention_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exemplar_ids: Vec<String>,
    pub risk_authority: RiskAuthority,
}

impl RouteContextV3 {
    fn validate(&self, confidence_kind: ShadowConfidenceKind) -> Result<()> {
        if self.schema_version != ROUTE_CONTEXT_SCHEMA_VERSION {
            anyhow::bail!("unsupported route context schema version")
        }
        let calibrated = confidence_kind == ShadowConfidenceKind::CalibratedProbability;
        self.task_family
            .validate(TASK_FAMILY_LABELS, calibrated, "task_family")?;
        self.next_step_role
            .validate(ROLE_LABELS, calibrated, "next_step_role")?;
        self.progress_state
            .validate(PROGRESS_LABELS, calibrated, "progress_state")?;
        self.route_risk
            .validate(RISK_LABELS, calibrated, "route_risk")?;
        validate_capabilities(&self.capabilities, calibrated)?;
        if let Some(ood) = &self.ood {
            if !calibrated {
                anyhow::bail!("heuristic output cannot declare a calibrated OOD score")
            }
            validate_ppm(ood.score_ppm, "ood.score_ppm")?;
            validate_ppm(ood.threshold_ppm, "ood.threshold_ppm")?;
            if ood.is_ood != (ood.score_ppm >= ood.threshold_ppm) {
                anyhow::bail!("ood.is_ood must match score and threshold")
            }
        }
        match (self.abstained, self.abstention_reason.as_deref()) {
            (true, Some(reason)) => validate_identifier(reason, "abstention_reason")?,
            (true, None) => anyhow::bail!("abstention requires a reason"),
            (false, Some(_)) => anyhow::bail!("non-abstained output cannot carry a reason"),
            (false, None) => {}
        }
        if self.exemplar_ids.len() > 16 {
            anyhow::bail!("exemplar_ids exceeds the bounded evidence limit")
        }
        validate_sorted_identifiers(&self.exemplar_ids, "exemplar_ids")?;
        Ok(())
    }
}

fn validate_capabilities(capabilities: &[CapabilityScore], calibrated: bool) -> Result<()> {
    if !calibrated && !capabilities.is_empty() {
        anyhow::bail!("heuristic output cannot contain capability scores")
    }
    let labels = capabilities
        .iter()
        .map(|capability| capability.label.as_str())
        .collect::<Vec<_>>();
    if !labels.windows(2).all(|pair| pair[0] < pair[1]) {
        anyhow::bail!("capabilities must be unique and canonically ordered")
    }
    for capability in capabilities {
        if !CAPABILITY_LABELS.contains(&capability.label.as_str()) {
            anyhow::bail!("unknown capability label {}", capability.label)
        }
        validate_ppm(capability.score_ppm, "capability.score_ppm")?;
        validate_ppm(capability.threshold_ppm, "capability.threshold_ppm")?;
        if capability.selected != (capability.score_ppm >= capability.threshold_ppm) {
            anyhow::bail!("capability selected flag must match score and threshold")
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowClassifierPrediction {
    pub fixture_id: String,
    pub dataset_digest: String,
    pub input_projection_digest: String,
    pub predictor_digest: String,
    pub context: RouteContextV3,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowClassifierSubmission {
    pub schema_version: u32,
    pub dataset_digest: String,
    pub predictor: ShadowPredictorProvenance,
    pub predictions: Vec<ShadowClassifierPrediction>,
}

impl ShadowClassifierSubmission {
    pub fn load_json(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = std::fs::read(path)
            .with_context(|| format!("read classifier submission {}", path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("parse classifier submission {}", path.display()))
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SHADOW_CLASSIFIER_SCHEMA_VERSION {
            anyhow::bail!("unsupported shadow classifier schema version")
        }
        validate_digest(&self.dataset_digest, "dataset_digest")?;
        self.predictor.validate()?;
        let predictor_digest = self.predictor.digest()?;
        let mut previous = None;
        for prediction in &self.predictions {
            validate_identifier(&prediction.fixture_id, "prediction.fixture_id")?;
            if previous.is_some_and(|value| value >= prediction.fixture_id.as_str()) {
                anyhow::bail!("predictions must be unique and ordered by fixture_id")
            }
            previous = Some(prediction.fixture_id.as_str());
            if prediction.dataset_digest != self.dataset_digest {
                anyhow::bail!("prediction dataset digest does not match submission")
            }
            validate_digest(
                &prediction.input_projection_digest,
                "prediction.input_projection_digest",
            )?;
            if prediction.predictor_digest != predictor_digest {
                anyhow::bail!("prediction predictor digest does not match provenance")
            }
            prediction
                .context
                .validate(self.predictor.confidence_kind)?;
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        canonical_digest(&("bitrouter.shadow-classifier-submission.v1", self))
    }
}

fn validate_sorted_identifiers(values: &[String], field: &str) -> Result<()> {
    let mut unique = BTreeSet::new();
    let mut previous: Option<&str> = None;
    for value in values {
        validate_identifier(value, field)?;
        if previous.is_some_and(|prior| prior >= value.as_str()) || !unique.insert(value) {
            anyhow::bail!("{field} must be unique and canonically ordered")
        }
        previous = Some(value);
    }
    Ok(())
}

pub(crate) fn validate_identifier(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        anyhow::bail!("{field} must be a non-empty bounded identifier")
    }
    Ok(())
}

pub(crate) fn validate_digest(value: &str, field: &str) -> Result<()> {
    let Some(hex_digest) = value.strip_prefix("sha256:") else {
        anyhow::bail!("{field} must be a sha256 digest")
    };
    if hex_digest.len() != 64
        || !hex_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        anyhow::bail!("{field} must contain 64 lowercase hexadecimal digits")
    }
    Ok(())
}

fn validate_optional_digest(value: Option<&str>, field: &str) -> Result<()> {
    if let Some(value) = value {
        validate_digest(value, field)?;
    }
    Ok(())
}

fn validate_ppm(value: u32, field: &str) -> Result<()> {
    if value > PROBABILITY_TOTAL_PPM {
        anyhow::bail!("{field} must be between 0 and {PROBABILITY_TOTAL_PPM}")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(byte: char) -> String {
        format!("sha256:{}", byte.to_string().repeat(64))
    }

    fn head(labels: &[&str], predicted: &str) -> CategoricalHead {
        let selected = labels
            .iter()
            .position(|label| *label == predicted)
            .unwrap_or_default();
        CategoricalHead {
            predicted_label: predicted.to_owned(),
            probabilities: labels
                .iter()
                .enumerate()
                .map(|(index, label)| ClassProbability {
                    label: (*label).to_owned(),
                    probability_ppm: u32::from(index == selected) * PROBABILITY_TOTAL_PPM,
                })
                .collect(),
        }
    }

    fn submission() -> ShadowClassifierSubmission {
        let predictor = ShadowPredictorProvenance {
            kind: ShadowPredictorKind::EmbeddingPrototype,
            name: "test-vector".into(),
            version: "1".into(),
            artifact_digest: digest('a'),
            feature_digest: digest('b'),
            confidence_kind: ShadowConfidenceKind::CalibratedProbability,
            calibration_digest: Some(digest('c')),
            calibration_split_digest: Some(digest('1')),
            training_split_digest: Some(digest('d')),
            resources: None,
        };
        let predictor_digest = predictor.digest().unwrap_or_default();
        ShadowClassifierSubmission {
            schema_version: SHADOW_CLASSIFIER_SCHEMA_VERSION,
            dataset_digest: digest('e'),
            predictor,
            predictions: vec![ShadowClassifierPrediction {
                fixture_id: "fixture-1".into(),
                dataset_digest: digest('e'),
                input_projection_digest: digest('f'),
                predictor_digest,
                context: RouteContextV3 {
                    schema_version: ROUTE_CONTEXT_SCHEMA_VERSION,
                    task_family: head(TASK_FAMILY_LABELS, "code:debugging"),
                    next_step_role: head(ROLE_LABELS, "implement"),
                    progress_state: head(PROGRESS_LABELS, "opening"),
                    route_risk: head(RISK_LABELS, "normal"),
                    capabilities: vec![],
                    ood: Some(OodAssessment {
                        score_ppm: 100_000,
                        threshold_ppm: 800_000,
                        is_ood: false,
                    }),
                    abstained: false,
                    abstention_reason: None,
                    exemplar_ids: vec!["exemplar-1".into()],
                    risk_authority: RiskAuthority::DeterministicRules,
                },
            }],
        }
    }

    #[test]
    fn admits_a_complete_calibrated_submission() {
        assert!(submission().validate().is_ok());
    }

    #[test]
    fn rejects_noncanonical_probabilities_and_forged_provenance() {
        let mut reordered = submission();
        reordered.predictions[0]
            .context
            .task_family
            .probabilities
            .swap(0, 1);
        assert!(reordered.validate().is_err());

        let mut wrong_total = submission();
        wrong_total.predictions[0]
            .context
            .next_step_role
            .probabilities[0]
            .probability_ppm = 1;
        assert!(wrong_total.validate().is_err());

        let mut forged = submission();
        forged.predictions[0].predictor_digest = digest('0');
        assert!(forged.validate().is_err());
    }

    #[test]
    fn heuristic_margin_cannot_masquerade_as_probability() {
        let mut value = submission();
        value.predictor.confidence_kind = ShadowConfidenceKind::HeuristicMargin;
        value.predictor.calibration_digest = None;
        value.predictor.calibration_split_digest = None;
        value.predictions[0].predictor_digest = value.predictor.digest().unwrap_or_default();
        assert!(value.validate().is_err());
    }

    #[test]
    fn abstention_and_ood_are_explicit_and_consistent() {
        let mut missing_reason = submission();
        missing_reason.predictions[0].context.abstained = true;
        assert!(missing_reason.validate().is_err());

        let mut wrong_ood = submission();
        wrong_ood.predictions[0].context.ood = Some(OodAssessment {
            score_ppm: 900_000,
            threshold_ppm: 800_000,
            is_ood: false,
        });
        assert!(wrong_ood.validate().is_err());
    }
}
