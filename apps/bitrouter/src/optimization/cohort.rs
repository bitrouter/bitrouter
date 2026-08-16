use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};

use crate::eval::compiler::{EvalEvidenceRecord, EvalEvidenceSnapshot};
use crate::eval::types::{
    EvalScope, EvalVerdict, ExperimentArm, ExperimentAssignmentUnit, MetricUnit,
};
use crate::optimization::exploration::RouteExploration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CohortGateVerdict {
    Pass,
    InsufficientEvidence,
    QualityFailed,
    HardViolation,
    AmbiguousEvaluator,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArmAssessment {
    pub observed: u32,
    pub eligible: u32,
    pub excluded: u32,
    pub conclusive: u32,
    pub pass: u32,
    pub fail: u32,
    pub hard_violations: u32,
    pub pass_rate_ppm: Option<u32>,
    pub mean_cost_micro_usd: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CohortAssessment {
    pub control: ArmAssessment,
    pub challenger: ArmAssessment,
    pub excluded_requests: u32,
    pub evaluator_config_digest: Option<String>,
    pub cost_delta_micro_usd: Option<i64>,
    pub verdict: CohortGateVerdict,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SubjectKey {
    scope: EvalScope,
    subject_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct NormalizedReference {
    unit: ExperimentAssignmentUnit,
    digest: String,
    arm: ExperimentArm,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct UnitKey {
    subject_scope: EvalScope,
    unit: ExperimentAssignmentUnit,
    digest: String,
    subject_id: String,
}

#[derive(Default)]
struct SubjectGroup<'a> {
    malformed: bool,
    references: BTreeSet<NormalizedReference>,
    records: Vec<&'a EvalEvidenceRecord>,
}

struct UnitGroup<'a> {
    arm: ExperimentArm,
    records: Vec<&'a EvalEvidenceRecord>,
}

enum ConclusiveRecord<'a> {
    None,
    Consistent(&'a EvalEvidenceRecord),
    Conflict,
}

#[derive(Default)]
struct CostAggregate {
    total: i128,
    count: u32,
}

impl CostAggregate {
    fn add(&mut self, value: i64) -> Result<()> {
        self.total = self
            .total
            .checked_add(i128::from(value))
            .context("summing complete cohort costs")?;
        self.count = self
            .count
            .checked_add(1)
            .context("counting complete cohort costs")?;
        Ok(())
    }

    fn mean(&self) -> Option<i64> {
        if self.count == 0 {
            return None;
        }
        i64::try_from(self.total / i128::from(self.count)).ok()
    }
}

pub fn assess_cohort(
    snapshot: &EvalEvidenceSnapshot,
    active_policy_digest: &str,
    exploration: &RouteExploration,
) -> Result<CohortAssessment> {
    let mut subjects = BTreeMap::<SubjectKey, SubjectGroup<'_>>::new();
    let mut excluded_request_subjects = BTreeSet::<SubjectKey>::new();
    for record in &snapshot.records {
        let target_decisions = record
            .subject
            .decisions
            .iter()
            .filter(|decision| decision.request_key == exploration.target_request_key)
            .collect::<Vec<_>>();
        if target_decisions.is_empty() {
            continue;
        }
        if record.subject.scope == EvalScope::Request {
            excluded_request_subjects.insert(SubjectKey {
                scope: record.subject.scope,
                subject_id: record.subject.subject_id.clone(),
            });
            continue;
        }
        let group = subjects
            .entry(SubjectKey {
                scope: record.subject.scope,
                subject_id: record.subject.subject_id.clone(),
            })
            .or_default();
        if record.subject.policy_digest != active_policy_digest {
            group.malformed = true;
        }
        for decision in target_decisions {
            let Some(experiment) = decision.experiment.as_ref() else {
                group.malformed = true;
                continue;
            };
            if decision.policy_digest != active_policy_digest
                || experiment.experiment_id != exploration.experiment_id
                || experiment.challenger_propensity_ppm != exploration.challenger_exposure_ppm
            {
                group.malformed = true;
                continue;
            }
            group.references.insert(NormalizedReference {
                unit: experiment.assignment_unit,
                digest: experiment.assignment_id_digest.clone(),
                arm: experiment.arm,
            });
        }
        if !group
            .records
            .iter()
            .any(|existing| existing.result_id == record.result_id)
        {
            group.records.push(record);
        }
    }
    let excluded_requests = u32::try_from(excluded_request_subjects.len())
        .context("counting excluded request eval subjects")?;
    let mut groups = BTreeMap::<UnitKey, UnitGroup<'_>>::new();
    for (subject, group) in subjects {
        if group.malformed || group.references.len() != 1 {
            continue;
        }
        let Some(reference) = group.references.into_iter().next() else {
            continue;
        };
        groups.insert(
            UnitKey {
                subject_scope: subject.scope,
                unit: reference.unit,
                digest: reference.digest,
                subject_id: subject.subject_id,
            },
            UnitGroup {
                arm: reference.arm,
                records: group.records,
            },
        );
    }

    let inferred_configs = groups
        .values()
        .filter_map(|group| match conclusive_record(&group.records, None) {
            ConclusiveRecord::Consistent(record) => Some(record),
            ConclusiveRecord::None | ConclusiveRecord::Conflict => None,
        })
        .map(|record| record.result.evaluator.config_digest.clone())
        .collect::<BTreeSet<_>>();
    let (evaluator_config_digest, ambiguous_evaluator) =
        match exploration.gate.evaluator_config_digest.as_ref() {
            Some(digest) => (Some(digest.clone()), false),
            None if inferred_configs.len() == 1 => (inferred_configs.into_iter().next(), false),
            None if inferred_configs.len() > 1 => (None, true),
            None => (None, false),
        };

    let mut control = ArmAssessment::default();
    let mut challenger = ArmAssessment::default();
    let mut control_cost = CostAggregate::default();
    let mut challenger_cost = CostAggregate::default();
    for group in groups.values_mut() {
        sort_records_by_submitted_at(&mut group.records)?;
        let arm = group.arm;
        let assessment = arm_assessment_mut(arm, &mut control, &mut challenger);
        assessment.observed = assessment
            .observed
            .checked_add(1)
            .context("counting observed experiment units")?;

        if let Some(cost) = latest_complete_cost(&group.records) {
            match arm {
                ExperimentArm::Control => control_cost.add(cost)?,
                ExperimentArm::Challenger => challenger_cost.add(cost)?,
            }
        }

        let hard_violations = group
            .records
            .iter()
            .filter(|record| {
                exploration
                    .gate
                    .evaluator_config_digest
                    .as_ref()
                    .is_none_or(|digest| record.result.evaluator.config_digest == *digest)
            })
            .flat_map(|record| record.result.hard_violations.iter())
            .collect::<BTreeSet<_>>();
        let hard_violation_count = u32::try_from(hard_violations.len()).unwrap_or(u32::MAX);
        assessment.hard_violations = assessment
            .hard_violations
            .checked_add(hard_violation_count)
            .context("counting hard violations")?;

        if ambiguous_evaluator {
            assessment.excluded = assessment
                .excluded
                .checked_add(1)
                .context("counting evaluator-ambiguous experiment units")?;
            continue;
        }

        let has_wrong_evaluator = evaluator_config_digest.as_ref().is_some_and(|digest| {
            group.records.iter().any(|record| {
                record.result.verdict != EvalVerdict::Inconclusive
                    && record.result.evaluator.config_digest != *digest
            })
        });
        if has_wrong_evaluator && exploration.gate.evaluator_config_digest.is_none() {
            assessment.excluded = assessment
                .excluded
                .checked_add(1)
                .context("counting ambiguous evaluator units")?;
            continue;
        }
        let record = match conclusive_record(&group.records, evaluator_config_digest.as_deref()) {
            ConclusiveRecord::Consistent(record) => record,
            ConclusiveRecord::None | ConclusiveRecord::Conflict => {
                assessment.excluded = assessment
                    .excluded
                    .checked_add(1)
                    .context("counting excluded experiment units")?;
                continue;
            }
        };
        let result = &record.result;
        assessment.eligible = assessment
            .eligible
            .checked_add(1)
            .context("counting eligible experiment units")?;
        assessment.conclusive = assessment
            .conclusive
            .checked_add(1)
            .context("counting conclusive experiment units")?;
        match result.verdict {
            EvalVerdict::Pass => {
                assessment.pass = assessment
                    .pass
                    .checked_add(1)
                    .context("counting passing experiment units")?;
            }
            EvalVerdict::Fail => {
                assessment.fail = assessment
                    .fail
                    .checked_add(1)
                    .context("counting failing experiment units")?;
            }
            EvalVerdict::Inconclusive => {}
        }
    }

    control.pass_rate_ppm = pass_rate(&control)?;
    challenger.pass_rate_ppm = pass_rate(&challenger)?;
    control.mean_cost_micro_usd = control_cost.mean();
    challenger.mean_cost_micro_usd = challenger_cost.mean();
    let cost_delta_micro_usd = match (control.mean_cost_micro_usd, challenger.mean_cost_micro_usd) {
        (Some(control), Some(challenger)) => challenger.checked_sub(control),
        _ => None,
    };
    let minimum = exploration.gate.minimum_tasks_per_arm;
    let verdict = if challenger.hard_violations > 0 {
        CohortGateVerdict::HardViolation
    } else if ambiguous_evaluator {
        CohortGateVerdict::AmbiguousEvaluator
    } else if control.eligible < minimum || challenger.eligible < minimum {
        CohortGateVerdict::InsufficientEvidence
    } else if challenger
        .pass_rate_ppm
        .is_none_or(|rate| rate < exploration.gate.minimum_pass_rate_ppm)
    {
        CohortGateVerdict::QualityFailed
    } else {
        CohortGateVerdict::Pass
    };
    Ok(CohortAssessment {
        control,
        challenger,
        excluded_requests,
        evaluator_config_digest,
        cost_delta_micro_usd,
        verdict,
    })
}

fn sort_records_by_submitted_at(records: &mut Vec<&EvalEvidenceRecord>) -> Result<()> {
    let mut normalized = records
        .iter()
        .copied()
        .map(|record| {
            let submitted_at = chrono::DateTime::parse_from_rfc3339(&record.result.submitted_at)
                .with_context(|| {
                    format!("result '{}' submitted_at must be RFC3339", record.result_id)
                })?;
            Ok((submitted_at, record))
        })
        .collect::<Result<Vec<_>>>()?;
    normalized.sort_by(|(left_time, left), (right_time, right)| {
        left_time
            .cmp(right_time)
            .then_with(|| left.result_id.cmp(&right.result_id))
    });
    *records = normalized.into_iter().map(|(_, record)| record).collect();
    Ok(())
}

fn conclusive_record<'a>(
    records: &[&'a EvalEvidenceRecord],
    evaluator_config_digest: Option<&str>,
) -> ConclusiveRecord<'a> {
    let mut selected: Option<&EvalEvidenceRecord> = None;
    for record in records.iter().copied().filter(|record| {
        record.result.verdict != EvalVerdict::Inconclusive
            && evaluator_config_digest
                .is_none_or(|digest| record.result.evaluator.config_digest == digest)
    }) {
        let Some(current) = selected else {
            selected = Some(record);
            continue;
        };
        if current.result.verdict != record.result.verdict
            || current.result.evaluator.config_digest != record.result.evaluator.config_digest
            || current
                .result
                .hard_violations
                .iter()
                .collect::<BTreeSet<_>>()
                != record
                    .result
                    .hard_violations
                    .iter()
                    .collect::<BTreeSet<_>>()
        {
            return ConclusiveRecord::Conflict;
        }
        selected = Some(record);
    }
    match selected {
        Some(record) => ConclusiveRecord::Consistent(record),
        None => ConclusiveRecord::None,
    }
}

fn arm_assessment_mut<'a>(
    arm: ExperimentArm,
    control: &'a mut ArmAssessment,
    challenger: &'a mut ArmAssessment,
) -> &'a mut ArmAssessment {
    match arm {
        ExperimentArm::Control => control,
        ExperimentArm::Challenger => challenger,
    }
}

