//! Deterministic compilation of observed evidence into policy lock artifacts.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::eval::compiler::{EvalEvidenceSnapshot, RouteEvalEvidence, TierEvalEvidence};
use crate::eval::types::EvaluatorKind;
use crate::policy_lock::{
    CertificateSource, CompilerIdentity, EconomicsSummary, LatencySummary, POLICY_COMPILER_ID,
    POLICY_COMPILER_VERSION, POLICY_LOCKFILE_VERSION, PolicyArtifact, PolicyCertificate,
    PolicyDefinition, PolicyLock, PromotionVerdict, QualitySummary, RouteOwner, semantic_digest,
    validate_document,
};
use crate::trajectory::guard::ProgressGuardPolicy;
use crate::workflow_state::predictive::PredictiveRouteProjection;
use bitrouter_sdk::routing::signals::NextStepRole;

const ACTIVE_ROUTE_MINIMUM_QUALITY_PPM: i64 = 900_000;

pub struct CompileInput<'a> {
    pub current: &'a PolicyLock,
    pub parent_digest: Option<&'a str>,
    pub snapshot_time_unix_ms: i64,
    pub eval: Option<&'a EvalEvidenceSnapshot>,
    /// Explicit operator/compiler input for guard proposals. `None` preserves
    /// the active lock exactly; admitted L1 evidence never mutates guards.
    pub proposed_progress_guards: Option<&'a BTreeMap<String, Option<ProgressGuardPolicy>>>,
}

/// Versioned, deterministic quality conditions used for positive route
/// recommendations. Publication remains a separate operator action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromotionQualityCriteria {
    pub minimum_candidate_pass_rate_ppm: i64,
    pub maximum_quality_loss_ppm: Option<i64>,
}

impl PromotionQualityCriteria {
    /// Conservative compatibility default: at least 90% observed pass rate
    /// and no observed regression from the baseline tier.
    pub fn quality_first() -> Self {
        Self {
            minimum_candidate_pass_rate_ppm: 900_000,
            maximum_quality_loss_ppm: Some(0),
        }
    }

    /// Produce a reviewable candidate from any conclusive positive evidence.
    /// The optimization layer must keep publication as an explicit action.
    pub fn manual_review() -> Self {
        Self {
            minimum_candidate_pass_rate_ppm: 1,
            maximum_quality_loss_ppm: None,
        }
    }

    pub fn custom(
        minimum_candidate_pass_rate_ppm: i64,
        maximum_quality_loss_ppm: i64,
    ) -> Result<Self> {
        let criteria = Self {
            minimum_candidate_pass_rate_ppm,
            maximum_quality_loss_ppm: Some(maximum_quality_loss_ppm),
        };
        criteria.validate()?;
        Ok(criteria)
    }

    pub fn validate(&self) -> Result<()> {
        if !(0..=1_000_000).contains(&self.minimum_candidate_pass_rate_ppm) {
            anyhow::bail!("minimum candidate pass rate must be between 0 and 1000000 ppm");
        }
        if self
            .maximum_quality_loss_ppm
            .is_some_and(|value| !(0..=1_000_000).contains(&value))
        {
            anyhow::bail!("maximum quality loss must be between 0 and 1000000 ppm");
        }
        Ok(())
    }

