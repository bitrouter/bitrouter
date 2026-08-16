use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::eval::compiler::EvalEvidenceSnapshot;
use crate::eval::types::EvalScope;
use crate::optimization::cohort::{CohortAssessment, CohortGateVerdict, assess_cohort};
use crate::optimization::exploration::{
    OptimizationGate, PolicyOptimizationState, RouteExploration, RouteRejection,
};
use crate::policy_lock::{
    CertificateSource, CompilerIdentity, EconomicsSummary, PolicyArtifact, PolicyCertificate,
    PolicyLock, PromotionVerdict, QualitySummary, RouteOwner, validate_document,
};

const HISTORY_OPTIMIZER_ID: &str = "bitrouter-history-optimizer";
const HISTORY_OPTIMIZER_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OptimizationOptions {
    pub candidate_tier: String,
    pub challenger_exposure_ppm: u32,
    pub minimum_tasks_per_arm: u32,
    pub maximum_challenger_tasks: u32,
    pub minimum_pass_rate_ppm: u32,
    pub evaluator_config_digest: Option<String>,
}

impl OptimizationOptions {
    fn gate(&self) -> OptimizationGate {
        OptimizationGate {
            minimum_tasks_per_arm: self.minimum_tasks_per_arm,
            maximum_challenger_tasks: self.maximum_challenger_tasks,
            minimum_pass_rate_ppm: self.minimum_pass_rate_ppm,
            evaluator_config_digest: self.evaluator_config_digest.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerAction {
    Explore,
    Promote,
    Retreat,
    Hold,
    Converged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoricalOpportunity {
    pub request_key: String,
    pub champion_tier: String,
    pub observed_cost_micro_usd: u64,
    pub independent_units: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptimizationEvidence {
    pub eval_snapshot_root: String,
    pub observed_subject_digest: String,
    pub target: Option<HistoricalOpportunity>,
    pub cohort: Option<CohortAssessment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptimizationStep {
    pub action: ControllerAction,
    pub successor: Option<PolicyLock>,
    pub evidence: OptimizationEvidence,
}

#[derive(Debug, Clone, Copy)]
pub struct OptimizationStepInput<'a> {
    pub eval: &'a EvalEvidenceSnapshot,
    pub active_policy: &'a PolicyLock,
    pub active_policy_digest: &'a str,
    pub policy_name: &'a str,
    pub options: &'a OptimizationOptions,
}

#[derive(Default)]
struct OpportunityAggregate {
    observed_cost_micro_usd: u64,
    request_count: u64,
    found_cost: bool,
    independent_units: BTreeSet<(EvalScope, String)>,
}

pub fn select_opportunity(
    input: OptimizationStepInput<'_>,
) -> Result<Option<HistoricalOpportunity>> {
    let policy = input
        .active_policy
        .policies
        .get(input.policy_name)
        .ok_or_else(|| anyhow::anyhow!("optimization policy '{}' is missing", input.policy_name))?;
    let mut aggregate = BTreeMap::<String, OpportunityAggregate>::new();
    for record in &input.eval.records {
        if record.subject.policy_digest != input.active_policy_digest {
            continue;
        }
        for decision in &record.subject.decisions {
            if decision.policy != input.policy_name
                || decision.policy_digest != input.active_policy_digest
            {
                continue;
            }
            let route = aggregate.entry(decision.request_key.clone()).or_default();
            match record.subject.scope {
                EvalScope::Request => {
                    route.request_count = route
                        .request_count
                        .checked_add(1)
                        .context("counting request opportunity observations")?;
                    for evidence in &record.subject.evidence {
                        if evidence.kind != "request.outcome" {
                            continue;
                        }
                        if let Some(value) = evidence.attributes.get("cost_micro_usd") {
                            let cost = value.parse::<u64>().with_context(|| {
                                format!("parsing request cost for '{}'", decision.request_key)
                            })?;
                            route.observed_cost_micro_usd = route
                                .observed_cost_micro_usd
                                .checked_add(cost)
                                .context("summing request opportunity cost")?;
                            route.found_cost = true;
                        }
                    }
                }
                EvalScope::Task | EvalScope::Episode => {
                    route
                        .independent_units
                        .insert((record.subject.scope, record.subject.subject_id.clone()));
                }
            }
        }
    }

    let gate = input.options.gate();
    let mut candidates = Vec::new();
    for (request_key, route) in aggregate {
        let Some(champion_tier) = policy
            .routes
            .get(&request_key)
            .or(policy.default_tier.as_ref())
        else {
            continue;
        };
        if champion_tier == &input.options.candidate_tier
            || request_key.ends_with("|guarded")
            || route.independent_units.is_empty()
            || input
                .active_policy
                .certificate(input.policy_name, &request_key)
                .is_some_and(|certificate| certificate.owner == RouteOwner::Operator)
        {
            continue;
        }
        let context = treatment_context_digest(
            input.active_policy_digest,
            input.policy_name,
            &request_key,
            champion_tier,
            &input.options.candidate_tier,
            input.options.challenger_exposure_ppm,
            &gate,
        )?;
        if policy.optimization.as_ref().is_some_and(|state| {
            state
                .rejections
                .iter()
                .any(|rejection| rejection.treatment_context_digest == context)
        }) {
            continue;
        }
        let independent_units = u32::try_from(route.independent_units.len()).unwrap_or(u32::MAX);
        candidates.push(HistoricalOpportunity {
            request_key,
            champion_tier: champion_tier.clone(),
            observed_cost_micro_usd: if route.found_cost {
                route.observed_cost_micro_usd
            } else {
                route.request_count
            },
            independent_units,
        });
    }
    candidates.sort_by(|left, right| {
        right
            .observed_cost_micro_usd
            .cmp(&left.observed_cost_micro_usd)
            .then_with(|| right.independent_units.cmp(&left.independent_units))
            .then_with(|| left.request_key.cmp(&right.request_key))
    });
    Ok(candidates.into_iter().next())
}

pub fn prepare_step(input: OptimizationStepInput<'_>) -> Result<OptimizationStep> {
    let observed_subject_digest = observed_subject_digest(input.eval)?;
    let active_exploration = input
        .active_policy
        .policies
        .get(input.policy_name)
        .and_then(|policy| policy.optimization.as_ref())
        .and_then(|state| state.active.as_ref());
    if let Some(exploration) = active_exploration {
        return prepare_active_step(input, exploration, observed_subject_digest);
    }
    let target = select_opportunity(input)?;
    let Some(target) = target else {
        return Ok(OptimizationStep {
            action: ControllerAction::Converged,
            successor: None,
            evidence: OptimizationEvidence {
                eval_snapshot_root: input.eval.evidence_root.clone(),
                observed_subject_digest,
                target: None,
                cohort: None,
            },
        });
    };
    let gate = input.options.gate();
    let context_digest = treatment_context_digest(
        input.active_policy_digest,
        input.policy_name,
        &target.request_key,
        &target.champion_tier,
        &input.options.candidate_tier,
        input.options.challenger_exposure_ppm,
        &gate,
    )?;
    let experiment_id = domain_digest("bitrouter.history-optimizer.experiment.v1", &context_digest);
    let compiler_config_digest = compiler_config_digest(input.options)?;
    let mut successor = input.active_policy.clone();
    successor.artifact = Some(PolicyArtifact {
        parent_digest: Some(input.active_policy_digest.into()),
        evidence_root: observed_subject_digest.clone(),
        eval_snapshot_root: Some(input.eval.evidence_root.clone()),
        source_snapshot_time_unix_ms: chrono::DateTime::parse_from_rfc3339(&input.eval.frozen_at)
            .context("eval snapshot frozen_at must be RFC3339")?
            .timestamp_millis(),
        migration: None,
        compiler: CompilerIdentity {
            id: HISTORY_OPTIMIZER_ID.into(),
            version: HISTORY_OPTIMIZER_VERSION,
            config_digest: compiler_config_digest.clone(),
        },
    });
    let policy = successor
        .policies
        .get_mut(input.policy_name)
        .ok_or_else(|| anyhow::anyhow!("optimization policy '{}' is missing", input.policy_name))?;
    let state = policy
        .optimization
        .get_or_insert_with(PolicyOptimizationState::default);
    state.active = Some(RouteExploration {
        experiment_id,
        target_request_key: target.request_key.clone(),
        champion_tier: target.champion_tier.clone(),
        challenger_tier: input.options.candidate_tier.clone(),
        challenger_exposure_ppm: input.options.challenger_exposure_ppm,
        gate,
    });
    let certificates = successor
        .certificates
        .entry(input.policy_name.into())
        .or_default();
    certificates.insert(
        target.request_key.clone(),
        PolicyCertificate {
            owner: RouteOwner::Compiler,
            selected_tier: target.champion_tier.clone(),
            baseline_tier: None,
            source: CertificateSource::Mixed,
            eligible_episodes: target.independent_units,
            independent_tasks: target.independent_units,
            quality: None,
            economics: None,
            latency: None,
            critical_violations: 0,
            verdict: PromotionVerdict::Experiment,
            evaluator_config_digest: input.options.evaluator_config_digest.clone(),
            compiler_config_digest,
            evidence_digest: observed_subject_digest.clone(),
            legacy: None,
        },
    );

    validate_document(&successor)?;
    Ok(OptimizationStep {
        action: ControllerAction::Explore,
        successor: Some(successor),
        evidence: OptimizationEvidence {
            eval_snapshot_root: input.eval.evidence_root.clone(),
            observed_subject_digest,
            target: Some(target),
            cohort: None,
        },
    })
}

fn prepare_active_step(
    input: OptimizationStepInput<'_>,
    exploration: &RouteExploration,
    observed_subject_digest: String,
) -> Result<OptimizationStep> {
    let assessment = assess_cohort(input.eval, input.active_policy_digest, exploration)?;
    let budget_reached =
        assessment.challenger.observed >= exploration.gate.maximum_challenger_tasks;
    let action = match assessment.verdict {
        CohortGateVerdict::HardViolation => ControllerAction::Retreat,
        CohortGateVerdict::AmbiguousEvaluator => ControllerAction::Hold,
        CohortGateVerdict::Pass
            if assessment
                .cost_delta_micro_usd
                .is_some_and(|delta| delta < 0) =>
        {
            ControllerAction::Promote
        }
        CohortGateVerdict::Pass
        | CohortGateVerdict::InsufficientEvidence
        | CohortGateVerdict::QualityFailed
            if budget_reached =>
        {
            ControllerAction::Retreat
        }
        CohortGateVerdict::Pass
        | CohortGateVerdict::InsufficientEvidence
        | CohortGateVerdict::QualityFailed => ControllerAction::Hold,
    };
    let target = HistoricalOpportunity {
        request_key: exploration.target_request_key.clone(),
        champion_tier: exploration.champion_tier.clone(),
        observed_cost_micro_usd: 0,
        independent_units: assessment.challenger.observed,
    };
    let successor = match action {
        ControllerAction::Promote | ControllerAction::Retreat => Some(active_successor(
            input,
            exploration,
            &assessment,
            &observed_subject_digest,
            action,
        )?),
        ControllerAction::Hold => None,
        ControllerAction::Explore | ControllerAction::Converged => {
            anyhow::bail!("invalid active optimization controller transition")
        }
    };
    Ok(OptimizationStep {
        action,
        successor,
        evidence: OptimizationEvidence {
            eval_snapshot_root: input.eval.evidence_root.clone(),
            observed_subject_digest,
            target: Some(target),
            cohort: Some(assessment),
        },
    })
}

fn active_successor(
    input: OptimizationStepInput<'_>,
    exploration: &RouteExploration,
    assessment: &CohortAssessment,
    observed_subject_digest: &str,
    action: ControllerAction,
) -> Result<PolicyLock> {
    let mut successor = input.active_policy.clone();
    let compiler_config_digest = active_compiler_config_digest(exploration)?;
    successor.artifact = Some(successor_artifact(
        input,
        observed_subject_digest,
        compiler_config_digest.clone(),
    )?);
    let policy = successor
        .policies
        .get_mut(input.policy_name)
        .ok_or_else(|| anyhow::anyhow!("optimization policy '{}' is missing", input.policy_name))?;
    let state = policy
        .optimization
        .get_or_insert_with(PolicyOptimizationState::default);
    state.active = None;
    let (selected_tier, verdict) = match action {
        ControllerAction::Promote => {
            policy.routes.insert(
                exploration.target_request_key.clone(),
                exploration.challenger_tier.clone(),
            );
            (
                exploration.challenger_tier.clone(),
                PromotionVerdict::Promote,
            )
        }
        ControllerAction::Retreat => {
            let treatment_context_digest = treatment_context_digest(
                input.active_policy_digest,
                input.policy_name,
                &exploration.target_request_key,
                &exploration.champion_tier,
                &exploration.challenger_tier,
                exploration.challenger_exposure_ppm,
                &exploration.gate,
            )?;
            state
                .rejections
                .retain(|rejection| rejection.treatment_context_digest != treatment_context_digest);
            if state.rejections.len() == 256 {
                state.rejections.remove(0);
            }
            state.rejections.push(RouteRejection {
                experiment_id: exploration.experiment_id.clone(),
                treatment_context_digest,
                evidence_root: input.eval.evidence_root.clone(),
                reason: rejection_reason(assessment).into(),
            });
            (exploration.champion_tier.clone(), PromotionVerdict::Blocked)
        }
        ControllerAction::Explore | ControllerAction::Hold | ControllerAction::Converged => {
            anyhow::bail!("controller action does not produce an active successor")
        }
    };
    successor
        .certificates
        .entry(input.policy_name.into())
        .or_default()
        .insert(
            exploration.target_request_key.clone(),
            cohort_certificate(
                assessment,
                selected_tier,
                exploration.champion_tier.clone(),
                verdict,
                compiler_config_digest,
                observed_subject_digest.into(),
            )?,
        );
    validate_document(&successor)?;
    Ok(successor)
}

fn successor_artifact(
    input: OptimizationStepInput<'_>,
    observed_subject_digest: &str,
    compiler_config_digest: String,
) -> Result<PolicyArtifact> {
    Ok(PolicyArtifact {
        parent_digest: Some(input.active_policy_digest.into()),
        evidence_root: observed_subject_digest.into(),
        eval_snapshot_root: Some(input.eval.evidence_root.clone()),
        source_snapshot_time_unix_ms: chrono::DateTime::parse_from_rfc3339(&input.eval.frozen_at)
            .context("eval snapshot frozen_at must be RFC3339")?
            .timestamp_millis(),
        migration: None,
        compiler: CompilerIdentity {
            id: HISTORY_OPTIMIZER_ID.into(),
            version: HISTORY_OPTIMIZER_VERSION,
            config_digest: compiler_config_digest,
        },
    })
}

fn cohort_certificate(
    assessment: &CohortAssessment,
    selected_tier: String,
    baseline_tier: String,
    verdict: PromotionVerdict,
    compiler_config_digest: String,
    evidence_digest: String,
) -> Result<PolicyCertificate> {
    let baseline_pass_rate_ppm = i64::from(assessment.control.pass_rate_ppm.unwrap_or_default());
    let candidate_pass_rate_ppm =
        i64::from(assessment.challenger.pass_rate_ppm.unwrap_or_default());
    let normalized_cost_delta_ppm = normalized_cost_delta(assessment)?;
    Ok(PolicyCertificate {
        owner: RouteOwner::Compiler,
        selected_tier,
        baseline_tier: Some(baseline_tier),
        source: CertificateSource::Mixed,
        eligible_episodes: assessment.challenger.eligible,
        independent_tasks: assessment.challenger.eligible,
        quality: Some(QualitySummary {
            baseline_pass_rate_ppm,
            candidate_pass_rate_ppm,
            delta_ppm: candidate_pass_rate_ppm
                .checked_sub(baseline_pass_rate_ppm)
                .context("computing certificate quality delta")?,
            lower_bound_ppm: candidate_pass_rate_ppm,
        }),
        economics: normalized_cost_delta_ppm.map(|value| EconomicsSummary {
            normalized_cost_delta_ppm: value,
        }),
        latency: None,
        critical_violations: assessment.challenger.hard_violations,
        verdict,
        evaluator_config_digest: assessment.evaluator_config_digest.clone(),
        compiler_config_digest,
        evidence_digest,
        legacy: None,
    })
}

fn normalized_cost_delta(assessment: &CohortAssessment) -> Result<Option<i64>> {
    let (Some(control), Some(delta)) = (
        assessment.control.mean_cost_micro_usd,
        assessment.cost_delta_micro_usd,
    ) else {
        return Ok(None);
    };
    if control == 0 {
        return Ok(None);
    }
    let scaled = i128::from(delta)
        .checked_mul(1_000_000)
        .context("scaling certificate cost delta")?
        .checked_div(i128::from(control))
        .context("computing certificate cost delta")?;
    Ok(i64::try_from(scaled).ok())
}

fn rejection_reason(assessment: &CohortAssessment) -> &'static str {
    if assessment.challenger.hard_violations > 0 {
        "challenger hard violation"
    } else if assessment.verdict == CohortGateVerdict::QualityFailed {
        "challenger quality gate failed at sample budget"
    } else if assessment
        .cost_delta_micro_usd
        .is_some_and(|delta| delta >= 0)
    {
        "challenger did not lower complete unit cost at sample budget"
    } else {
        "challenger evidence remained insufficient at sample budget"
    }
}

fn active_compiler_config_digest(exploration: &RouteExploration) -> Result<String> {
    #[derive(Serialize)]
    struct ActiveCompilerConfig<'a> {
        id: &'a str,
        version: u32,
        exploration: &'a RouteExploration,
    }
    canonical_digest(&ActiveCompilerConfig {
        id: HISTORY_OPTIMIZER_ID,
        version: HISTORY_OPTIMIZER_VERSION,
        exploration,
    })
}

#[derive(Serialize)]
struct TreatmentContext<'a> {
    parent_policy_digest: &'a str,
    policy_name: &'a str,
    request_key: &'a str,
    champion_tier: &'a str,
    challenger_tier: &'a str,
    challenger_exposure_ppm: u32,
    gate: &'a OptimizationGate,
}

pub fn treatment_context_digest(
    parent_policy_digest: &str,
    policy_name: &str,
    request_key: &str,
    champion_tier: &str,
    challenger_tier: &str,
    challenger_exposure_ppm: u32,
    gate: &OptimizationGate,
) -> Result<String> {
    canonical_digest(&TreatmentContext {
        parent_policy_digest,
        policy_name,
        request_key,
        champion_tier,
        challenger_tier,
        challenger_exposure_ppm,
        gate,
    })
}

fn compiler_config_digest(options: &OptimizationOptions) -> Result<String> {
    #[derive(Serialize)]
    struct CompilerConfig<'a> {
        id: &'a str,
        version: u32,
        options: &'a OptimizationOptions,
    }
    canonical_digest(&CompilerConfig {
        id: HISTORY_OPTIMIZER_ID,
        version: HISTORY_OPTIMIZER_VERSION,
        options,
    })
}

fn observed_subject_digest(eval: &EvalEvidenceSnapshot) -> Result<String> {
    let mut records = eval
        .records
        .iter()
        .map(|record| {
            (
                record.result_id.as_str(),
                record.content_digest.as_str(),
                &record.subject,
            )
        })
        .collect::<Vec<_>>();
    records.sort_by(|left, right| left.0.cmp(right.0));
    canonical_digest(&records)
}

fn canonical_digest<T: Serialize>(value: &T) -> Result<String> {
    let canonical = serde_json::to_vec(value).context("serializing optimizer digest input")?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(canonical))))
}