fn pass_rate(assessment: &ArmAssessment) -> Result<Option<u32>> {
    if assessment.conclusive == 0 {
        return Ok(None);
    }
    assessment
        .pass
        .checked_mul(1_000_000)
        .context("computing cohort pass rate")?
        .checked_div(assessment.conclusive)
        .map(Some)
        .context("computing cohort pass-rate denominator")
}

fn latest_complete_cost(records: &[&EvalEvidenceRecord]) -> Option<i64> {
    let trajectory = records.iter().rev().find_map(|record| {
        let complete = record
            .result
            .metrics
            .get("trajectory.history_complete")
            .is_some_and(|metric| metric.unit == MetricUnit::Boolean && metric.value == 1);
        complete.then(|| metric_cost(record, "trajectory.cost.usd_micros"))?
    });
    trajectory.or_else(|| {
        records
            .iter()
            .rev()
            .find_map(|record| metric_cost(record, "cost.usd_micros"))
    })
}

fn metric_cost(record: &EvalEvidenceRecord, metric_id: &str) -> Option<i64> {
    record
        .result
        .metrics
        .get(metric_id)
        .filter(|metric| metric.unit == MetricUnit::MicroUsd && metric.value >= 0)
        .map(|metric| metric.value)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use anyhow::Result;

    use crate::eval::compiler::{EvalEvidenceRecord, EvalEvidenceSnapshot};
    use crate::eval::types::{
        EVAL_SCHEMA_VERSION, EvalDecisionRef, EvalExperimentRef, EvalScope, EvalSubject,
        EvalVerdict, EvaluationResult, EvaluatorIdentity, EvaluatorKind, ExperimentArm,
        ExperimentAssignmentUnit, MetricUnit, MetricValue,
    };
    use crate::optimization::exploration::{OptimizationGate, RouteExploration};

    use super::{CohortGateVerdict, assess_cohort};

    const POLICY_DIGEST: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER_POLICY_DIGEST: &str =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const EXPERIMENT_ID: &str =
        "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const OTHER_EXPERIMENT_ID: &str =
        "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    const EVALUATOR_A: &str =
        "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    const EVALUATOR_B: &str =
        "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    const EVIDENCE_DIGEST: &str =
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[derive(Clone)]
    struct RecordSpec<'a> {
        scope: EvalScope,
        subject_id: &'a str,
        assignment_id: &'a str,
        arm: ExperimentArm,
        policy_digest: &'a str,
        experiment_id: &'a str,
        evaluator_digest: &'a str,
        verdict: EvalVerdict,
        hard_violation: bool,
        trajectory_cost: Option<i64>,
        evaluator_cost: Option<i64>,
        submitted_at: &'a str,
        result_suffix: &'a str,
    }

    fn exploration(evaluator_config_digest: Option<&str>) -> RouteExploration {
        RouteExploration {
            experiment_id: EXPERIMENT_ID.into(),
            target_request_key: "agent_trace/v2|verify|normal".into(),
            champion_tier: "strong".into(),
            challenger_tier: "economy".into(),
            challenger_exposure_ppm: 500_000,
            gate: OptimizationGate {
                minimum_tasks_per_arm: 3,
                maximum_challenger_tasks: 20,
                minimum_pass_rate_ppm: 900_000,
                evaluator_config_digest: evaluator_config_digest.map(str::to_owned),
            },
        }
    }

    fn spec(
        subject_id: &str,
        arm: ExperimentArm,
        verdict: EvalVerdict,
        cost: i64,
    ) -> RecordSpec<'_> {
        RecordSpec {
            scope: EvalScope::Task,
            subject_id,
            assignment_id: subject_id,
            arm,
            policy_digest: POLICY_DIGEST,
            experiment_id: EXPERIMENT_ID,
            evaluator_digest: EVALUATOR_A,
            verdict,
            hard_violation: false,
            trajectory_cost: Some(cost),
            evaluator_cost: None,
            submitted_at: "2026-08-17T00:00:01Z",
            result_suffix: "quality",
        }
    }

    fn record(spec: RecordSpec<'_>) -> EvalEvidenceRecord {
        let mut metrics = BTreeMap::new();
        if let Some(value) = spec.trajectory_cost {
            metrics.insert(
                "trajectory.cost.usd_micros".into(),
                MetricValue::new(value, MetricUnit::MicroUsd),
            );
            metrics.insert(
                "trajectory.history_complete".into(),
                MetricValue::new(1, MetricUnit::Boolean),
            );
        }
        if let Some(value) = spec.evaluator_cost {
            metrics.insert(
                "cost.usd_micros".into(),
                MetricValue::new(value, MetricUnit::MicroUsd),
            );
        }
        let eval_id = format!("eval-{}", spec.subject_id);
        EvalEvidenceRecord {
            result_id: format!("result-{}-{}", spec.subject_id, spec.result_suffix),
            content_digest: EVIDENCE_DIGEST.into(),
            subject: EvalSubject {
                schema_version: EVAL_SCHEMA_VERSION,
                eval_id: eval_id.clone(),
                scope: spec.scope,
                subject_id: spec.subject_id.into(),
                policy_digest: spec.policy_digest.into(),
                preset: Some("auto".into()),
                cohort: Some("untrusted-evaluator-label".into()),
                holdout: false,
                decisions: vec![decision(
                    "decision-1",
                    spec.arm,
                    spec.policy_digest,
                    spec.experiment_id,
                    spec.assignment_id,
                )],
                requested_dimensions: metrics.keys().cloned().collect(),
                evidence: Vec::new(),
                evidence_digest: EVIDENCE_DIGEST.into(),
                observed_at: "2026-08-17T00:00:00Z".into(),
            },
            result: EvaluationResult {
                schema_version: EVAL_SCHEMA_VERSION,
                eval_id,
                evidence_digest: EVIDENCE_DIGEST.into(),
                evaluator: EvaluatorIdentity {
                    authority_id: "authority".into(),
                    evaluator_id: "evaluator".into(),
                    kind: EvaluatorKind::TaskNative,
                    version: "1".into(),
                    config_digest: spec.evaluator_digest.into(),
                },
                verdict: spec.verdict,
                metrics,
                hard_violations: if spec.hard_violation {
                    vec!["quality.security".into()]
                } else {
                    Vec::new()
                },
                confidence_ppm: None,
                evidence_refs: Vec::new(),
                decision_credit: BTreeMap::new(),
                idempotency_key: format!("idempotency-{}-{}", spec.subject_id, spec.result_suffix),
                submitted_at: spec.submitted_at.into(),
            },
        }
    }

    fn decision(
        id: &str,
        arm: ExperimentArm,
        policy_digest: &str,
        experiment_id: &str,
        assignment_id: &str,
    ) -> EvalDecisionRef {
        EvalDecisionRef {
            decision_id: id.into(),
            policy: "auto".into(),
            request_key: "agent_trace/v2|verify|normal".into(),
            selected_tier: match arm {
                ExperimentArm::Control => "strong",
                ExperimentArm::Challenger => "economy",
            }
            .into(),
            selected_effort: None,
            baseline_tier: None,
            baseline_effort: None,
            policy_digest: policy_digest.into(),
            experiment: Some(EvalExperimentRef {
                experiment_id: experiment_id.into(),
                arm,
                assignment_unit: ExperimentAssignmentUnit::Task,
                assignment_id_digest: assignment_id.into(),
                challenger_propensity_ppm: 500_000,
            }),
        }
    }

    fn snapshot(records: Vec<EvalEvidenceRecord>) -> EvalEvidenceSnapshot {
        EvalEvidenceSnapshot {
            evidence_root: EVIDENCE_DIGEST.into(),
            frozen_at: "2026-08-17T00:00:02Z".into(),
            records,
        }
    }

    #[test]
    fn admits_only_exact_task_units_and_excludes_ambiguous_or_conflicting_evidence() -> Result<()> {
        let exact = record(spec(
            "challenger-exact",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            700,
        ));
        let duplicate = exact.clone();
        let mut request = record(spec(
            "request-pass",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            1,
        ));
        request.subject.scope = EvalScope::Request;
        let mut wrong_policy = spec(
            "wrong-policy",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            1,
        );
        wrong_policy.policy_digest = OTHER_POLICY_DIGEST;
        let mut wrong_experiment = spec(
            "wrong-experiment",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            1,
        );
        wrong_experiment.experiment_id = OTHER_EXPERIMENT_ID;
        let mut mixed = record(spec(
            "mixed-arm",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            1,
        ));
        mixed.subject.decisions.push(decision(
            "decision-2",
            ExperimentArm::Control,
            POLICY_DIGEST,
            EXPERIMENT_ID,
            "mixed-arm",
        ));
        let conflicting_pass = record(spec(
            "conflicting-result",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            1,
        ));
        let mut conflicting_fail_spec = spec(
            "conflicting-result",
            ExperimentArm::Challenger,
            EvalVerdict::Fail,
            1,
        );
        conflicting_fail_spec.result_suffix = "conflict";
        let conflicting_fail = record(conflicting_fail_spec);
        let control = record(spec(
            "control-exact",
            ExperimentArm::Control,
            EvalVerdict::Pass,
            1_000,
        ));

        let assessment = assess_cohort(
            &snapshot(vec![
                exact,
                duplicate,
                request,
                record(wrong_policy),
                record(wrong_experiment),
                mixed,
                conflicting_pass,
                conflicting_fail,
                control,
            ]),
            POLICY_DIGEST,
            &exploration(Some(EVALUATOR_A)),
        )?;

        assert_eq!(assessment.challenger.eligible, 1);
        assert_eq!(assessment.challenger.conclusive, 1);
        assert_eq!(assessment.challenger.pass, 1);
        assert_eq!(assessment.control.eligible, 1);
        assert_eq!(assessment.excluded_requests, 1);
        assert_eq!(assessment.challenger.excluded, 1);
        Ok(())
    }

    #[test]
    fn one_subject_cannot_multiply_one_arm_with_different_assignment_digests() -> Result<()> {
        let mut malformed = record(spec(
            "same-arm-multiple-digests",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            700,
        ));
        malformed.subject.decisions.push(decision(
            "decision-2",
            ExperimentArm::Challenger,
            POLICY_DIGEST,
            EXPERIMENT_ID,
            "different-assignment",
        ));

        let assessment = assess_cohort(
            &snapshot(vec![malformed]),
            POLICY_DIGEST,
            &exploration(Some(EVALUATOR_A)),
        )?;

        assert_eq!(assessment.control.observed, 0);
        assert_eq!(assessment.challenger.observed, 0);
        assert_eq!(assessment.control.excluded, 0);
        assert_eq!(assessment.challenger.excluded, 0);
        Ok(())
    }

    #[test]
    fn one_subject_cannot_straddle_arms_with_different_assignment_digests() -> Result<()> {
        let mut malformed = record(spec(
            "opposite-arm-multiple-digests",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            700,
        ));
        malformed.subject.decisions.push(decision(
            "decision-2",
            ExperimentArm::Control,
            POLICY_DIGEST,
            EXPERIMENT_ID,
            "different-assignment",
        ));

        let assessment = assess_cohort(
            &snapshot(vec![malformed]),
            POLICY_DIGEST,
            &exploration(Some(EVALUATOR_A)),
        )?;

        assert_eq!(assessment.control.observed, 0);
        assert_eq!(assessment.challenger.observed, 0);
        assert_eq!(assessment.control.excluded, 0);
        assert_eq!(assessment.challenger.excluded, 0);
        Ok(())
    }

    #[test]
    fn wrong_assignment_propensity_cannot_enter_either_arm() -> Result<()> {
        let mut malformed = record(spec(
            "wrong-propensity",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            700,
        ));
        malformed.subject.decisions[0]
            .experiment
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("test experiment reference is missing"))?
            .challenger_propensity_ppm = 499_999;

        let assessment = assess_cohort(
            &snapshot(vec![malformed]),
            POLICY_DIGEST,
            &exploration(Some(EVALUATOR_A)),
        )?;

        assert_eq!(assessment.control.observed, 0);
        assert_eq!(assessment.challenger.observed, 0);
        assert_eq!(assessment.control.excluded, 0);
        assert_eq!(assessment.challenger.excluded, 0);
        Ok(())
    }

    #[test]
    fn aggregates_pass_rate_and_prefers_complete_trajectory_cost() -> Result<()> {
        let mut records = Vec::new();
        for index in 0..3 {
            records.push(record(spec(
                &format!("control-{index}"),
                ExperimentArm::Control,
                EvalVerdict::Pass,
                1_000,
            )));
        }
        for index in 0..10 {
            let subject_id = format!("challenger-{index}");
            let mut challenger = spec(
                &subject_id,
                ExperimentArm::Challenger,
                if index == 9 {
                    EvalVerdict::Fail
                } else {
                    EvalVerdict::Pass
                },
                700,
            );
            challenger.evaluator_cost = Some(10);
            records.push(record(challenger));
        }

        let assessment = assess_cohort(
            &snapshot(records),
            POLICY_DIGEST,
            &exploration(Some(EVALUATOR_A)),
        )?;

        assert_eq!(assessment.challenger.pass, 9);
        assert_eq!(assessment.challenger.fail, 1);
        assert_eq!(assessment.challenger.pass_rate_ppm, Some(900_000));
        assert_eq!(assessment.control.mean_cost_micro_usd, Some(1_000));
        assert_eq!(assessment.challenger.mean_cost_micro_usd, Some(700));
        assert_eq!(assessment.cost_delta_micro_usd, Some(-300));
        assert_eq!(assessment.verdict, CohortGateVerdict::Pass);
        Ok(())
    }

    #[test]
    fn inferred_multiple_evaluator_configs_hold_the_gate() -> Result<()> {
        let mut first = spec(
            "challenger-a",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            700,
        );
        first.evaluator_digest = EVALUATOR_A;
        let mut second = spec(
            "challenger-b",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            700,
        );
        second.evaluator_digest = EVALUATOR_B;

        let assessment = assess_cohort(
            &snapshot(vec![record(first), record(second)]),
            POLICY_DIGEST,
            &exploration(None),
        )?;

        assert_eq!(assessment.verdict, CohortGateVerdict::AmbiguousEvaluator);
        assert_eq!(assessment.evaluator_config_digest, None);
        assert_eq!(assessment.challenger.observed, 2);
        assert_eq!(assessment.challenger.excluded, 2);
        Ok(())
    }

    #[test]
    fn hard_violation_precedes_inferred_evaluator_ambiguity() -> Result<()> {
        let mut hard = spec(
            "challenger-hard-a",
            ExperimentArm::Challenger,
            EvalVerdict::Fail,
            700,
        );
        hard.hard_violation = true;
        hard.evaluator_digest = EVALUATOR_A;
        let mut other = spec(
            "challenger-pass-b",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            700,
        );
        other.evaluator_digest = EVALUATOR_B;

        let assessment = assess_cohort(
            &snapshot(vec![record(hard), record(other)]),
            POLICY_DIGEST,
            &exploration(None),
        )?;

        assert_eq!(assessment.challenger.hard_violations, 1);
        assert_eq!(assessment.verdict, CohortGateVerdict::HardViolation);
        Ok(())
    }

    #[test]
    fn conflicting_subject_does_not_poison_evaluator_inference() -> Result<()> {
        let exact = spec(
            "challenger-exact",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            700,
        );
        let mut conflict_a = spec(
            "challenger-conflict",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            700,
        );
        conflict_a.result_suffix = "conflict-a";
        let mut conflict_b = conflict_a.clone();
        conflict_b.evaluator_digest = EVALUATOR_B;
        conflict_b.verdict = EvalVerdict::Fail;
        conflict_b.result_suffix = "conflict-b";

        let assessment = assess_cohort(
            &snapshot(vec![record(exact), record(conflict_a), record(conflict_b)]),
            POLICY_DIGEST,
            &exploration(None),
        )?;

        assert_eq!(
            assessment.evaluator_config_digest.as_deref(),
            Some(EVALUATOR_A)
        );
        assert_eq!(assessment.verdict, CohortGateVerdict::InsufficientEvidence);
        assert_eq!(assessment.challenger.eligible, 1);
        assert_eq!(assessment.challenger.excluded, 1);
        Ok(())
    }

    #[test]
    fn hard_violation_fails_immediately() -> Result<()> {
        let mut violation = spec(
            "challenger-hard-fail",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            700,
        );
        violation.hard_violation = true;

        let assessment = assess_cohort(
            &snapshot(vec![record(violation)]),
            POLICY_DIGEST,
            &exploration(Some(EVALUATOR_A)),
        )?;

        assert_eq!(assessment.challenger.hard_violations, 1);
        assert_eq!(assessment.verdict, CohortGateVerdict::HardViolation);
        Ok(())
    }

    #[test]
    fn explicit_evaluator_digest_filters_conclusive_quality_exactly() -> Result<()> {
        let exact = spec(
            "challenger-exact-config",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            700,
        );
        let mut other = spec(
            "challenger-other-config",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            600,
        );
        other.evaluator_digest = EVALUATOR_B;

        let assessment = assess_cohort(
            &snapshot(vec![record(exact), record(other)]),
            POLICY_DIGEST,
            &exploration(Some(EVALUATOR_A)),
        )?;

        assert_eq!(assessment.challenger.eligible, 1);
        assert_eq!(assessment.challenger.pass, 1);
        assert_eq!(assessment.challenger.excluded, 1);
        assert_eq!(
            assessment.evaluator_config_digest.as_deref(),
            Some(EVALUATOR_A)
        );
        Ok(())
    }

    #[test]
    fn assignment_digest_collision_keeps_distinct_subjects_independent() -> Result<()> {
        let mut first = spec(
            "collision-subject-a",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            700,
        );
        first.assignment_id = "shared-assignment";
        let mut second = spec(
            "collision-subject-b",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            650,
        );
        second.assignment_id = "shared-assignment";

        let assessment = assess_cohort(
            &snapshot(vec![record(first), record(second)]),
            POLICY_DIGEST,
            &exploration(Some(EVALUATOR_A)),
        )?;

        assert_eq!(assessment.challenger.observed, 2);
        assert_eq!(assessment.challenger.eligible, 2);
        assert_eq!(assessment.challenger.pass, 2);
        assert_eq!(assessment.challenger.excluded, 0);
        Ok(())
    }

    #[test]
    fn identical_subject_ids_in_different_scopes_remain_independent() -> Result<()> {
        let task = spec(
            "cross-scope-subject",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            700,
        );
        let mut episode = task.clone();
        episode.scope = EvalScope::Episode;
        episode.result_suffix = "episode";
        episode.trajectory_cost = Some(600);

        let assessment = assess_cohort(
            &snapshot(vec![record(task), record(episode)]),
            POLICY_DIGEST,
            &exploration(Some(EVALUATOR_A)),
        )?;

        assert_eq!(assessment.challenger.observed, 2);
        assert_eq!(assessment.challenger.eligible, 2);
        assert_eq!(assessment.challenger.pass, 2);
        assert_eq!(assessment.challenger.mean_cost_micro_usd, Some(650));
        Ok(())
    }

    #[test]
    fn consistent_conclusive_duplicates_use_the_latest_complete_subject_record() -> Result<()> {
        let mut older = spec(
            "consistent-duplicate",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            700,
        );
        older.result_suffix = "older";
        let mut newer = older.clone();
        newer.result_suffix = "newer";
        newer.submitted_at = "2026-08-17T00:00:02Z";
        newer.trajectory_cost = Some(600);

        let assessment = assess_cohort(
            &snapshot(vec![record(older), record(newer)]),
            POLICY_DIGEST,
            &exploration(Some(EVALUATOR_A)),
        )?;

        assert_eq!(assessment.challenger.observed, 1);
        assert_eq!(assessment.challenger.eligible, 1);
        assert_eq!(assessment.challenger.pass, 1);
        assert_eq!(assessment.challenger.excluded, 0);
        assert_eq!(assessment.challenger.mean_cost_micro_usd, Some(600));
        Ok(())
    }

    #[test]
    fn latest_complete_cost_orders_rfc3339_instants_then_result_id() -> Result<()> {
        let mut stale = spec(
            "offset-crossing-cost",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            1_200,
        );
        stale.result_suffix = "stale";
        stale.submitted_at = "2026-08-17T02:00:00+02:00";
        let mut tied_earlier_id = stale.clone();
        tied_earlier_id.result_suffix = "a";
        tied_earlier_id.submitted_at = "2026-08-17T01:00:00Z";
        tied_earlier_id.trajectory_cost = Some(700);
        let mut tied_later_id = stale.clone();
        tied_later_id.result_suffix = "z";
        tied_later_id.submitted_at = "2026-08-17T02:00:00+01:00";
        tied_later_id.trajectory_cost = Some(600);

        let assessment = assess_cohort(
            &snapshot(vec![
                record(tied_later_id),
                record(stale),
                record(tied_earlier_id),
            ]),
            POLICY_DIGEST,
            &exploration(Some(EVALUATOR_A)),
        )?;

        assert_eq!(assessment.challenger.mean_cost_micro_usd, Some(600));
        Ok(())
    }

    #[test]
    fn malformed_result_timestamp_returns_a_checked_error() {
        let mut malformed = spec(
            "malformed-submitted-at",
            ExperimentArm::Challenger,
            EvalVerdict::Pass,
            700,
        );
        malformed.submitted_at = "not-rfc3339";

        let error = assess_cohort(
            &snapshot(vec![record(malformed)]),
            POLICY_DIGEST,
            &exploration(Some(EVALUATOR_A)),
        )
        .err();

        assert!(
            error.is_some_and(|error| error.to_string().contains("submitted_at must be RFC3339"))
        );
    }
}
