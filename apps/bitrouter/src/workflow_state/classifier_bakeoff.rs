//! Deterministic offline evaluation for shadow-classifier submissions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::classifier_baseline::{
    ClassifierBaselineManifest, ClassifierEvaluationCase, selected_research_fixtures,
};
use super::fixture::WorkflowTraceFixture;
use super::predictive::{compiled_predictor_contract, predict_next_step};
use super::replay::extract_fixture_ir;
use super::shadow_classifier::{
    CategoricalHead, ROUTE_CONTEXT_SCHEMA_VERSION, RiskAuthority, RouteContextV3,
    SHADOW_CLASSIFIER_SCHEMA_VERSION, ShadowClassifierPrediction, ShadowClassifierSubmission,
    ShadowConfidenceKind, ShadowPredictorKind, ShadowPredictorProvenance,
};
use crate::eval::types::canonical_digest;
use crate::workflow_state::predictive::TaskFamily;

pub const CLASSIFIER_BAKEOFF_SCHEMA_VERSION: u32 = 1;
const DECISION_WEIGHT_TASK: u32 = 4;
const DECISION_WEIGHT_ROLE: u32 = 3;
const DECISION_WEIGHT_PROGRESS: u32 = 2;
const DECISION_WEIGHT_RISK: u32 = 8;
const DECISION_WEIGHT_MAX: u32 =
    DECISION_WEIGHT_TASK + DECISION_WEIGHT_ROLE + DECISION_WEIGHT_PROGRESS + DECISION_WEIGHT_RISK;