fn domain_digest(domain: &str, value: &str) -> String {
    let bytes = format!("{domain}\0{value}");
    format!("sha256:{}", hex::encode(Sha256::digest(bytes.as_bytes())))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use anyhow::Result;
    use bitrouter_sdk::config::PolicyModelTarget;

    use crate::eval::compiler::{EvalEvidenceRecord, EvalEvidenceSnapshot};
    use crate::eval::types::{
        EVAL_SCHEMA_VERSION, EvalDecisionRef, EvalExperimentRef, EvalScope, EvalSubject,
        EvalVerdict, EvaluationResult, EvaluatorIdentity, EvaluatorKind, EvidenceItem,
        ExperimentArm, ExperimentAssignmentUnit, MetricUnit, MetricValue,
    };
    use crate::optimization::exploration::{
        OptimizationGate, PolicyOptimizationState, RouteRejection,
    };
    use crate::policy_lock::{
        CertificateSource, PolicyCertificate, PolicyDefinition, PolicyLock, PromotionVerdict,
        RouteOwner,
    };

    use super::{
        ControllerAction, OptimizationOptions, OptimizationStepInput, prepare_step,
        select_opportunity, treatment_context_digest,
    };

    const ACTIVE_DIGEST: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SNAPSHOT_ROOT: &str =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const CONFIG_DIGEST: &str =
        "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const EXPERIMENT_ID: &str =
        "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    fn options() -> OptimizationOptions {
        OptimizationOptions {
            candidate_tier: "economy".into(),
            challenger_exposure_ppm: 100_000,
            minimum_tasks_per_arm: 3,
            maximum_challenger_tasks: 20,
            minimum_pass_rate_ppm: 900_000,
            evaluator_config_digest: None,
        }
    }

    fn lock() -> PolicyLock {
        let request_keys = [
            "agent_trace/v2|verify|normal",
            "agent_trace/v2|edit|normal",
            "agent_trace/v2|review|normal",
            "agent_trace/v2|opening|normal",
            "agent_trace/v2|planning|normal",
            "agent_trace/v2|test|guarded",
            "agent_trace/v2|debug|normal",
            "agent_trace/v2|finalization|normal",
        ];
        let mut policy = PolicyDefinition::default();
        policy.tiers.insert(
            "strong".into(),
            PolicyModelTarget::Model("strong-model".into()),
        );
        policy.tiers.insert(
            "economy".into(),
            PolicyModelTarget::Model("economy-model".into()),
        );
        for request_key in request_keys {
            policy.routes.insert(request_key.into(), "strong".into());
        }
        policy
            .routes
            .insert("agent_trace/v2|planning|normal".into(), "economy".into());

        let certificates = request_keys
            .into_iter()
            .map(|request_key| {
                let selected_tier = policy
                    .routes
                    .get(request_key)
                    .cloned()
                    .unwrap_or_else(|| "strong".into());
                (
                    request_key.into(),
                    PolicyCertificate {
                        owner: if request_key == "agent_trace/v2|opening|normal" {
                            RouteOwner::Operator
                        } else {
                            RouteOwner::Compiler
                        },
                        selected_tier,
                        baseline_tier: None,
                        source: if request_key == "agent_trace/v2|opening|normal" {
                            CertificateSource::Operator
                        } else {
                            CertificateSource::TaskNative
                        },
                        eligible_episodes: 0,
                        independent_tasks: 0,
                        quality: None,
                        economics: None,
                        latency: None,
                        critical_violations: 0,
                        verdict: PromotionVerdict::Retain,
                        evaluator_config_digest: None,
                        compiler_config_digest: CONFIG_DIGEST.into(),
                        evidence_digest: SNAPSHOT_ROOT.into(),
                        legacy: None,
                    },
                )
            })
            .collect();

        PolicyLock {
            policies: BTreeMap::from([("auto".into(), policy)]),
            certificates: BTreeMap::from([("auto".into(), certificates)]),
            ..PolicyLock::default()
        }
    }

    fn request_record(
        request_key: &str,
        request_id: &str,
        cost: Option<u64>,
    ) -> EvalEvidenceRecord {
        let mut attributes = BTreeMap::new();
        if let Some(cost) = cost {
            attributes.insert("cost_micro_usd".into(), cost.to_string());
        }
        let evidence = EvidenceItem {
            evidence_id: format!("request-{request_id}"),
            kind: "request.outcome".into(),
            digest: SNAPSHOT_ROOT.into(),
            redacted: true,
            attributes,
        };
        record(
            EvalScope::Request,
            request_id,
            request_key,
            EvalVerdict::Inconclusive,
            vec![evidence],
        )
    }

    fn unit_record(request_key: &str, unit_id: &str) -> EvalEvidenceRecord {
        record(
            EvalScope::Task,
            unit_id,
            request_key,
            EvalVerdict::Pass,
            Vec::new(),
        )
    }

    fn record(
        scope: EvalScope,
        subject_id: &str,
        request_key: &str,
        verdict: EvalVerdict,
        evidence: Vec<EvidenceItem>,
    ) -> EvalEvidenceRecord {
        let eval_id = format!("eval-{subject_id}-{request_key}");
        EvalEvidenceRecord {
            result_id: format!("result-{eval_id}"),
            content_digest: SNAPSHOT_ROOT.into(),
            subject: EvalSubject {
                schema_version: EVAL_SCHEMA_VERSION,
                eval_id: eval_id.clone(),
                scope,
                subject_id: subject_id.into(),
                policy_digest: ACTIVE_DIGEST.into(),
                preset: Some("auto".into()),
                cohort: None,
                holdout: false,
                decisions: vec![EvalDecisionRef {
                    decision_id: format!("decision-{subject_id}"),
                    policy: "auto".into(),
                    request_key: request_key.into(),
                    selected_tier: "strong".into(),
                    selected_effort: None,
                    baseline_tier: None,
                    baseline_effort: None,
                    policy_digest: ACTIVE_DIGEST.into(),
                    experiment: None,
                }],
                requested_dimensions: BTreeSet::new(),
                evidence,
                evidence_digest: SNAPSHOT_ROOT.into(),
                observed_at: "2026-08-17T00:00:00Z".into(),
            },
            result: EvaluationResult {
                schema_version: EVAL_SCHEMA_VERSION,
                eval_id,
                evidence_digest: SNAPSHOT_ROOT.into(),
                evaluator: EvaluatorIdentity {
                    authority_id: "test".into(),
                    evaluator_id: "history".into(),
                    kind: EvaluatorKind::TaskNative,
                    version: "1".into(),
                    config_digest: CONFIG_DIGEST.into(),
                },
                verdict,
                metrics: BTreeMap::new(),
                hard_violations: Vec::new(),
                confidence_ppm: None,
                evidence_refs: Vec::new(),
                decision_credit: BTreeMap::new(),
                idempotency_key: format!("idempotency-{subject_id}"),
                submitted_at: "2026-08-17T00:00:01Z".into(),
            },
        }
    }

    fn snapshot() -> EvalEvidenceSnapshot {
        let mut records = Vec::new();
        let ranked = [
            ("agent_trace/v2|verify|normal", 900, 4),
            ("agent_trace/v2|edit|normal", 500, 5),
            ("agent_trace/v2|review|normal", 900, 3),
            ("agent_trace/v2|opening|normal", 2_000, 5),
            ("agent_trace/v2|planning|normal", 1_900, 5),
            ("agent_trace/v2|test|guarded", 1_800, 5),
            ("agent_trace/v2|debug|normal", 1_700, 0),
            ("agent_trace/v2|finalization|normal", 1_600, 5),
        ];
        for (request_key, cost, units) in ranked {
            records.push(request_record(
                request_key,
                &format!("request-{cost}"),
                Some(cost),
            ));
            for index in 0..units {
                records.push(unit_record(request_key, &format!("unit-{cost}-{index}")));
            }
        }
        EvalEvidenceSnapshot {
            evidence_root: SNAPSHOT_ROOT.into(),
            frozen_at: "2026-08-17T00:00:02Z".into(),
            records,
        }
    }

    fn active_lock() -> PolicyLock {
        let mut lock = lock();
        lock.policies.get_mut("auto").and_then(|policy| {
            policy.optimization = Some(PolicyOptimizationState {
                active: Some(crate::optimization::exploration::RouteExploration {
                    experiment_id: EXPERIMENT_ID.into(),
                    target_request_key: "agent_trace/v2|verify|normal".into(),
                    champion_tier: "strong".into(),
                    challenger_tier: "economy".into(),
                    challenger_exposure_ppm: 100_000,
                    gate: OptimizationGate {
                        minimum_tasks_per_arm: 3,
                        maximum_challenger_tasks: 20,
                        minimum_pass_rate_ppm: 900_000,
                        evaluator_config_digest: Some(CONFIG_DIGEST.into()),
                    },
                }),
                rejections: Vec::new(),
            });
            policy.optimization.as_ref()
        });
        lock
    }

    fn experiment_record(
        subject_id: &str,
        arm: ExperimentArm,
        verdict: EvalVerdict,
        cost: i64,
        hard_violation: bool,
    ) -> EvalEvidenceRecord {
        let mut record = unit_record("agent_trace/v2|verify|normal", subject_id);
        record.subject.decisions[0].selected_tier = match arm {
            ExperimentArm::Control => "strong",
            ExperimentArm::Challenger => "economy",
        }
        .into();
        record.subject.decisions[0].experiment = Some(EvalExperimentRef {
            experiment_id: EXPERIMENT_ID.into(),
            arm,
            assignment_unit: ExperimentAssignmentUnit::Task,
            assignment_id_digest: format!("assignment-{subject_id}"),
            challenger_propensity_ppm: 100_000,
        });
        record.result.verdict = verdict;
        record.result.metrics = BTreeMap::from([
            (
                "trajectory.cost.usd_micros".into(),
                MetricValue::new(cost, MetricUnit::MicroUsd),
            ),
            (
                "trajectory.history_complete".into(),
                MetricValue::new(1, MetricUnit::Boolean),
            ),
        ]);
        record.subject.requested_dimensions = record.result.metrics.keys().cloned().collect();
        if hard_violation {
            record.result.hard_violations = vec!["quality.security".into()];
        }
        record
    }

    fn active_snapshot(
        control: usize,
        challenger: usize,
        challenger_verdict: EvalVerdict,
        control_cost: i64,
        challenger_cost: i64,
    ) -> EvalEvidenceSnapshot {
        let mut records = Vec::new();
        for index in 0..control {
            records.push(experiment_record(
                &format!("control-{index}"),
                ExperimentArm::Control,
                EvalVerdict::Pass,
                control_cost,
                false,
            ));
        }
        for index in 0..challenger {
            records.push(experiment_record(
                &format!("challenger-{index}"),
                ExperimentArm::Challenger,
                challenger_verdict,
                challenger_cost,
                false,
            ));
        }
        EvalEvidenceSnapshot {
            evidence_root: SNAPSHOT_ROOT.into(),
            frozen_at: "2026-08-17T00:00:02Z".into(),
            records,
        }
    }

    #[test]
    fn cold_start_ranks_cost_then_coverage_then_key_and_excludes_unsafe_routes() -> Result<()> {
        let mut lock = lock();
        let gate = OptimizationGate {
            minimum_tasks_per_arm: 3,
            maximum_challenger_tasks: 20,
            minimum_pass_rate_ppm: 900_000,
            evaluator_config_digest: None,
        };
        let rejected_context = treatment_context_digest(
            ACTIVE_DIGEST,
            "auto",
            "agent_trace/v2|finalization|normal",
            "strong",
            "economy",
            100_000,
            &gate,
        )?;
        lock.policies
            .get_mut("auto")
            .ok_or_else(|| anyhow::anyhow!("missing test policy"))?
            .optimization = Some(PolicyOptimizationState {
            active: None,
            rejections: vec![RouteRejection {
                experiment_id: SNAPSHOT_ROOT.into(),
                treatment_context_digest: rejected_context,
                evidence_root: SNAPSHOT_ROOT.into(),
                reason: "higher cost".into(),
            }],
        });
        let snapshot = snapshot();
        let options = options();
        let selected = select_opportunity(OptimizationStepInput {
            eval: &snapshot,
            active_policy: &lock,
            active_policy_digest: ACTIVE_DIGEST,
            policy_name: "auto",
            options: &options,
        })?
        .ok_or_else(|| anyhow::anyhow!("expected one eligible historical opportunity"))?;

        assert_eq!(selected.request_key, "agent_trace/v2|verify|normal");
        assert_eq!(selected.observed_cost_micro_usd, 900);
        assert_eq!(selected.independent_units, 4);
        Ok(())
    }

    #[test]
    fn champion_only_history_starts_exploration_without_promoting() -> Result<()> {
        let lock = lock();
        let snapshot = snapshot();
        let options = options();

        let step = prepare_step(OptimizationStepInput {
            eval: &snapshot,
            active_policy: &lock,
            active_policy_digest: ACTIVE_DIGEST,
            policy_name: "auto",
            options: &options,
        })?;

        assert_eq!(step.action, ControllerAction::Explore);
        let successor = step
            .successor
            .ok_or_else(|| anyhow::anyhow!("exploration must produce a successor"))?;
        assert_eq!(
            successor
                .policies
                .get("auto")
                .ok_or_else(|| anyhow::anyhow!("missing successor policy"))?
                .routes,
            lock.policies
                .get("auto")
                .ok_or_else(|| anyhow::anyhow!("missing parent policy"))?
                .routes
        );
        Ok(())
    }

    #[test]
    fn sufficient_passing_cheaper_evidence_promotes_exactly_one_route() -> Result<()> {
        let lock = active_lock();
        let original = lock.clone();
        let snapshot = active_snapshot(3, 3, EvalVerdict::Pass, 1_000, 700);
        let mut options = options();
        options.evaluator_config_digest = Some(CONFIG_DIGEST.into());

        let step = prepare_step(OptimizationStepInput {
            eval: &snapshot,
            active_policy: &lock,
            active_policy_digest: ACTIVE_DIGEST,
            policy_name: "auto",
            options: &options,
        })?;

        assert_eq!(step.action, ControllerAction::Promote);
        assert_eq!(lock, original);
        let successor = step
            .successor
            .ok_or_else(|| anyhow::anyhow!("promotion must create a successor"))?;
        let parent_policy = lock
            .policies
            .get("auto")
            .ok_or_else(|| anyhow::anyhow!("missing parent policy"))?;
        let successor_policy = successor
            .policies
            .get("auto")
            .ok_or_else(|| anyhow::anyhow!("missing successor policy"))?;
        let changed_routes = parent_policy
            .routes
            .iter()
            .filter(|(key, tier)| successor_policy.routes.get(*key) != Some(*tier))
            .collect::<Vec<_>>();
        assert_eq!(changed_routes.len(), 1);
        assert_eq!(
            successor_policy.routes.get("agent_trace/v2|verify|normal"),
            Some(&"economy".to_string())
        );
        assert!(
            successor_policy
                .optimization
                .as_ref()
                .is_some_and(|state| state.active.is_none())
        );
        let artifact = successor
            .artifact
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing successor provenance"))?;
        assert_eq!(artifact.parent_digest.as_deref(), Some(ACTIVE_DIGEST));
        assert_eq!(artifact.eval_snapshot_root.as_deref(), Some(SNAPSHOT_ROOT));
        assert_eq!(artifact.compiler.id, "bitrouter-history-optimizer");
        assert_eq!(
            successor
                .certificate("auto", "agent_trace/v2|verify|normal")
                .map(|certificate| certificate.verdict),
            Some(PromotionVerdict::Promote)
        );
        Ok(())
    }

    #[test]
    fn passing_quality_with_higher_complete_cost_holds() -> Result<()> {
        let lock = active_lock();
        let snapshot = active_snapshot(3, 3, EvalVerdict::Pass, 1_000, 1_100);
        let options = options();

        let step = prepare_step(OptimizationStepInput {
            eval: &snapshot,
            active_policy: &lock,
            active_policy_digest: ACTIVE_DIGEST,
            policy_name: "auto",
            options: &options,
        })?;

        assert_eq!(step.action, ControllerAction::Hold);
        assert!(step.successor.is_none());
        Ok(())
    }

    #[test]
    fn insufficient_evidence_holds_below_budget_and_retreats_at_budget() -> Result<()> {
        let lock = active_lock();
        let options = options();
        let below_budget = active_snapshot(3, 2, EvalVerdict::Pass, 1_000, 700);
        let hold = prepare_step(OptimizationStepInput {
            eval: &below_budget,
            active_policy: &lock,
            active_policy_digest: ACTIVE_DIGEST,
            policy_name: "auto",
            options: &options,
        })?;
        assert_eq!(hold.action, ControllerAction::Hold);
        assert!(hold.successor.is_none());

        let at_budget = active_snapshot(3, 20, EvalVerdict::Fail, 1_000, 700);
        let retreat = prepare_step(OptimizationStepInput {
            eval: &at_budget,
            active_policy: &lock,
            active_policy_digest: ACTIVE_DIGEST,
            policy_name: "auto",
            options: &options,
        })?;
        assert_eq!(retreat.action, ControllerAction::Retreat);
        let successor = retreat
            .successor
            .ok_or_else(|| anyhow::anyhow!("retreat must create a successor"))?;
        assert_eq!(
            successor
                .policies
                .get("auto")
                .and_then(|policy| policy.routes.get("agent_trace/v2|verify|normal")),
            Some(&"strong".to_string())
        );
        assert_eq!(
            successor
                .certificate("auto", "agent_trace/v2|verify|normal")
                .map(|certificate| certificate.verdict),
            Some(PromotionVerdict::Blocked)
        );
        Ok(())
    }

    #[test]
    fn hard_violation_retreats_immediately_and_keeps_rejections_bounded() -> Result<()> {
        let mut lock = active_lock();
        let policy = lock
            .policies
            .get_mut("auto")
            .ok_or_else(|| anyhow::anyhow!("missing active policy"))?;
        let state = policy
            .optimization
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("missing active optimization"))?;
        state.rejections = (0..256)
            .map(|index| RouteRejection {
                experiment_id: SNAPSHOT_ROOT.into(),
                treatment_context_digest: format!("sha256:{index:064x}"),
                evidence_root: SNAPSHOT_ROOT.into(),
                reason: "prior rejection".into(),
            })
            .collect();
        let mut snapshot = active_snapshot(0, 1, EvalVerdict::Pass, 1_000, 700);
        snapshot.records[0].result.hard_violations = vec!["quality.security".into()];
        let options = options();

        let step = prepare_step(OptimizationStepInput {
            eval: &snapshot,
            active_policy: &lock,
            active_policy_digest: ACTIVE_DIGEST,
            policy_name: "auto",
            options: &options,
        })?;

        assert_eq!(step.action, ControllerAction::Retreat);
        let successor = step
            .successor
            .ok_or_else(|| anyhow::anyhow!("hard violation must publish retreat"))?;
        let rejections = &successor
            .policies
            .get("auto")
            .ok_or_else(|| anyhow::anyhow!("missing successor policy"))?
            .optimization
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing rejection state"))?
            .rejections;
        assert_eq!(rejections.len(), 256);
        assert_eq!(
            rejections
                .last()
                .map(|rejection| rejection.experiment_id.as_str()),
            Some(EXPERIMENT_ID)
        );
        Ok(())
    }

    #[test]
    fn retreat_replaces_an_existing_rejection_for_the_same_treatment_context() -> Result<()> {
        let mut lock = active_lock();
        let exploration = lock
            .policies
            .get("auto")
            .and_then(|policy| policy.optimization.as_ref())
            .and_then(|state| state.active.as_ref())
            .ok_or_else(|| anyhow::anyhow!("missing active exploration"))?;
        let context = treatment_context_digest(
            ACTIVE_DIGEST,
            "auto",
            &exploration.target_request_key,
            &exploration.champion_tier,
            &exploration.challenger_tier,
            exploration.challenger_exposure_ppm,
            &exploration.gate,
        )?;
        lock.policies
            .get_mut("auto")
            .and_then(|policy| policy.optimization.as_mut())
            .ok_or_else(|| anyhow::anyhow!("missing optimization state"))?
            .rejections
            .push(RouteRejection {
                experiment_id: EXPERIMENT_ID.into(),
                treatment_context_digest: context.clone(),
                evidence_root: CONFIG_DIGEST.into(),
                reason: "prior attempt".into(),
            });
        let mut snapshot = active_snapshot(0, 1, EvalVerdict::Pass, 1_000, 700);
        snapshot.records[0].result.hard_violations = vec!["quality.security".into()];

        let step = prepare_step(OptimizationStepInput {
            eval: &snapshot,
            active_policy: &lock,
            active_policy_digest: ACTIVE_DIGEST,
            policy_name: "auto",
            options: &options(),
        })?;

        let successor = step
            .successor
            .ok_or_else(|| anyhow::anyhow!("retreat must publish a successor"))?;
        let rejections = &successor
            .policies
            .get("auto")
            .and_then(|policy| policy.optimization.as_ref())
            .ok_or_else(|| anyhow::anyhow!("missing successor rejection state"))?
            .rejections;
        assert_eq!(
            rejections
                .iter()
                .filter(|rejection| rejection.treatment_context_digest == context)
                .count(),
            1
        );
        assert_eq!(rejections[0].evidence_root, SNAPSHOT_ROOT);
        Ok(())
    }

    #[test]
    fn no_eligible_unrejected_route_converges_without_a_successor() -> Result<()> {
        let mut lock = lock();
        for certificate in lock
            .certificates
            .get_mut("auto")
            .into_iter()
            .flat_map(|certificates| certificates.values_mut())
        {
            certificate.owner = RouteOwner::Operator;
            certificate.source = CertificateSource::Operator;
        }
        let snapshot = snapshot();

        let step = prepare_step(OptimizationStepInput {
            eval: &snapshot,
            active_policy: &lock,
            active_policy_digest: ACTIVE_DIGEST,
            policy_name: "auto",
            options: &options(),
        })?;

        assert_eq!(step.action, ControllerAction::Converged);
        assert!(step.successor.is_none());
        Ok(())
    }
}