    fn admits(&self, candidate_pass_rate_ppm: i64, baseline_pass_rate_ppm: Option<i64>) -> bool {
        candidate_pass_rate_ppm >= self.minimum_candidate_pass_rate_ppm
            && self.maximum_quality_loss_ppm.is_none_or(|maximum_loss| {
                baseline_pass_rate_ppm.is_none_or(|baseline| {
                    candidate_pass_rate_ppm >= baseline.saturating_sub(maximum_loss)
                })
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompileChange {
    pub policy: String,
    pub request_key: String,
    pub previous_tier: Option<String>,
    pub selected_tier: String,
    pub verdict: PromotionVerdict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompileConflict {
    pub policy: String,
    pub request_key: String,
    pub operator_tier: String,
    pub recommended_tier: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct CompileResult {
    pub document: PolicyLock,
    pub changes: Vec<CompileChange>,
    pub conflicts: Vec<CompileConflict>,
}

#[derive(Serialize)]
struct CompilerConfigDigest {
    id: &'static str,
    version: u32,
    precedence: &'static str,
    active_route_minimum_quality_ppm: i64,
    promotion_quality: PromotionQualityCriteria,
    progress_guard_proposals_digest: String,
}

/// Compile a deterministic v4 candidate without mutating the active lock.
pub fn compile_candidate(input: CompileInput<'_>) -> Result<CompileResult> {
    compile_candidate_with_quality(input, &PromotionQualityCriteria::quality_first())
}

/// Compile with explicit promotion quality criteria. The criteria are included
/// in every certificate's compiler config digest.
pub fn compile_candidate_with_quality(
    input: CompileInput<'_>,
    quality: &PromotionQualityCriteria,
) -> Result<CompileResult> {
    validate_document(input.current)?;
    quality.validate()?;
    if let Some(eval) = input.eval {
        validate_eval_action_treatments(input.current, eval)?;
    }
    if input.snapshot_time_unix_ms < 0 {
        anyhow::bail!("snapshot time cannot be negative");
    }
    let evidence_root = input
        .eval
        .map(|eval| eval.evidence_root.clone())
        .unwrap_or(canonical_digest(&("policy-evidence-v4", "empty"))?);
    let eval_routes = match input.eval {
        Some(eval) => eval.route_evidence()?,
        None => BTreeMap::new(),
    };
    let compiler_config_digest = canonical_digest(&CompilerConfigDigest {
        id: POLICY_COMPILER_ID,
        version: POLICY_COMPILER_VERSION,
        precedence: "guardrail>operator>eval_negative>eval_positive>inherited",
        active_route_minimum_quality_ppm: ACTIVE_ROUTE_MINIMUM_QUALITY_PPM,
        promotion_quality: quality.clone(),
        progress_guard_proposals_digest: canonical_digest(&input.proposed_progress_guards)?,
    })?;
    let parent_digest = match input.parent_digest {
        Some(digest) => digest.to_string(),
        None => semantic_digest(input.current)?,
    };
    let mut document = PolicyLock {
        lockfile_version: POLICY_LOCKFILE_VERSION,
        artifact: Some(PolicyArtifact {
            parent_digest: Some(parent_digest),
            evidence_root: evidence_root.clone(),
            eval_snapshot_root: input.eval.map(|eval| eval.evidence_root.clone()),
            source_snapshot_time_unix_ms: source_snapshot_time(&input)?,
            compiler: CompilerIdentity {
                id: POLICY_COMPILER_ID.to_string(),
                version: POLICY_COMPILER_VERSION,
                config_digest: compiler_config_digest.clone(),
            },
        }),
        policies: input.current.policies.clone(),
        certificates: BTreeMap::new(),
    };
    if let Some(proposals) = input.proposed_progress_guards {
        for (policy_name, proposal) in proposals {
            let policy = document.policies.get_mut(policy_name).ok_or_else(|| {
                anyhow::anyhow!("progress guard proposal references missing policy '{policy_name}'")
            })?;
            policy.progress_guard = proposal.clone();
        }
    }
    let mut changes = Vec::new();
    let mut conflicts = Vec::new();

    for (policy_name, policy) in &input.current.policies {
        let policy_eval = eval_routes
            .iter()
            .filter_map(|((name, key), route)| {
                (name == policy_name && PredictiveRouteProjection::parse_key(key).is_some())
                    .then_some((key.clone(), route))
            })
            .collect::<BTreeMap<_, _>>();
        let mut keys = policy.routes.keys().cloned().collect::<BTreeSet<_>>();
        keys.extend(policy_eval.keys().cloned());
        let mut compiled_routes = BTreeMap::new();
        let mut certificates = BTreeMap::new();
        for request_key in keys {
            let prior_tier = policy.routes.get(&request_key);
            let prior = input.current.certificate(policy_name, &request_key);
            let route = policy_eval.get(&request_key).copied();
            let recommendation = route.and_then(|route| {
                eval_recommendation(
                    policy,
                    &request_key,
                    prior_tier.map(String::as_str),
                    route,
                    quality,
                )
            });
            if let Some(prior) = prior {
                if prior.owner == RouteOwner::Operator {
                    if let Some((recommended, _)) = recommendation
                        && recommended != prior.selected_tier
                    {
                        conflicts.push(CompileConflict {
                            policy: policy_name.clone(),
                            request_key: request_key.clone(),
                            operator_tier: prior.selected_tier.clone(),
                            recommended_tier: recommended,
                            reason: "admitted evidence conflicts with an operator-owned route"
                                .into(),
                        });
                    }
                    compiled_routes.insert(request_key.clone(), prior.selected_tier.clone());
                    certificates.insert(request_key, prior.clone());
                    continue;
                }
                if recommendation.is_none() {
                    compiled_routes.insert(request_key.clone(), prior.selected_tier.clone());
                    certificates.insert(request_key, prior.clone());
                    continue;
                }
            }
            let Some((selected_tier, verdict)) = recommendation else {
                continue;
            };
            let route = route
                .ok_or_else(|| anyhow::anyhow!("recommendation has no evaluation evidence"))?;
            if prior_tier != Some(&selected_tier) {
                changes.push(CompileChange {
                    policy: policy_name.clone(),
                    request_key: request_key.clone(),
                    previous_tier: prior_tier.cloned(),
                    selected_tier: selected_tier.clone(),
                    verdict,
                });
            }
            compiled_routes.insert(request_key.clone(), selected_tier.clone());
            let candidate = route.tiers.get(&selected_tier);
            let baseline_tier = route
                .baseline_tier
                .clone()
                .or_else(|| policy.default_tier.clone());
            let baseline = baseline_tier
                .as_ref()
                .and_then(|tier| route.tiers.get(tier));
            certificates.insert(
                request_key.clone(),
                PolicyCertificate {
                    classifier_digest: route.classifier_cohorts.first().cloned(),
                    owner: RouteOwner::Compiler,
                    selected_tier,
                    baseline_tier,
                    source: eval_certificate_source(route),
                    eligible_episodes: candidate.map_or(0, |tier| tier.eligible_episodes),
                    independent_tasks: candidate.map_or(0, |tier| {
                        u32::try_from(tier.independent_tasks.len()).unwrap_or(u32::MAX)
                    }),
                    quality: candidate.map(|tier| quality_summary(tier, baseline)),
                    economics: metric_delta_summary(candidate, baseline, |tier| {
                        tier.cost_micro_usd.mean()
                    })
                    .map(|normalized_cost_delta_ppm| EconomicsSummary {
                        normalized_cost_delta_ppm,
                    }),
                    latency: metric_delta_summary(candidate, baseline, |tier| {
                        tier.latency_ms.mean()
                    })
                    .map(|normalized_latency_delta_ppm| LatencySummary {
                        normalized_latency_delta_ppm,
                    }),
                    critical_violations: candidate.map_or(0, |tier| tier.critical_violations),
                    verdict,
                    evaluator_config_digest: Some(canonical_digest(
                        &route.evaluator_config_digests,
                    )?),
                    compiler_config_digest: compiler_config_digest.clone(),
                    evidence_digest: eval_route_evidence_digest(
                        policy_name,
                        &request_key,
                        Some(route),
                    )?,
                },
            );
        }
        if let Some(compiled) = document.policies.get_mut(policy_name) {
            compiled.routes = compiled_routes;
            if !compiled.routes.is_empty() {
                compiled.predictor =
                    Some(crate::workflow_state::predictive::compiled_predictor_contract());
            }
        }
        if !certificates.is_empty() {
            document
                .certificates
                .insert(policy_name.clone(), certificates);
        }
    }

    validate_document(&document)?;
    Ok(CompileResult {
        document,
        changes,
        conflicts,
    })
}

/// Refuse to relabel a measured model/effort/context action under a changed
/// tier catalog. Semantic evidence must carry its frozen candidate catalog.
pub(crate) fn validate_eval_action_treatments(
    current: &PolicyLock,
    eval: &EvalEvidenceSnapshot,
) -> Result<()> {
    for record in &eval.records {
        for decision in &record.subject.decisions {
            let Some(policy) = current.policies.get(&decision.policy) else {
                continue;
            };
            let assessed = crate::eval::types::classifier_cohorts(&record.subject)
                .iter()
                .any(|cohort| cohort != "unassessed");
            if assessed && decision.route_measurement.is_none() {
                anyhow::bail!(
                    "semantic eval result '{}' has no action measurement",
                    record.result_id
                );
            }
            if let Some(measurement) = &decision.route_measurement {
                crate::eval::types::validate_route_measurement(measurement)?;
                for tier in std::iter::once(decision.selected_tier.as_str())
                    .chain(decision.baseline_tier.as_deref())
                {
                    let expected = policy
                        .tiers
                        .get(tier)
                        .ok_or_else(|| anyhow::anyhow!("eval names unavailable tier '{tier}'"))?;
                    let measured = measurement
                        .candidates
                        .iter()
                        .find(|candidate| candidate.tier == tier)
                        .ok_or_else(|| {
                            anyhow::anyhow!("eval action measurement omits tier '{tier}'")
                        })?;
                    if measured.model != expected.model()
                        || measured.context != expected.context
                        || expected
                            .effort()
                            .is_some_and(|effort| measured.effort != Some(effort))
                    {
                        anyhow::bail!(
                            "eval result '{}' attributes tier '{tier}' to a different model/effort/context action",
                            record.result_id
                        );
                    }
                }
            }
            if let Some(expected) = policy
                .tiers
                .get(&decision.selected_tier)
                .and_then(bitrouter_sdk::config::PolicyModelTarget::effort)
                && decision.selected_effort != Some(expected)
            {
                anyhow::bail!(
                    "eval result '{}' attributes tier '{}:{}' to effort {:?}, expected '{}'",
                    record.result_id,
                    decision.policy,
                    decision.selected_tier,
                    decision.selected_effort,
                    expected
                );
            }
            if let Some(baseline_tier) = decision.baseline_tier.as_deref()
                && let Some(expected) = policy
                    .tiers
                    .get(baseline_tier)
                    .and_then(bitrouter_sdk::config::PolicyModelTarget::effort)
                && decision.baseline_effort != Some(expected)
            {
                anyhow::bail!(
                    "eval result '{}' attributes baseline tier '{}:{}' to effort {:?}, expected '{}'",
                    record.result_id,
                    decision.policy,
                    baseline_tier,
                    decision.baseline_effort,
                    expected
                );
            }
        }
    }
    Ok(())
}

fn source_snapshot_time(input: &CompileInput<'_>) -> Result<i64> {
    let eval_time = match input.eval {
        Some(eval) => chrono::DateTime::parse_from_rfc3339(&eval.frozen_at)
            .context("eval snapshot frozen_at must be RFC3339")?
            .timestamp_millis(),
        None => 0,
    };
    Ok(input.snapshot_time_unix_ms.max(eval_time))
}

fn eval_recommendation(
    policy: &PolicyDefinition,
    request_key: &str,
    prior_tier: Option<&str>,
    route: &RouteEvalEvidence,
    quality: &PromotionQualityCriteria,
) -> Option<(String, PromotionVerdict)> {
    if route.classifier_cohorts.len() > 1 || route.evaluator_config_digests.len() > 1 {
        return None;
    }
    let baseline = route
        .baseline_tier
        .as_deref()
        .or(policy.default_tier.as_deref());
    if let (Some(prior), Some(baseline)) = (prior_tier, baseline)
        && prior != baseline
        && let Some(active) = route.tiers.get(prior)
        && (active.critical_violations > 0
            || (active.fail_weight_ppm > 0
                && active.pass_rate_ppm() < ACTIVE_ROUTE_MINIMUM_QUALITY_PPM))
    {
        return Some((baseline.to_string(), PromotionVerdict::Demote));
    }
    if !positive_route_is_allowed(policy, request_key) {
        return None;
    }
    let baseline_pass_rate = baseline
        .and_then(|tier| route.tiers.get(tier))
        .map(TierEvalEvidence::pass_rate_ppm);
    let minimum_tasks = semantic_threshold(policy, request_key).max(1);
    route
        .tiers
        .iter()
        .filter(|(tier, evidence)| {
            Some(tier.as_str()) != baseline
                && policy.tiers.contains_key(tier.as_str())
                && u32::try_from(evidence.independent_tasks.len()).unwrap_or(u32::MAX)
                    >= minimum_tasks
                && evidence.critical_violations == 0
                && quality.admits(evidence.pass_rate_ppm(), baseline_pass_rate)
        })
        .max_by(|(left_tier, left), (right_tier, right)| {
            let cost_order = if left.independent_tasks == right.independent_tasks {
                match (left.cost_micro_usd.mean(), right.cost_micro_usd.mean()) {
                    (Some(left_cost), Some(right_cost)) => right_cost.cmp(&left_cost),
                    _ => std::cmp::Ordering::Equal,
                }
            } else {
                std::cmp::Ordering::Equal
            };
            cost_order
                .then_with(|| {
                    left.independent_tasks
                        .len()
                        .cmp(&right.independent_tasks.len())
                })
                .then_with(|| left.pass_rate_ppm().cmp(&right.pass_rate_ppm()))
                .then_with(|| right_tier.cmp(left_tier))
        })
        .map(|(tier, _)| (tier.clone(), PromotionVerdict::Promote))
}

fn eval_certificate_source(route: &RouteEvalEvidence) -> CertificateSource {
    if route.sources.len() != 1 {
        return CertificateSource::Mixed;
    }
    match route.sources.iter().next() {
        Some(EvaluatorKind::TaskNative) => CertificateSource::TaskNative,
        Some(EvaluatorKind::Human) => CertificateSource::Human,
        Some(EvaluatorKind::Enterprise) => CertificateSource::Enterprise,
        Some(EvaluatorKind::Agentic) => CertificateSource::Agentic,
        Some(EvaluatorKind::Generic) | None => CertificateSource::Mixed,
    }
}

fn quality_summary(
    candidate: &TierEvalEvidence,
    baseline: Option<&TierEvalEvidence>,
) -> QualitySummary {
    let candidate_pass_rate_ppm = candidate.pass_rate_ppm();
    let baseline_pass_rate_ppm = baseline
        .map(TierEvalEvidence::pass_rate_ppm)
        .unwrap_or_default();
    QualitySummary {
        baseline_pass_rate_ppm,
        candidate_pass_rate_ppm,
        delta_ppm: candidate_pass_rate_ppm.saturating_sub(baseline_pass_rate_ppm),
        lower_bound_ppm: candidate_pass_rate_ppm,
    }
}

fn metric_delta_summary(
    candidate: Option<&TierEvalEvidence>,
    baseline: Option<&TierEvalEvidence>,
    value: impl Fn(&TierEvalEvidence) -> Option<i64>,
) -> Option<i64> {
    let candidate = candidate.and_then(&value)?;
    let baseline = baseline.and_then(value)?;
    if baseline == 0 {
        return None;
    }
    Some(candidate.saturating_sub(baseline).saturating_mul(1_000_000) / baseline)
}

fn eval_route_evidence_digest(
    policy_name: &str,
    request_key: &str,
    evidence: Option<&RouteEvalEvidence>,
) -> Result<String> {
    canonical_digest(&(
        policy_name,
        request_key,
        evidence.map(|route| &route.matched_request_keys),
        evidence.map(|route| &route.evidence_records),
    ))
}

fn semantic_threshold(policy: &PolicyDefinition, request_key: &str) -> u32 {
    let opening = is_opening_like(request_key);
    if opening {
        policy
            .adequacy
            .min_semantic_successes_for_lock
            .max(policy.adequacy.min_semantic_successes_for_opening)
    } else {
        policy.adequacy.min_semantic_successes_for_lock
    }
}

fn positive_route_is_allowed(policy: &PolicyDefinition, request_key: &str) -> bool {
    PredictiveRouteProjection::parse_key(request_key)
        .is_some_and(|_| !is_opening_like(request_key) || policy.adequacy.explore_opening)
}

fn is_opening_like(request_key: &str) -> bool {
    PredictiveRouteProjection::parse_key(request_key)
        .is_some_and(|projection| projection.next_step_role == NextStepRole::Orchestrate)
}

fn canonical_digest<T: Serialize>(value: &T) -> Result<String> {
    let canonical = serde_json::to_vec(value).context("serializing canonical compiler input")?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(canonical))))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use bitrouter_sdk::config::{AdequacyConfig, PolicyModelTarget};
    use bitrouter_sdk::language_model::types::ReasoningEffort;

    use crate::eval::compiler::{EvalEvidenceRecord, EvalEvidenceSnapshot};
    use crate::eval::types::{
        EvalDecisionRef, EvalScope, EvalSubject, EvalVerdict, EvaluationResult, EvaluatorIdentity,
        EvaluatorKind, MetricUnit, MetricValue, evidence_digest,
    };
    use crate::policy_lock::{
        CertificateSource, PolicyDefinition, PolicyLock, PromotionVerdict, RouteOwner,
    };

    const EDIT_KEY: &str = "semantic_route/v1|unknown|implement|normal";

    fn policy(route: Option<&str>) -> PolicyDefinition {
        PolicyDefinition {
            tiers: BTreeMap::from([
                ("economy".into(), "vendor:economy".into()),
                ("strong".into(), "vendor:strong".into()),
            ]),
            routes: route
                .map(|tier| BTreeMap::from([(EDIT_KEY.to_string(), tier.to_string())]))
                .unwrap_or_default(),
            default_tier: Some("strong".into()),
            tool_use_tier: Some("strong".into()),
            tool_safe_tiers: vec!["strong".into()],
            adequacy: AdequacyConfig {
                escalation_tier: Some("strong".into()),
                explore_tier: Some("economy".into()),
                min_semantic_successes_for_lock: 1,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn lock(route: Option<&str>) -> PolicyLock {
        PolicyLock {
            policies: BTreeMap::from([("auto".into(), policy(route))]),
            ..Default::default()
        }
    }

    fn regressed_route_evidence() -> crate::eval::compiler::RouteEvalEvidence {
        use crate::eval::compiler::TierEvalEvidence;

        crate::eval::compiler::RouteEvalEvidence {
            baseline_tier: Some("strong".into()),
            tiers: BTreeMap::from([
                (
                    "strong".into(),
                    TierEvalEvidence {
                        eligible_episodes: 10,
                        independent_tasks: BTreeSet::from(["baseline-task".into()]),
                        total_weight_ppm: 10_000_000,
                        pass_weight_ppm: 10_000_000,
                        ..Default::default()
                    },
                ),
                (
                    "economy".into(),
                    TierEvalEvidence {
                        eligible_episodes: 10,
                        independent_tasks: BTreeSet::from(["candidate-task".into()]),
                        total_weight_ppm: 10_000_000,
                        pass_weight_ppm: 8_000_000,
                        fail_weight_ppm: 2_000_000,
                        ..Default::default()
                    },
                ),
            ]),
            ..Default::default()
        }
    }

    #[test]
    fn promotion_quality_criteria_make_the_tradeoff_explicit() -> anyhow::Result<()> {
        let route = regressed_route_evidence();
        let policy = policy(None);

        assert!(
            super::eval_recommendation(
                &policy,
                EDIT_KEY,
                None,
                &route,
                &super::PromotionQualityCriteria::quality_first(),
            )
            .is_none()
        );
        assert_eq!(
            super::eval_recommendation(
                &policy,
                EDIT_KEY,
                None,
                &route,
                &super::PromotionQualityCriteria::manual_review(),
            ),
            Some(("economy".into(), PromotionVerdict::Promote))
        );
        assert_eq!(
            super::eval_recommendation(
                &policy,
                EDIT_KEY,
                None,
                &route,
                &super::PromotionQualityCriteria::custom(750_000, 250_000)?,
            ),
            Some(("economy".into(), PromotionVerdict::Promote))
        );
        assert!(
            super::eval_recommendation(
                &policy,
                EDIT_KEY,
                None,
                &route,
                &super::PromotionQualityCriteria::custom(750_000, 100_000)?,
            )
            .is_none()
        );
        Ok(())
    }

    #[test]
    fn promotion_quality_criteria_reject_invalid_ppm_values() {
        assert!(super::PromotionQualityCriteria::custom(-1, 0).is_err());
        assert!(super::PromotionQualityCriteria::custom(0, 1_000_001).is_err());
    }

    #[test]
    fn structured_targets_require_exact_effort_attribution() -> anyhow::Result<()> {
        let mut current = lock(None);
        let policy = current
            .policies
            .get_mut("auto")
            .ok_or_else(|| anyhow::anyhow!("test fixture is missing policy auto"))?;
        policy.tiers.insert(
            "strong".into(),
            PolicyModelTarget {
                context: Default::default(),
                model: "openai:gpt-5.6".into(),
                effort: Some(ReasoningEffort::High),
            },
        );
        policy.tiers.insert(
            "economy".into(),
            PolicyModelTarget {
                context: Default::default(),
                model: "openai:gpt-5.6".into(),
                effort: Some(ReasoningEffort::Low),
            },
        );
        let mut eval = EvalEvidenceSnapshot {
            evidence_root: "evidence-root".into(),
            frozen_at: "2026-08-10T00:00:00Z".into(),
            records: vec![EvalEvidenceRecord {
                result_id: "result-effort".into(),
                content_digest: "content-effort".into(),
                subject: EvalSubject {
                    schema_version: 1,
                    eval_id: "eval-effort".into(),
                    scope: EvalScope::Task,
                    subject_id: "task-effort".into(),
                    policy_digest: "policy-digest".into(),
                    preset: Some("auto".into()),
                    cohort: None,
                    holdout: false,
                    decisions: vec![EvalDecisionRef {
                        decision_id: "decision-effort".into(),
                        policy: "auto".into(),
                        route_projection: EDIT_KEY.into(),
                        request_key: EDIT_KEY.into(),
                        selected_tier: "economy".into(),
                        selected_effort: None,
                        baseline_tier: Some("strong".into()),
                        baseline_effort: Some(ReasoningEffort::High),
                        policy_digest: "policy-digest".into(),
                        experiment: None,
                        route_measurement: None,
                    }],
                    requested_dimensions: BTreeSet::new(),
                    evidence: Vec::new(),
                    evidence_digest: "evidence-digest".into(),
                    observed_at: "2026-08-10T00:00:00Z".into(),
                },
                result: EvaluationResult {
                    schema_version: 1,
                    eval_id: "eval-effort".into(),
                    evidence_digest: "evidence-digest".into(),
                    evaluator: EvaluatorIdentity {
                        authority_id: "authority".into(),
                        evaluator_id: "evaluator".into(),
                        kind: EvaluatorKind::TaskNative,
                        version: "1".into(),
                        config_digest: "config-digest".into(),
                    },
                    verdict: EvalVerdict::Pass,
                    metrics: BTreeMap::new(),
                    hard_violations: Vec::new(),
                    confidence_ppm: Some(1_000_000),
                    evidence_refs: Vec::new(),
                    decision_credit: BTreeMap::new(),
                    idempotency_key: "idempotency-effort".into(),
                    submitted_at: "2026-08-10T00:00:01Z".into(),
                },
            }],
        };

        let error = super::validate_eval_action_treatments(&current, &eval)
            .err()
            .ok_or_else(|| anyhow::anyhow!("missing effort attribution must fail"))?;
        assert!(error.to_string().contains("expected 'low'"));

        eval.records[0].subject.decisions[0].selected_effort = Some(ReasoningEffort::Low);
        super::validate_eval_action_treatments(&current, &eval)?;
        let policy = current
            .policies
            .get("auto")
            .ok_or_else(|| anyhow::anyhow!("missing policy"))?;
        let candidates = policy
            .tiers
            .iter()
            .map(|(tier, target)| crate::eval::types::RouteActionCandidate {
                tier: tier.clone(),
                model: target.model.clone(),
                effort: target.effort,
                context: target.context,
                logging_probability_ppm: if tier == "economy" { 1_000_000 } else { 0 },
            })
            .collect();
        eval.records[0].subject.decisions[0].route_measurement =
            Some(crate::eval::types::RouteDecisionMeasurement::new(
                "economy",
                "openai:gpt-5.6",
                Some(ReasoningEffort::Low),
                candidates,
            )?);
        super::validate_eval_action_treatments(&current, &eval)?;
        current
            .policies
            .get_mut("auto")
            .and_then(|policy| policy.tiers.get_mut("economy"))
            .ok_or_else(|| anyhow::anyhow!("missing tier"))?
            .context = bitrouter_sdk::routing::ContextStrategy::Preserve;
        assert!(
            super::validate_eval_action_treatments(&current, &eval).is_err(),
            "evidence context cannot be reattributed to preserve"
        );
        Ok(())
    }

    #[test]
    fn admitted_negative_evidence_demotes_compiler_owned_route() -> anyhow::Result<()> {
        const TEMPLATE_ECONOMY_KEY: &str = "semantic_route/v1|unknown|verify|normal";
        let mut current = lock(None);
        let policy = current
            .policies
            .get_mut("auto")
            .ok_or_else(|| anyhow::anyhow!("policy missing"))?;
        policy
            .routes
            .insert(TEMPLATE_ECONOMY_KEY.into(), "economy".into());
        policy.predictor = Some(crate::workflow_state::predictive::compiled_predictor_contract());
        let digest = format!("sha256:{}", "a".repeat(64));
        current.certificates.insert(
            "auto".into(),
            BTreeMap::from([(
                TEMPLATE_ECONOMY_KEY.into(),
                crate::policy_lock::PolicyCertificate {
                    classifier_digest: None,
                    owner: crate::policy_lock::RouteOwner::Compiler,
                    selected_tier: "economy".into(),
                    baseline_tier: Some("strong".into()),
                    source: crate::policy_lock::CertificateSource::TaskNative,
                    eligible_episodes: 1,
                    independent_tasks: 1,
                    quality: None,
                    economics: None,
                    latency: None,
                    critical_violations: 0,
                    verdict: PromotionVerdict::Experiment,
                    evaluator_config_digest: None,
                    compiler_config_digest: digest.clone(),
                    evidence_digest: digest,
                },
            )]),
        );
        let evidence = Vec::new();
        let evidence_digest = evidence_digest(&evidence)?;
        let subject = EvalSubject {
            schema_version: 1,
            eval_id: "eval-pretrained-demotion".into(),
            scope: EvalScope::Task,
            subject_id: "task-pretrained-demotion".into(),
            policy_digest:
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            preset: Some("auto".into()),
            cohort: None,
            holdout: false,
            decisions: vec![EvalDecisionRef {
                decision_id: "decision-pretrained-demotion".into(),
                policy: "auto".into(),
                route_projection: TEMPLATE_ECONOMY_KEY.into(),
                request_key: TEMPLATE_ECONOMY_KEY.into(),
                selected_tier: "economy".into(),
                selected_effort: None,
                baseline_tier: Some("strong".into()),
                baseline_effort: None,
                policy_digest:
                    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                experiment: None,
                route_measurement: None,
            }],
            requested_dimensions: BTreeSet::from(["quality.pass".into()]),
            evidence,
            evidence_digest: evidence_digest.clone(),
            observed_at: "2026-07-30T00:00:00Z".into(),
        };
        let result = EvaluationResult {
            schema_version: 1,
            eval_id: subject.eval_id.clone(),
            evidence_digest,
            evaluator: EvaluatorIdentity {
                authority_id: "task-native".into(),
                evaluator_id: "suite".into(),
                kind: EvaluatorKind::TaskNative,
                version: "1".into(),
                config_digest:
                    "sha256:1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            },
            verdict: EvalVerdict::Fail,
            metrics: BTreeMap::new(),
            hard_violations: Vec::new(),
            confidence_ppm: Some(1_000_000),
            evidence_refs: Vec::new(),
            decision_credit: BTreeMap::new(),
            idempotency_key: "result-pretrained-demotion".into(),
            submitted_at: "2026-07-30T00:01:00Z".into(),
        };
        let eval = EvalEvidenceSnapshot {
            evidence_root:
                "sha256:2123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            frozen_at: "2026-07-30T00:02:00Z".into(),
            records: vec![EvalEvidenceRecord {
                result_id:
                    "sha256:3123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                content_digest:
                    "sha256:3123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                subject,
                result,
            }],
        };

        let compiled = super::compile_candidate(super::CompileInput {
            current: &current,
            parent_digest: None,
            snapshot_time_unix_ms: 1_785_369_600_000,
            eval: Some(&eval),
            proposed_progress_guards: None,
        })?;

        assert_eq!(
            compiled.document.policies["auto"].routes[TEMPLATE_ECONOMY_KEY],
            "strong"
        );
        assert!(compiled.conflicts.is_empty());
        let certificate = &compiled.document.certificates["auto"][TEMPLATE_ECONOMY_KEY];
        assert_eq!(certificate.owner, RouteOwner::Compiler);
        assert_eq!(certificate.source, CertificateSource::TaskNative);
        assert_eq!(certificate.verdict, PromotionVerdict::Demote);
        Ok(())
    }

    #[test]
    fn admitted_generic_eval_promotes_a_qualified_candidate() -> anyhow::Result<()> {
        let evidence = Vec::new();
        let evidence_digest = evidence_digest(&evidence)?;
        let subject = EvalSubject {
            schema_version: 1,
            eval_id: "eval-policy".into(),
            scope: EvalScope::Task,
            subject_id: "task-a".into(),
            policy_digest:
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            preset: Some("auto".into()),
            cohort: None,
            holdout: false,
            decisions: vec![EvalDecisionRef {
                decision_id: "decision-a".into(),
                policy: "auto".into(),
                route_projection: EDIT_KEY.into(),
                request_key: EDIT_KEY.into(),
                selected_tier: "economy".into(),
                selected_effort: None,
                baseline_tier: Some("strong".into()),
                baseline_effort: None,
                policy_digest:
                    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                experiment: None,
                route_measurement: None,
            }],
            requested_dimensions: BTreeSet::from(["quality.pass".into()]),
            evidence,
            evidence_digest: evidence_digest.clone(),
            observed_at: "2026-07-30T00:00:00Z".into(),
        };
        let result = EvaluationResult {
            schema_version: 1,
            eval_id: subject.eval_id.clone(),
            evidence_digest,
            evaluator: EvaluatorIdentity {
                authority_id: "task-native".into(),
                evaluator_id: "suite".into(),
                kind: EvaluatorKind::TaskNative,
                version: "1".into(),
                config_digest:
                    "sha256:1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            },
            verdict: EvalVerdict::Pass,
            metrics: BTreeMap::new(),
            hard_violations: Vec::new(),
            confidence_ppm: Some(1_000_000),
            evidence_refs: Vec::new(),
            decision_credit: BTreeMap::new(),
            idempotency_key: "result-policy".into(),
            submitted_at: "2026-07-30T00:01:00Z".into(),
        };
        let eval = EvalEvidenceSnapshot {
            evidence_root:
                "sha256:2123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            frozen_at: "2026-07-30T00:02:00Z".into(),
            records: vec![EvalEvidenceRecord {
                result_id:
                    "sha256:3123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                content_digest:
                    "sha256:3123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                subject,
                result,
            }],
        };

        let compiled = super::compile_candidate(super::CompileInput {
            current: &lock(None),
            parent_digest: None,
            snapshot_time_unix_ms: 1_785_369_600_000,
            eval: Some(&eval),
            proposed_progress_guards: None,
        })?;

        assert_eq!(
            compiled.document.policies["auto"].routes[EDIT_KEY],
            "economy"
        );
        let certificate = &compiled.document.certificates["auto"][EDIT_KEY];
        assert_eq!(certificate.source, CertificateSource::TaskNative);
        assert_eq!(certificate.verdict, PromotionVerdict::Promote);
        assert_eq!(
            certificate
                .quality
                .as_ref()
                .map(|q| q.candidate_pass_rate_ppm),
            Some(1_000_000)
        );
        let first_certificate = certificate.clone();
        let second = super::compile_candidate(super::CompileInput {
            current: &compiled.document,
            parent_digest: None,
            snapshot_time_unix_ms: 1_785_369_600_000,
            eval: None,
            proposed_progress_guards: None,
        })?;
        assert_eq!(
            second.document.certificates["auto"][EDIT_KEY], first_certificate,
            "a later compile with no new route evidence must preserve prior provenance exactly"
        );
        Ok(())
    }

    #[test]
    fn qualified_eval_candidates_prefer_lower_observed_cost() -> anyhow::Result<()> {
        let mut current = lock(None);
        current
            .policies
            .get_mut("auto")
            .ok_or_else(|| anyhow::anyhow!("test fixture is missing policy auto"))?
            .tiers
            .insert("balanced".into(), "vendor:balanced".into());
        let records = [("balanced", "task-shared", 600), ("economy", "task-shared", 300)]
            .into_iter()
            .map(|(tier, task, cost)| -> anyhow::Result<EvalEvidenceRecord> {
                let evidence = Vec::new();
                let evidence_digest = evidence_digest(&evidence)?;
                let subject = EvalSubject {
                    schema_version: 1,
                    eval_id: format!("eval-{tier}"),
                    scope: EvalScope::Task,
                    subject_id: task.into(),
                    policy_digest: "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                    preset: Some("auto".into()),
                    cohort: None,
                    holdout: false,
                    decisions: vec![EvalDecisionRef {
                        decision_id: format!("decision-{tier}"),
                        policy: "auto".into(),
                        route_projection: EDIT_KEY.into(),
                        request_key: EDIT_KEY.into(),
                        selected_tier: tier.into(),
                        selected_effort: None,
                        baseline_tier: Some("strong".into()),
                        baseline_effort: None,
                        policy_digest: "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                        experiment: None,
                        route_measurement: None,
                    }],
                    requested_dimensions: BTreeSet::from([
                        "quality.pass".into(),
                        "cost.usd_micros".into(),
                    ]),
                    evidence,
                    evidence_digest: evidence_digest.clone(),
                    observed_at: "2026-07-30T00:00:00Z".into(),
                };
                let result = EvaluationResult {
                    schema_version: 1,
                    eval_id: subject.eval_id.clone(),
                    evidence_digest,
                    evaluator: EvaluatorIdentity {
                        authority_id: "task-native".into(),
                        evaluator_id: "suite".into(),
                        kind: EvaluatorKind::TaskNative,
                        version: "1".into(),
                        config_digest: "sha256:1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                    },
                    verdict: EvalVerdict::Pass,
                    metrics: BTreeMap::from([(
                        "cost.usd_micros".into(),
                        MetricValue::new(cost, MetricUnit::MicroUsd),
                    )]),
                    hard_violations: Vec::new(),
                    confidence_ppm: Some(1_000_000),
                    evidence_refs: Vec::new(),
                    decision_credit: BTreeMap::new(),
                    idempotency_key: format!("result-{tier}"),
                    submitted_at: "2026-07-30T00:01:00Z".into(),
                };
                Ok(EvalEvidenceRecord {
                    result_id: format!("result-{tier}"),
                    content_digest: format!("content-{tier}"),
                    subject,
                    result,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let eval = EvalEvidenceSnapshot {
            evidence_root:
                "sha256:2123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            frozen_at: "2026-07-30T00:02:00Z".into(),
            records,
        };

        let compiled = super::compile_candidate(super::CompileInput {
            current: &current,
            parent_digest: None,
            snapshot_time_unix_ms: 1_785_369_600_000,
            eval: Some(&eval),
            proposed_progress_guards: None,
        })?;

        assert_eq!(
            compiled.document.policies["auto"].routes[EDIT_KEY],
            "economy"
        );
        Ok(())
    }

    #[test]
    fn compiler_preserves_guards_unless_explicit_input_proposes_change() -> anyhow::Result<()> {
        use crate::trajectory::guard::{IncompleteHistoryAction, ProgressGuardPolicy};

        let guard = ProgressGuardPolicy {
            escalation_tier: "strong".into(),
            protected_tiers: BTreeSet::from(["strong".into()]),
            max_consecutive_unprotected: Some(3),
            max_same_projection_unprotected: Some(4),
            max_recovery_count: Some(1),
            max_episode_requests: Some(8),
            max_episode_elapsed_ms: None,
            max_episode_cost_micro_usd: Some(50_000),
            hold_for_requests: 2,
            incomplete_history: IncompleteHistoryAction::Observe,
        };
        let mut current = PolicyLock::default();
        let mut definition = policy(None);
        definition.progress_guard = Some(guard.clone());
        current.policies.insert("auto".into(), definition);

        let preserved = super::compile_candidate(super::CompileInput {
            current: &current,
            parent_digest: None,
            snapshot_time_unix_ms: 1_785_369_600_000,
            eval: None,
            proposed_progress_guards: None,
        })?;
        assert_eq!(
            preserved.document.policies["auto"].progress_guard,
            Some(guard.clone())
        );

        let mut proposed = guard.clone();
        proposed.max_episode_requests = Some(12);
        let proposals = BTreeMap::from([("auto".to_string(), Some(proposed.clone()))]);
        let changed = super::compile_candidate(super::CompileInput {
            current: &current,
            parent_digest: None,
            snapshot_time_unix_ms: 1_785_369_600_000,
            eval: None,
            proposed_progress_guards: Some(&proposals),
        })?;
        assert_eq!(
            changed.document.policies["auto"].progress_guard,
            Some(proposed)
        );
        Ok(())
    }
}