const DECISION_WEIGHT_ABSTENTION: u32 = DECISION_WEIGHT_MAX;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifierLabelReport {
    pub support_count: usize,
    pub true_positive: usize,
    pub false_positive: usize,
    pub false_negative: usize,
    pub recall_ppm: u32,
    pub f1_ppm: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifierHeadReport {
    pub exact_count: usize,
    pub error_count: usize,
    pub macro_f1_ppm: u32,
    pub by_label: BTreeMap<String, ClassifierLabelReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifierSliceReport {
    pub total_count: usize,
    pub accepted_count: usize,
    pub accepted_error_count: usize,
    pub coverage_ppm: u32,
    pub accepted_error_risk_ppm: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadCalibrationReport {
    pub observation_count: usize,
    pub multiclass_brier_ppm: u32,
    pub top_label_ece_ppm: u32,
    pub bin_count: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationReport {
    pub task_family: HeadCalibrationReport,
    pub next_step_role: HeadCalibrationReport,
    pub progress_state: HeadCalibrationReport,
    pub route_risk: HeadCalibrationReport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OodDetectionReport {
    pub true_positive: usize,
    pub true_negative: usize,
    pub false_positive: usize,
    pub false_negative: usize,
    pub accuracy_ppm: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionWeightedLossReport {
    pub task_error_weight: u32,
    pub role_error_weight: u32,
    pub progress_error_weight: u32,
    pub risk_error_weight: u32,
    pub abstention_weight: u32,
    pub incurred_weight_units: u64,
    pub maximum_weight_units: u64,
    pub normalized_loss_ppm: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifierBakeoffReport {
    pub schema_version: u32,
    pub dataset_digest: String,
    pub submission_digest: String,
    pub predictor_digest: String,
    pub total_count: usize,
    pub accepted_count: usize,
    pub abstained_count: usize,
    pub coverage_ppm: u32,
    pub all_heads_exact_count: usize,
    pub task_family: ClassifierHeadReport,
    pub next_step_role: ClassifierHeadReport,
    pub progress_state: ClassifierHeadReport,
    pub route_risk: ClassifierHeadReport,
    pub slices: BTreeMap<String, ClassifierSliceReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration: Option<CalibrationReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ood_detection: Option<OodDetectionReport>,
    pub decision_weighted_loss: DecisionWeightedLossReport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<super::shadow_classifier::ResourceMeasurements>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifierBakeoffArtifact {
    pub schema_version: u32,
    pub manifest: ClassifierBaselineManifest,
    pub report: ClassifierBakeoffReport,
}

impl ClassifierBakeoffArtifact {
    pub fn build(
        fixtures: &[WorkflowTraceFixture],
        submission: Option<ShadowClassifierSubmission>,
    ) -> Result<Self> {
        let manifest = ClassifierBaselineManifest::from_fixtures(fixtures)?;
        let (submission, allow_compiled_scorecard) = match submission {
            Some(submission) => (submission, false),
            None => (current_scorecard_submission(fixtures, &manifest)?, true),
        };
        let report = evaluate_submission(&manifest, &submission, allow_compiled_scorecard)?;
        Ok(Self {
            schema_version: CLASSIFIER_BAKEOFF_SCHEMA_VERSION,
            manifest,
            report,
        })
    }

    pub fn write_json(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let encoded = serde_json::to_vec_pretty(self).context("serialize classifier bake-off")?;
        std::fs::write(path, encoded)
            .with_context(|| format!("write classifier bake-off {}", path.display()))
    }
}

fn evaluate_submission(
    manifest: &ClassifierBaselineManifest,
    submission: &ShadowClassifierSubmission,
    allow_compiled_scorecard: bool,
) -> Result<ClassifierBakeoffReport> {
    submission.validate()?;
    validate_submission_against_manifest(manifest, submission, allow_compiled_scorecard)?;
    let submission_digest = submission.digest()?;
    let predictor_digest = submission.predictor.digest()?;
    let total_count = manifest.evaluation_cases.len();
    let accepted_count = submission
        .predictions
        .iter()
        .filter(|prediction| !prediction.context.abstained)
        .count();
    let all_heads_exact_count = manifest
        .evaluation_cases
        .iter()
        .zip(&submission.predictions)
        .filter(|(case, prediction)| all_heads_correct(case, &prediction.context))
        .count();
    let task_family = head_report(
        &manifest.evaluation_cases,
        &submission.predictions,
        |case| case.task_family.as_str(),
        |context| &context.task_family,
    );
    let next_step_role = head_report(
        &manifest.evaluation_cases,
        &submission.predictions,
        |case| case.next_step_role.as_str(),
        |context| &context.next_step_role,
    );
    let progress_state = head_report(
        &manifest.evaluation_cases,
        &submission.predictions,
        |case| case.progress_state.as_str(),
        |context| &context.progress_state,
    );
    let route_risk = head_report(
        &manifest.evaluation_cases,
        &submission.predictions,
        |case| case.route_risk.as_str(),
        |context| &context.route_risk,
    );
    Ok(ClassifierBakeoffReport {
        schema_version: CLASSIFIER_BAKEOFF_SCHEMA_VERSION,
        dataset_digest: manifest.dataset_digest.clone(),
        submission_digest,
        predictor_digest,
        total_count,
        accepted_count,
        abstained_count: total_count.saturating_sub(accepted_count),
        coverage_ppm: ratio_ppm(accepted_count as u128, total_count as u128),
        all_heads_exact_count,
        task_family,
        next_step_role,
        progress_state,
        route_risk,
        slices: slice_reports(&manifest.evaluation_cases, &submission.predictions),
        calibration: calibration_report(manifest, submission),
        ood_detection: ood_detection_report(manifest, submission),
        decision_weighted_loss: decision_weighted_loss(manifest, submission),
        resources: submission.predictor.resources.clone(),
    })
}

fn validate_submission_against_manifest(
    manifest: &ClassifierBaselineManifest,
    submission: &ShadowClassifierSubmission,
    allow_compiled_scorecard: bool,
) -> Result<()> {
    if submission.dataset_digest != manifest.dataset_digest {
        anyhow::bail!("submission dataset digest does not match manifest")
    }
    if submission.predictions.len() != manifest.evaluation_cases.len() {
        anyhow::bail!("submission must contain exactly one prediction per evaluation case")
    }
    for (prediction, case) in submission
        .predictions
        .iter()
        .zip(&manifest.evaluation_cases)
    {
        if prediction.fixture_id != case.fixture_id {
            anyhow::bail!("submission fixture order or identity does not match manifest")
        }
        if prediction.input_projection_digest != case.input_projection_digest {
            anyhow::bail!("submission input projection does not match manifest")
        }
    }
    if submission.predictor.kind != ShadowPredictorKind::DeterministicScorecard {
        let training = submission
            .predictor
            .training_split_digest
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("learned predictor requires a training split digest"))?;
        if training == manifest.dataset_digest {
            anyhow::bail!("evaluation dataset cannot also be the training split")
        }
        if submission.predictor.confidence_kind != ShadowConfidenceKind::CalibratedProbability {
            anyhow::bail!("learned candidates must provide calibrated probabilities")
        }
        if submission
            .predictions
            .iter()
            .any(|prediction| prediction.context.ood.is_none())
        {
            anyhow::bail!("learned candidates must assess OOD for every evaluation case")
        }
    } else if !allow_compiled_scorecard {
        anyhow::bail!("external submissions cannot claim the compiled scorecard kind")
    } else if submission.predictor != current_scorecard_provenance()? {
        anyhow::bail!("internal scorecard provenance must match the compiled predictor")
    }
    if submission.predictor.confidence_kind == ShadowConfidenceKind::CalibratedProbability {
        let calibration = submission
            .predictor
            .calibration_split_digest
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("calibrated predictor requires a calibration split"))?;
        if calibration == manifest.dataset_digest
            || submission.predictor.training_split_digest.as_deref() == Some(calibration)
        {
            anyhow::bail!("calibration, training, and evaluation splits must be distinct")
        }
    }
    Ok(())
}

fn current_scorecard_submission(
    fixtures: &[WorkflowTraceFixture],
    manifest: &ClassifierBaselineManifest,
) -> Result<ShadowClassifierSubmission> {
    let predictor = current_scorecard_provenance()?;
    let predictor_digest = predictor.digest()?;
    let selected = selected_research_fixtures(fixtures);
    let predictions = selected
        .iter()
        .zip(&manifest.evaluation_cases)
        .map(|(fixture, case)| {
            let observed = extract_fixture_ir(fixture);
            let predicted = predict_next_step(&observed, &fixture.prompt);
            let abstained = predicted.task_family == TaskFamily::Unknown;
            ShadowClassifierPrediction {
                fixture_id: case.fixture_id.clone(),
                dataset_digest: manifest.dataset_digest.clone(),
                input_projection_digest: case.input_projection_digest.clone(),
                predictor_digest: predictor_digest.clone(),
                context: RouteContextV3 {
                    schema_version: ROUTE_CONTEXT_SCHEMA_VERSION,
                    task_family: heuristic_head(predicted.task_family.key()),
                    next_step_role: heuristic_head(predicted.next_step_role.key()),
                    progress_state: heuristic_head(predicted.progress_state.key()),
                    route_risk: heuristic_head(&predicted.route_risk.to_string()),
                    capabilities: Vec::new(),
                    ood: None,
                    abstained,
                    abstention_reason: abstained.then(|| "scorecard_unknown_task".to_owned()),
                    exemplar_ids: Vec::new(),
                    risk_authority: RiskAuthority::DeterministicRules,
                },
            }
        })
        .collect();
    Ok(ShadowClassifierSubmission {
        schema_version: SHADOW_CLASSIFIER_SCHEMA_VERSION,
        dataset_digest: manifest.dataset_digest.clone(),
        predictor,
        predictions,
    })
}

fn current_scorecard_provenance() -> Result<ShadowPredictorProvenance> {
    let contract = compiled_predictor_contract();
    Ok(ShadowPredictorProvenance {
        kind: ShadowPredictorKind::DeterministicScorecard,
        name: contract.algorithm,
        version: contract.version.to_string(),
        artifact_digest: contract.config_digest.clone(),
        feature_digest: canonical_digest(&(
            "bitrouter.scorecard-feature-projection.v1",
            &contract.config_digest,
        ))?,
        confidence_kind: ShadowConfidenceKind::HeuristicMargin,
        calibration_digest: None,
        calibration_split_digest: None,
        training_split_digest: None,
        resources: None,
    })
}

fn heuristic_head(label: &str) -> CategoricalHead {
    CategoricalHead {
        predicted_label: label.to_owned(),
        probabilities: Vec::new(),
    }
}

fn head_report<Expected, Head>(
    cases: &[ClassifierEvaluationCase],
    predictions: &[ShadowClassifierPrediction],
    expected: Expected,
    head: Head,
) -> ClassifierHeadReport
where
    Expected: Fn(&ClassifierEvaluationCase) -> &str,
    Head: Fn(&RouteContextV3) -> &CategoricalHead,
{
    let pairs = cases
        .iter()
        .zip(predictions)
        .map(|(case, prediction)| {
            (
                expected(case).to_owned(),
                head(&prediction.context).predicted_label.clone(),
            )
        })
        .collect::<Vec<_>>();
    let exact_count = pairs
        .iter()
        .filter(|(expected, predicted)| expected == predicted)
        .count();
    let labels = pairs
        .iter()
        .flat_map(|(expected, predicted)| [expected.as_str(), predicted.as_str()])
        .collect::<BTreeSet<_>>();
    let by_label = labels
        .into_iter()
        .map(|label| {
            let true_positive = pairs
                .iter()
                .filter(|(expected, predicted)| expected == label && predicted == label)
                .count();
            let false_positive = pairs
                .iter()
                .filter(|(expected, predicted)| expected != label && predicted == label)
                .count();
            let false_negative = pairs
                .iter()
                .filter(|(expected, predicted)| expected == label && predicted != label)
                .count();
            let support_count = true_positive + false_negative;
            (
                label.to_owned(),
                ClassifierLabelReport {
                    support_count,
                    true_positive,
                    false_positive,
                    false_negative,
                    recall_ppm: ratio_ppm(true_positive as u128, support_count as u128),
                    f1_ppm: ratio_ppm(
                        (2 * true_positive) as u128,
                        (2 * true_positive + false_positive + false_negative) as u128,
                    ),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let macro_f1_ppm = by_label
        .values()
        .map(|label| u128::from(label.f1_ppm))
        .sum::<u128>()
        .checked_div(by_label.len() as u128)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_default();
    ClassifierHeadReport {
        exact_count,
        error_count: pairs.len().saturating_sub(exact_count),
        macro_f1_ppm,
        by_label,
    }
}

fn slice_reports(
    cases: &[ClassifierEvaluationCase],
    predictions: &[ShadowClassifierPrediction],
) -> BTreeMap<String, ClassifierSliceReport> {
    let mut members = BTreeMap::<String, Vec<usize>>::new();
    for (index, case) in cases.iter().enumerate() {
        for slice in &case.research_slices {
            members.entry(slice.clone()).or_default().push(index);
        }
    }
    members
        .into_iter()
        .map(|(slice, indexes)| {
            let accepted = indexes
                .iter()
                .filter(|index| !predictions[**index].context.abstained)
                .count();
            let accepted_errors = indexes
                .iter()
                .filter(|index| {
                    let prediction = &predictions[**index];
                    !prediction.context.abstained
                        && !all_heads_correct(&cases[**index], &prediction.context)
                })
                .count();
            let report = ClassifierSliceReport {
                total_count: indexes.len(),
                accepted_count: accepted,
                accepted_error_count: accepted_errors,
                coverage_ppm: ratio_ppm(accepted as u128, indexes.len() as u128),
                accepted_error_risk_ppm: ratio_ppm(accepted_errors as u128, accepted as u128),
            };
            (slice, report)
        })
        .collect()
}

fn all_heads_correct(case: &ClassifierEvaluationCase, context: &RouteContextV3) -> bool {
    context.task_family.predicted_label == case.task_family
        && context.next_step_role.predicted_label == case.next_step_role
        && context.progress_state.predicted_label == case.progress_state
        && context.route_risk.predicted_label == case.route_risk
}

fn calibration_report(
    manifest: &ClassifierBaselineManifest,
    submission: &ShadowClassifierSubmission,
) -> Option<CalibrationReport> {
    if submission.predictor.confidence_kind != ShadowConfidenceKind::CalibratedProbability {
        return None;
    }
    Some(CalibrationReport {
        task_family: head_calibration_report(
            &manifest.evaluation_cases,
            &submission.predictions,
            |case| case.task_family.as_str(),
            |context| &context.task_family,
        ),
        next_step_role: head_calibration_report(
            &manifest.evaluation_cases,
            &submission.predictions,
            |case| case.next_step_role.as_str(),
            |context| &context.next_step_role,
        ),
        progress_state: head_calibration_report(
            &manifest.evaluation_cases,
            &submission.predictions,
            |case| case.progress_state.as_str(),
            |context| &context.progress_state,
        ),
        route_risk: head_calibration_report(
            &manifest.evaluation_cases,
            &submission.predictions,
            |case| case.route_risk.as_str(),
            |context| &context.route_risk,
        ),
    })
}

fn head_calibration_report<Expected, Head>(
    cases: &[ClassifierEvaluationCase],
    predictions: &[ShadowClassifierPrediction],
    expected: Expected,
    head: Head,
) -> HeadCalibrationReport
where
    Expected: Fn(&ClassifierEvaluationCase) -> &str,
    Head: Fn(&RouteContextV3) -> &CategoricalHead,
{
    let mut squared_error = 0_u128;
    let mut bins = [(0_u128, 0_u128, 0_u128); 10];
    for (case, prediction) in cases.iter().zip(predictions) {
        let expected = expected(case);
        let head = head(&prediction.context);
        for class in &head.probabilities {
            let target = u128::from(class.label == expected) * 1_000_000;
            squared_error += u128::from(class.probability_ppm).abs_diff(target).pow(2);
        }
        let confidence = head.top_probability_ppm().unwrap_or_default();
        let bin = usize::min((confidence / 100_000) as usize, 9);
        bins[bin].0 += 1;
        bins[bin].1 += u128::from(confidence);
        bins[bin].2 += u128::from(head.predicted_label == expected);
    }
    let observations = cases.len();
    let brier = if observations == 0 {
        0
    } else {
        squared_error / (observations as u128 * 1_000_000)
    };
    let ece_numerator = bins
        .iter()
        .map(|(count, confidence_sum, correct)| {
            if *count == 0 {
                0
            } else {
                confidence_sum.abs_diff(correct * 1_000_000)
            }
        })
        .sum::<u128>();
    HeadCalibrationReport {
        observation_count: observations,
        multiclass_brier_ppm: u32::try_from(brier).unwrap_or(u32::MAX),
        top_label_ece_ppm: ratio_ppm(ece_numerator, observations as u128),
        bin_count: 10,
    }
}

fn ood_detection_report(
    manifest: &ClassifierBaselineManifest,
    submission: &ShadowClassifierSubmission,
) -> Option<OodDetectionReport> {
    if submission
        .predictions
        .iter()
        .any(|prediction| prediction.context.ood.is_none())
    {
        return None;
    }
    let mut report = OodDetectionReport {
        true_positive: 0,
        true_negative: 0,
        false_positive: 0,
        false_negative: 0,
        accuracy_ppm: 0,
    };
    for (case, prediction) in manifest
        .evaluation_cases
        .iter()
        .zip(&submission.predictions)
    {
        let expected = case.research_slices.iter().any(|slice| slice == "ood");
        let predicted = prediction
            .context
            .ood
            .as_ref()
            .is_some_and(|assessment| assessment.is_ood);
        match (expected, predicted) {
            (true, true) => report.true_positive += 1,
            (false, false) => report.true_negative += 1,
            (false, true) => report.false_positive += 1,
            (true, false) => report.false_negative += 1,
        }
    }
    report.accuracy_ppm = ratio_ppm(
        (report.true_positive + report.true_negative) as u128,
        manifest.evaluation_cases.len() as u128,
    );
    Some(report)
}

fn decision_weighted_loss(
    manifest: &ClassifierBaselineManifest,
    submission: &ShadowClassifierSubmission,
) -> DecisionWeightedLossReport {
    let incurred = manifest
        .evaluation_cases
        .iter()
        .zip(&submission.predictions)
        .map(|(case, prediction)| {
            let context = &prediction.context;
            if context.abstained {
                return u64::from(DECISION_WEIGHT_ABSTENTION);
            }
            u64::from(context.task_family.predicted_label != case.task_family)
                * u64::from(DECISION_WEIGHT_TASK)
                + u64::from(context.next_step_role.predicted_label != case.next_step_role)
                    * u64::from(DECISION_WEIGHT_ROLE)
                + u64::from(context.progress_state.predicted_label != case.progress_state)
                    * u64::from(DECISION_WEIGHT_PROGRESS)
                + u64::from(context.route_risk.predicted_label != case.route_risk)
                    * u64::from(DECISION_WEIGHT_RISK)
        })
        .sum::<u64>();
    let maximum = manifest.evaluation_cases.len() as u64 * u64::from(DECISION_WEIGHT_MAX);
    DecisionWeightedLossReport {
        task_error_weight: DECISION_WEIGHT_TASK,
        role_error_weight: DECISION_WEIGHT_ROLE,
        progress_error_weight: DECISION_WEIGHT_PROGRESS,
        risk_error_weight: DECISION_WEIGHT_RISK,
        abstention_weight: DECISION_WEIGHT_ABSTENTION,
        incurred_weight_units: incurred,
        maximum_weight_units: maximum,
        normalized_loss_ppm: ratio_ppm(incurred as u128, maximum as u128),
    }
}

fn ratio_ppm(numerator: u128, denominator: u128) -> u32 {
    numerator
        .saturating_mul(1_000_000)
        .checked_div(denominator)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow_state::shadow_classifier::{
        ClassProbability, OodAssessment, PROGRESS_LABELS, RISK_LABELS, ROLE_LABELS,
        TASK_FAMILY_LABELS,
    };

    fn digest(byte: char) -> String {
        format!("sha256:{}", byte.to_string().repeat(64))
    }

    fn head(labels: &[&str], predicted: &str) -> CategoricalHead {
        let selected = labels
            .iter()
            .position(|label| *label == predicted)
            .unwrap_or_default();
        CategoricalHead {
            predicted_label: predicted.into(),
            probabilities: labels
                .iter()
                .enumerate()
                .map(|(index, label)| ClassProbability {
                    label: (*label).into(),
                    probability_ppm: u32::from(index == selected) * 1_000_000,
                })
                .collect(),
        }
    }

    fn manifest() -> ClassifierBaselineManifest {
        ClassifierBaselineManifest {
            schema_version: 2,
            dataset_digest: digest('a'),
            fixture_count: 2,
            by_slice: BTreeMap::from([("english".into(), 2), ("ood".into(), 1)]),
            current_predictor_exact_count: 0,
            current_predictor_mismatch_count: 2,
            evaluation_cases: vec![
                ClassifierEvaluationCase {
                    fixture_id: "a".into(),
                    input_projection_digest: digest('b'),
                    task_family: "code:debugging".into(),
                    next_step_role: "implement".into(),
                    progress_state: "opening".into(),
                    route_risk: "guarded".into(),
                    research_slices: vec!["english".into()],
                },
                ClassifierEvaluationCase {
                    fixture_id: "b".into(),
                    input_projection_digest: digest('c'),
                    task_family: "unknown".into(),
                    next_step_role: "finalize".into(),
                    progress_state: "opening".into(),
                    route_risk: "normal".into(),
                    research_slices: vec!["english".into(), "ood".into()],
                },
            ],
        }
    }

    fn submission(manifest: &ClassifierBaselineManifest) -> ShadowClassifierSubmission {
        let predictor = ShadowPredictorProvenance {
            kind: ShadowPredictorKind::EmbeddingPrototype,
            name: "synthetic-test-vector".into(),
            version: "1".into(),
            artifact_digest: digest('d'),
            feature_digest: digest('e'),
            confidence_kind: ShadowConfidenceKind::CalibratedProbability,
            calibration_digest: Some(digest('f')),
            calibration_split_digest: Some(digest('1')),
            training_split_digest: Some(digest('2')),
            resources: None,
        };
        let predictor_digest = predictor.digest().unwrap_or_default();
        let predictions = manifest
            .evaluation_cases
            .iter()
            .map(|case| ShadowClassifierPrediction {
                fixture_id: case.fixture_id.clone(),
                dataset_digest: manifest.dataset_digest.clone(),
                input_projection_digest: case.input_projection_digest.clone(),
                predictor_digest: predictor_digest.clone(),
                context: RouteContextV3 {
                    schema_version: ROUTE_CONTEXT_SCHEMA_VERSION,
                    task_family: head(TASK_FAMILY_LABELS, &case.task_family),
                    next_step_role: head(ROLE_LABELS, &case.next_step_role),
                    progress_state: head(PROGRESS_LABELS, &case.progress_state),
                    route_risk: head(RISK_LABELS, &case.route_risk),
                    capabilities: Vec::new(),
                    ood: Some(OodAssessment {
                        score_ppm: u32::from(
                            case.research_slices.iter().any(|slice| slice == "ood"),
                        ) * 1_000_000,
                        threshold_ppm: 500_000,
                        is_ood: case.research_slices.iter().any(|slice| slice == "ood"),
                    }),
                    abstained: false,
                    abstention_reason: None,
                    exemplar_ids: Vec::new(),
                    risk_authority: RiskAuthority::DeterministicRules,
                },
            })
            .collect();
        ShadowClassifierSubmission {
            schema_version: SHADOW_CLASSIFIER_SCHEMA_VERSION,
            dataset_digest: manifest.dataset_digest.clone(),
            predictor,
            predictions,
        }
    }

    #[test]
    fn perfect_test_vector_has_complete_deterministic_metrics() {
        let manifest = manifest();
        let submission = submission(&manifest);
        let report = evaluate_submission(&manifest, &submission, false).unwrap();
        assert_eq!(report.all_heads_exact_count, 2);
        assert_eq!(report.task_family.macro_f1_ppm, 1_000_000);
        assert_eq!(report.route_risk.by_label["guarded"].recall_ppm, 1_000_000);
        assert_eq!(report.coverage_ppm, 1_000_000);
        let calibration = report.calibration.unwrap();
        assert_eq!(calibration.task_family.multiclass_brier_ppm, 0);
        assert_eq!(calibration.next_step_role.multiclass_brier_ppm, 0);
        assert_eq!(calibration.progress_state.multiclass_brier_ppm, 0);
        assert_eq!(calibration.route_risk.multiclass_brier_ppm, 0);
        assert_eq!(report.ood_detection.unwrap().accuracy_ppm, 1_000_000);
        assert_eq!(report.decision_weighted_loss.normalized_loss_ppm, 0);
    }

    #[test]
    fn rejects_missing_reordered_and_leaking_predictions() {
        let manifest = manifest();
        let mut missing = submission(&manifest);
        missing.predictions.pop();
        assert!(evaluate_submission(&manifest, &missing, false).is_err());

        let mut reordered = submission(&manifest);
        reordered.predictions.swap(0, 1);
        assert!(evaluate_submission(&manifest, &reordered, false).is_err());

        let mut leaking = submission(&manifest);
        leaking.predictor.training_split_digest = Some(manifest.dataset_digest.clone());
        let predictor_digest = leaking.predictor.digest().unwrap_or_default();
        for prediction in &mut leaking.predictions {
            prediction.predictor_digest = predictor_digest.clone();
        }
        assert!(evaluate_submission(&manifest, &leaking, false).is_err());

        let mut false_scorecard = submission(&manifest);
        false_scorecard.predictor.kind = ShadowPredictorKind::DeterministicScorecard;
        let predictor_digest = false_scorecard.predictor.digest().unwrap_or_default();
        for prediction in &mut false_scorecard.predictions {
            prediction.predictor_digest = predictor_digest.clone();
        }
        assert!(evaluate_submission(&manifest, &false_scorecard, false).is_err());

        let mut missing_ood = submission(&manifest);
        missing_ood.predictions[0].context.ood = None;
        assert!(evaluate_submission(&manifest, &missing_ood, false).is_err());
    }

    #[test]
    fn abstention_is_coverage_not_an_unknown_label() {
        let manifest = manifest();
        let mut submission = submission(&manifest);
        submission.predictions[0].context.abstained = true;
        submission.predictions[0].context.abstention_reason = Some("low_confidence".into());
        let report = evaluate_submission(&manifest, &submission, false).unwrap();
        assert_eq!(report.accepted_count, 1);
        assert_eq!(report.coverage_ppm, 500_000);
        assert_eq!(report.task_family.exact_count, 2);
        assert_eq!(report.decision_weighted_loss.incurred_weight_units, 17);
    }
}
