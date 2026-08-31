use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::eval::compiler::EvalEvidenceSnapshot;
use crate::eval::store::EvalSnapshot;
use crate::eval::types::{EvalScope, EvalSubject};
use crate::optimization::cohort::{CohortAssessment, CohortGateVerdict, assess_cohort};
use crate::optimization::exploration::{
    OptimizationGate, PolicyOptimizationState, RouteExploration, RouteRejection,
};
use crate::optimization::identity::{
    HISTORY_OPTIMIZER_ID, HISTORY_OPTIMIZER_VERSION, canonical_digest, experiment_id,
    explore_compiler_config_digest, retreat_compiler_config_digest, treatment_context_digest,
};
use crate::policy_lock::{
    CertificateSource, CompilerIdentity, EconomicsSummary, PolicyArtifact, PolicyCertificate,
    PolicyLock, PromotionVerdict, QualitySummary, RouteOwner, validate_document,
};
use crate::workflow_state::predictive::compiled_predictor_contract;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OptimizationOptions {
    pub policy: String,
    pub candidate_tier: Option<String>,
    pub challenger_exposure_ppm: u32,
    pub minimum_tasks_per_arm: u32,
    pub maximum_challenger_tasks: u32,
    pub minimum_pass_rate_ppm: u32,
    pub evaluator_config_digest: Option<String>,
}

impl Default for OptimizationOptions {
    fn default() -> Self {
        Self {
            policy: "auto".into(),
            candidate_tier: None,
            challenger_exposure_ppm: 100_000,
            minimum_tasks_per_arm: 3,
            maximum_challenger_tasks: 20,
            minimum_pass_rate_ppm: 900_000,
            evaluator_config_digest: None,
        }
    }
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

    fn candidate_tier(&self) -> Result<&str> {
        self.candidate_tier
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("optimization candidate tier was not resolved"))
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

#[derive(Debug)]
pub struct PreparedOptimizationStep {
    pub step: OptimizationStep,
    pub treatment: Option<RouteExploration>,
    pub config_path: PathBuf,
    pub policy_path: PathBuf,
    pub parent_policy_digest: String,
    pub config_before: String,
    pub policy_mode: bitrouter_sdk::config::PolicyRuntimeMode,
    eval_snapshot: EvalSnapshot,
    database_url: String,
}

#[derive(Debug)]
pub struct OptimizationPublication {
    pub action: ControllerAction,
    pub parent_policy_digest: String,
    pub active_policy_digest: String,
    pub eval_snapshot_root: String,
    pub published: bool,
    pub config_activated: bool,
    pub config_path: PathBuf,
    pub config_before: String,
    pub config_after: String,
    pub update: Option<crate::policy_lock::PolicyFileUpdate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OptimizationStatus {
    pub policy: String,
    pub policy_mode: bitrouter_sdk::config::PolicyRuntimeMode,
    pub active_policy_digest: String,
    pub parent_policy_digest: Option<String>,
    pub eval_snapshot_root: Option<String>,
    pub observed_subject_digest: Option<String>,
    pub active_experiment: Option<RouteExploration>,
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
    if policy.optimization.as_ref().is_some_and(|state| {
        state
            .rejections
            .iter()
            .any(|rejection| rejection.treatment_context_digest.is_none())
    }) {
        return Ok(None);
    }
    let mut aggregate = BTreeMap::<String, OpportunityAggregate>::new();
    let mut request_subjects = BTreeMap::<(String, String), &EvalSubject>::new();
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
            let route = aggregate
                .entry(decision.route_projection.clone())
                .or_default();
            match record.subject.scope {
                EvalScope::Request => {
                    let key = (
                        record.subject.subject_id.clone(),
                        decision.route_projection.clone(),
                    );
                    request_subjects
                        .entry(key)
                        .and_modify(|selected| {
                            if (
                                record.subject.eval_id.as_str(),
                                record.subject.evidence_digest.as_str(),
                            ) < (selected.eval_id.as_str(), selected.evidence_digest.as_str())
                            {
                                *selected = &record.subject;
                            }
                        })
                        .or_insert(&record.subject);
                }
                EvalScope::Task | EvalScope::Episode => {
                    route
                        .independent_units
                        .insert((record.subject.scope, record.subject.subject_id.clone()));
                }
            }
        }
    }
    for ((_, request_key), subject) in request_subjects {
        let route = aggregate.entry(request_key.clone()).or_default();
        route.request_count = route
            .request_count
            .checked_add(1)
            .context("counting unique request opportunity observations")?;
        for evidence in &subject.evidence {
            if evidence.kind != "request.outcome" {
                continue;
            }
            if let Some(value) = evidence.attributes.get("cost_micro_usd") {
                let cost = value
                    .parse::<u64>()
                    .with_context(|| format!("parsing request cost for '{request_key}'"))?;
                route.observed_cost_micro_usd = route
                    .observed_cost_micro_usd
                    .checked_add(cost)
                    .context("summing unique request opportunity cost")?;
                route.found_cost = true;
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
        if champion_tier == input.options.candidate_tier()?
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
            input.policy_name,
            &request_key,
            champion_tier,
            input.options.candidate_tier()?,
            input.options.challenger_exposure_ppm,
            &gate,
        )?;
        if policy.optimization.as_ref().is_some_and(|state| {
            state.rejections.iter().any(|rejection| {
                rejection.treatment_context_digest.as_deref() == Some(context.as_str())
            })
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
        input.policy_name,
        &target.request_key,
        &target.champion_tier,
        input.options.candidate_tier()?,
        input.options.challenger_exposure_ppm,
        &gate,
    )?;
    let experiment_id = experiment_id(input.active_policy_digest, &context_digest)?;
    let compiler_config_digest = explore_compiler_config_digest(
        input.policy_name,
        input.options.candidate_tier()?,
        input.options.challenger_exposure_ppm,
        &gate,
    )?;
    let mut successor = input.active_policy.clone();
    prune_superseded_route_less_optimizer_certificates(&mut successor);
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
        challenger_tier: input.options.candidate_tier()?.into(),
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
            baseline_tier: Some(target.champion_tier.clone()),
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
        | CohortGateVerdict::AmbiguousEvaluator
            if budget_reached =>
        {
            ControllerAction::Retreat
        }
        CohortGateVerdict::Pass
        | CohortGateVerdict::InsufficientEvidence
        | CohortGateVerdict::QualityFailed
        | CohortGateVerdict::AmbiguousEvaluator => ControllerAction::Hold,
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
    prune_superseded_route_less_optimizer_certificates(&mut successor);
    let compiler_config_digest = retreat_compiler_config_digest(exploration)?;
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
            policy.predictor = Some(compiled_predictor_contract());
            (
                exploration.challenger_tier.clone(),
                PromotionVerdict::Promote,
            )
        }
        ControllerAction::Retreat => {
            let experiment_parent_digest = input
                .active_policy
                .artifact
                .as_ref()
                .and_then(|artifact| artifact.parent_digest.clone())
                .context("active exploration artifact has no parent digest")?;
            let treatment_context_digest = treatment_context_digest(
                input.policy_name,
                &exploration.target_request_key,
                &exploration.champion_tier,
                &exploration.challenger_tier,
                exploration.challenger_exposure_ppm,
                &exploration.gate,
            )?;
            state.rejections.retain(|rejection| {
                rejection.treatment_context_digest.as_deref()
                    != Some(treatment_context_digest.as_str())
            });
            if state.rejections.len() == 256 {
                state.rejections.remove(0);
            }
            state.rejections.push(RouteRejection {
                experiment_id: exploration.experiment_id.clone(),
                target_request_key: Some(exploration.target_request_key.clone()),
                treatment_context_digest: Some(treatment_context_digest),
                treatment: Some(exploration.clone()),
                experiment_parent_digest: Some(experiment_parent_digest),
                source_policy_digest: Some(input.active_policy_digest.into()),
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

fn prune_superseded_route_less_optimizer_certificates(document: &mut PolicyLock) {
    let Some(artifact) = document.artifact.as_ref() else {
        return;
    };
    if artifact.compiler.id != HISTORY_OPTIMIZER_ID
        || artifact.compiler.version != HISTORY_OPTIMIZER_VERSION
    {
        return;
    }
    for (policy_name, certificates) in &mut document.certificates {
        let Some(policy) = document.policies.get(policy_name) else {
            continue;
        };
        certificates.retain(|request_key, certificate| {
            policy.routes.contains_key(request_key)
                || certificate.owner != RouteOwner::Compiler
                || certificate.source != CertificateSource::Mixed
                || certificate.compiler_config_digest != artifact.compiler.config_digest
                || certificate.evidence_digest != artifact.evidence_root
                || !matches!(
                    certificate.verdict,
                    PromotionVerdict::Experiment | PromotionVerdict::Blocked
                )
        });
    }
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

pub async fn prepare_files(
    config_path: &Path,
    mut options: OptimizationOptions,
) -> Result<PreparedOptimizationStep> {
    let config_before = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config = bitrouter_sdk::config::parse(&config_before)
        .with_context(|| format!("parsing {}", config_path.display()))?;
    let active = crate::policy_lock::load_for_config(&config, Some(config_path))
        .await?
        .ok_or_else(|| anyhow::anyhow!("no policy lock is configured"))?;
    let policy = active
        .document
        .policies
        .get(&options.policy)
        .ok_or_else(|| anyhow::anyhow!("optimization policy '{}' is missing", options.policy))?;
    let has_active_exploration = policy
        .optimization
        .as_ref()
        .is_some_and(|state| state.active.is_some());
    if options.candidate_tier.is_none() && !has_active_exploration {
        options.candidate_tier = Some(policy.adequacy.explore_tier.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "optimization policy '{}' has no adequacy.explore_tier; pass --candidate-tier",
                options.policy
            )
        })?);
    }
    let database_url = crate::db::anchor_url(
        &config.database.url,
        config_path.parent().unwrap_or_else(|| Path::new(".")),
    );
    let readonly_database_url = readonly_database_url(&database_url)?;
    let database = crate::db::connect(&readonly_database_url).await?;
    let store = crate::eval::store::EvalStore::new(database);
    let frozen_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let manifest = store.materialize_snapshot(&frozen_at).await?;
    let eval = EvalEvidenceSnapshot::from_manifest(&store, manifest.clone()).await?;
    let step = prepare_step(OptimizationStepInput {
        eval: &eval,
        active_policy: &active.document,
        active_policy_digest: &active.digest,
        policy_name: &options.policy,
        options: &options,
    })?;
    let treatment = active
        .document
        .policies
        .get(&options.policy)
        .and_then(|policy| policy.optimization.as_ref())
        .and_then(|state| state.active.clone())
        .or_else(|| {
            step.successor
                .as_ref()
                .and_then(|successor| successor.policies.get(&options.policy))
                .and_then(|policy| policy.optimization.as_ref())
                .and_then(|state| state.active.clone())
        });
    Ok(PreparedOptimizationStep {
        step,
        treatment,
        config_path: config_path.to_path_buf(),
        policy_path: active.path,
        parent_policy_digest: active.digest,
        config_before,
        policy_mode: config.policy.mode,
        eval_snapshot: manifest,
        database_url,
    })
}

fn readonly_database_url(database_url: &str) -> Result<String> {
    let Some(after_scheme) = database_url
        .strip_prefix("sqlite://")
        .or_else(|| database_url.strip_prefix("sqlite:"))
    else {
        return Ok(database_url.to_string());
    };
    let (path, query) = after_scheme
        .split_once('?')
        .map_or((after_scheme, None), |(path, query)| (path, Some(query)));
    if path.is_empty() || path == ":memory:" {
        anyhow::bail!("history-driven optimization requires a persistent Eval database");
    }
    if !Path::new(path).is_file() {
        anyhow::bail!("Eval database '{path}' does not exist or is not migrated");
    }
    let mut parameters = query
        .into_iter()
        .flat_map(|query| query.split('&'))
        .filter(|parameter| !parameter.is_empty() && !parameter.starts_with("mode="))
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    parameters.push("mode=ro".into());
    Ok(format!("sqlite://{path}?{}", parameters.join("&")))
}

pub async fn publish_prepared(
    prepared: PreparedOptimizationStep,
) -> Result<OptimizationPublication> {
    publish_prepared_with_config_writer(prepared, |path, expected, updated| {
        crate::policy_lock::write_text_atomic_unlocked(path, expected, updated)
    })
    .await
}

async fn publish_prepared_with_config_writer<F>(
    prepared: PreparedOptimizationStep,
    config_writer: F,
) -> Result<OptimizationPublication>
where
    F: FnOnce(&Path, &str, &str) -> Result<()>,
{
    let PreparedOptimizationStep {
        step,
        treatment: _,
        config_path,
        policy_path,
        parent_policy_digest,
        config_before,
        policy_mode,
        eval_snapshot,
        database_url,
    } = prepared;
    let Some(successor) = step.successor else {
        return Ok(OptimizationPublication {
            action: step.action,
            parent_policy_digest: parent_policy_digest.clone(),
            active_policy_digest: parent_policy_digest,
            eval_snapshot_root: step.evidence.eval_snapshot_root,
            published: false,
            config_activated: false,
            config_path,
            config_before: config_before.clone(),
            config_after: config_before,
            update: None,
        });
    };

    let _config_guard = crate::policy_lock::acquire_publication_lock(&config_path)?;
    let _policy_guard = crate::policy_lock::acquire_publication_lock(&policy_path)?;
    let current_config = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    if current_config != config_before {
        anyhow::bail!(
            "config changed since optimization preparation; refusing to publish {}",
            config_path.display()
        );
    }
    let parsed = bitrouter_sdk::config::parse(&current_config)
        .with_context(|| format!("parsing {}", config_path.display()))?;
    let active = crate::policy_lock::load_for_config(&parsed, Some(&config_path))
        .await?
        .ok_or_else(|| anyhow::anyhow!("no policy lock is configured"))?;
    if active.path != policy_path || active.digest != parent_policy_digest {
        anyhow::bail!(
            "policy lock changed since it was loaded (expected {}, found {}); refusing to overwrite",
            parent_policy_digest,
            active.digest
        );
    }
    let config_after = if policy_mode == bitrouter_sdk::config::PolicyRuntimeMode::Frozen {
        crate::policy_lock::edit_config_mode(
            &config_before,
            bitrouter_sdk::config::PolicyRuntimeMode::Adaptive,
        )?
    } else {
        config_before.clone()
    };
    let candidate_config = bitrouter_sdk::config::parse(&config_after)
        .context("validating optimization config activation")?;
    crate::policy_lock::validate_for_config(&candidate_config, &successor)?;
    let database = crate::db::connect(&database_url).await?;
    let store = crate::eval::store::EvalStore::new(database);
    let persisted = store.persist_snapshot(&eval_snapshot).await?;
    if persisted.evidence_root != step.evidence.eval_snapshot_root {
        anyhow::bail!("persisted Eval snapshot does not match the prepared controller evidence");
    }
    if config_after != config_before
        && let Err(error) = config_writer(&config_path, &config_before, &config_after)
    {
        let recovery =
            restore_config_after_activation_error(&config_path, &config_before, &config_after);
        return match recovery {
            Ok(()) => {
                Err(error.context("config activation failed; restored exact pre-step config bytes"))
            }
            Err(recovery) => Err(error.context(format!(
                "config activation failed and config recovery also failed: {recovery:#}"
            ))),
        };
    }

    let child_digest = crate::policy_lock::semantic_digest(&successor)?;
    let history_dir = crate::policy_lock::default_history_dir(&policy_path);
    let record = match crate::policy_lock::publish_candidate_unlocked(
        &policy_path,
        &parent_policy_digest,
        &successor,
        &history_dir,
    ) {
        Ok(record) => record,
        Err(error) => {
            let recovery = recover_failed_publication(
                &config_path,
                &config_before,
                &config_after,
                &policy_path,
                &parent_policy_digest,
                &child_digest,
                &history_dir,
            )
            .await;
            return match recovery {
                Ok(()) => {
                    Err(error.context("policy publication failed; restored config and policy"))
                }
                Err(recovery) => Err(error.context(format!(
                    "policy publication failed and recovery also failed: {recovery:#}"
                ))),
            };
        }
    };
    let update = crate::policy_lock::PolicyFileUpdate {
        path: policy_path,
        digest: record.child_digest.clone(),
        document: successor,
        changes: Vec::new(),
        conflicts: Vec::new(),
    };
    Ok(OptimizationPublication {
        action: step.action,
        parent_policy_digest,
        active_policy_digest: record.child_digest,
        eval_snapshot_root: step.evidence.eval_snapshot_root,
        published: true,
        config_activated: config_after != config_before,
        config_path,
        config_before,
        config_after,
        update: Some(update),
    })
}

fn restore_config_after_activation_error(
    config_path: &Path,
    config_before: &str,
    config_after: &str,
) -> Result<()> {
    let current = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading config recovery target {}", config_path.display()))?;
    if current == config_before {
        return Ok(());
    }
    if current != config_after {
        anyhow::bail!("config changed during activation recovery");
    }
    crate::policy_lock::write_text_atomic_unlocked(config_path, config_after, config_before)
        .context("restoring config after activation failure")
}

async fn recover_failed_publication(
    config_path: &Path,
    config_before: &str,
    config_after: &str,
    policy_path: &Path,
    parent_digest: &str,
    child_digest: &str,
    history_dir: &Path,
) -> Result<()> {
    let policy_restore = match crate::policy_lock::load(policy_path).await {
        Ok(current) if current.digest == parent_digest => Ok(()),
        Ok(current) if current.digest == child_digest => {
            crate::policy_lock::rollback_to_digest_unlocked(
                policy_path,
                child_digest,
                parent_digest,
                history_dir,
            )
            .map(|_| ())
        }
        Ok(current) => Err(anyhow::anyhow!(
            "active policy changed during publication recovery (found {})",
            current.digest
        )),
        Err(error) => Err(error.context("loading policy during publication recovery")),
    };
    let config_restore = match std::fs::read_to_string(config_path) {
        Ok(current) if current == config_before => Ok(()),
        Ok(current) if current == config_after => {
            crate::policy_lock::write_text_atomic_unlocked(config_path, config_after, config_before)
        }
        Ok(_) => Err(anyhow::anyhow!(
            "config changed during optimization publication recovery"
        )),
        Err(error) => Err(error).context("reading config during publication recovery"),
    };
    match (policy_restore, config_restore) {
        (Ok(()), Ok(())) => Ok(()),
        (policy_restore, config_restore) => anyhow::bail!(
            "publication recovery was incomplete (policy: {}; config: {})",
            policy_restore
                .err()
                .map(|error| format!("{error:#}"))
                .unwrap_or_else(|| "ok".into()),
            config_restore
                .err()
                .map(|error| format!("{error:#}"))
                .unwrap_or_else(|| "ok".into()),
        ),
    }
}

pub async fn read_status(config_path: &Path, policy_name: &str) -> Result<OptimizationStatus> {
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config = bitrouter_sdk::config::parse(&raw)
        .with_context(|| format!("parsing {}", config_path.display()))?;
    let active = crate::policy_lock::load_for_config(&config, Some(config_path))
        .await?
        .ok_or_else(|| anyhow::anyhow!("no policy lock is configured"))?;
    let policy = active
        .document
        .policies
        .get(policy_name)
        .ok_or_else(|| anyhow::anyhow!("optimization policy '{policy_name}' is missing"))?;
    Ok(OptimizationStatus {
        policy: policy_name.into(),
        policy_mode: config.policy.mode,
        active_policy_digest: active.digest,
        parent_policy_digest: active
            .document
            .artifact
            .as_ref()
            .and_then(|artifact| artifact.parent_digest.clone()),
        eval_snapshot_root: active
            .document
            .artifact
            .as_ref()
            .and_then(|artifact| artifact.eval_snapshot_root.clone()),
        observed_subject_digest: active
            .document
            .artifact
            .as_ref()
            .map(|artifact| artifact.evidence_root.clone()),
        active_experiment: policy
            .optimization
            .as_ref()
            .and_then(|state| state.active.clone()),
    })
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
        RouteOwner, semantic_digest,
    };
    use crate::workflow_state::predictive::compiled_predictor_contract;

    use super::{
        ControllerAction, HISTORY_OPTIMIZER_ID, HISTORY_OPTIMIZER_VERSION, OptimizationOptions,
        OptimizationStepInput, prepare_step, prune_superseded_route_less_optimizer_certificates,
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
    const OTHER_CONFIG_DIGEST: &str =
        "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

    fn options() -> OptimizationOptions {
        OptimizationOptions {
            policy: "auto".into(),
            candidate_tier: Some("economy".into()),
            challenger_exposure_ppm: 100_000,
            minimum_tasks_per_arm: 3,
            maximum_challenger_tasks: 20,
            minimum_pass_rate_ppm: 900_000,
            evaluator_config_digest: None,
        }
    }

    fn lock() -> PolicyLock {
        let request_keys = [
            "agent_route/v1|unknown|verify|normal",
            "agent_route/v1|unknown|implement|normal",
            "agent_route/v1|code:review|verify|normal",
            "agent_route/v1|unknown|orchestrate|normal",
            "agent_route/v1|agent:multi_step_planning|orchestrate|normal",
            "agent_route/v1|unknown|verify|guarded",
            "agent_route/v1|code:debugging|implement|normal",
            "agent_route/v1|unknown|finalize|normal",
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
        policy.routes.insert(
            "agent_route/v1|agent:multi_step_planning|orchestrate|normal".into(),
            "economy".into(),
        );
        policy.predictor = Some(compiled_predictor_contract());

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
                        owner: if request_key == "agent_route/v1|unknown|orchestrate|normal" {
                            RouteOwner::Operator
                        } else {
                            RouteOwner::Compiler
                        },
                        selected_tier,
                        baseline_tier: None,
                        source: if request_key == "agent_route/v1|unknown|orchestrate|normal" {
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
                    route_projection: request_key.into(),
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
            ("agent_route/v1|unknown|verify|normal", 900, 4),
            ("agent_route/v1|unknown|implement|normal", 500, 5),
            ("agent_route/v1|code:review|verify|normal", 900, 3),
            ("agent_route/v1|unknown|orchestrate|normal", 2_000, 5),
            (
                "agent_route/v1|agent:multi_step_planning|orchestrate|normal",
                1_900,
                5,
            ),
            ("agent_route/v1|unknown|verify|guarded", 1_800, 5),
            ("agent_route/v1|code:debugging|implement|normal", 1_700, 0),
            ("agent_route/v1|unknown|finalize|normal", 1_600, 5),
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
        if let Some(artifact) = lock.artifact.as_mut() {
            artifact.parent_digest = Some(CONFIG_DIGEST.into());
        }
        lock.policies.get_mut("auto").and_then(|policy| {
            policy.optimization = Some(PolicyOptimizationState {
                active: Some(crate::optimization::exploration::RouteExploration {
                    experiment_id: EXPERIMENT_ID.into(),
                    target_request_key: "agent_route/v1|unknown|implement|normal".into(),
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
        let mut record = unit_record("agent_route/v1|unknown|implement|normal", subject_id);
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
            "auto",
            "agent_route/v1|unknown|finalize|normal",
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
                target_request_key: Some("agent_route/v1|unknown|finalize|normal".into()),
                treatment_context_digest: Some(rejected_context),
                treatment: None,
                experiment_parent_digest: None,
                source_policy_digest: None,
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

        assert_eq!(selected.request_key, "agent_route/v1|unknown|verify|normal");
        assert_eq!(selected.observed_cost_micro_usd, 900);
        assert_eq!(selected.independent_units, 4);
        Ok(())
    }

    #[test]
    fn unmigratable_legacy_rejection_conservatively_suppresses_new_opportunities() -> Result<()> {
        let legacy: RouteRejection = serde_json::from_str(
            r#"{
                "experiment_id":"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "evidence_root":"sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
                "reason":"legacy rejected treatment"
            }"#,
        )?;
        assert!(legacy.target_request_key.is_none());
        assert!(legacy.treatment_context_digest.is_none());
        let mut lock = lock();
        lock.policies
            .get_mut("auto")
            .ok_or_else(|| anyhow::anyhow!("missing test policy"))?
            .optimization = Some(PolicyOptimizationState {
            active: None,
            rejections: vec![legacy],
        });
        let snapshot = snapshot();
        let options = options();

        let selected = select_opportunity(OptimizationStepInput {
            eval: &snapshot,
            active_policy: &lock,
            active_policy_digest: ACTIVE_DIGEST,
            policy_name: "auto",
            options: &options,
        })?;

        assert!(selected.is_none());
        Ok(())
    }

    #[test]
    fn request_result_multiplicity_does_not_change_opportunity_ranking() -> Result<()> {
        let repeated = request_record(
            "agent_route/v1|unknown|implement|normal",
            "shared-request-subject",
            Some(600),
        );
        let mut duplicate_result = repeated.clone();
        duplicate_result.result_id = "result-shared-request-subject-duplicate".into();
        duplicate_result.subject.eval_id = "eval-z-shared-request-subject".into();
        duplicate_result.result.eval_id = duplicate_result.subject.eval_id.clone();
        duplicate_result.subject.evidence[0]
            .attributes
            .insert("cost_micro_usd".into(), "900".into());
        duplicate_result.result.idempotency_key =
            "idempotency-shared-request-subject-duplicate".into();
        let mut records = vec![
            repeated,
            duplicate_result,
            request_record(
                "agent_route/v1|unknown|verify|normal",
                "larger-request-subject",
                Some(1_000),
            ),
            unit_record("agent_route/v1|unknown|implement|normal", "edit-task"),
            unit_record("agent_route/v1|unknown|verify|normal", "verify-task"),
        ];
        let snapshot = EvalEvidenceSnapshot {
            evidence_root: SNAPSHOT_ROOT.into(),
            frozen_at: "2026-08-17T00:00:02Z".into(),
            records: records.clone(),
        };
        let lock = lock();
        let options = options();

        let selected = select_opportunity(OptimizationStepInput {
            eval: &snapshot,
            active_policy: &lock,
            active_policy_digest: ACTIVE_DIGEST,
            policy_name: "auto",
            options: &options,
        })?
        .ok_or_else(|| anyhow::anyhow!("expected one opportunity"))?;

        assert_eq!(selected.request_key, "agent_route/v1|unknown|verify|normal");
        assert_eq!(selected.observed_cost_micro_usd, 1_000);
        records.reverse();
        let reversed = EvalEvidenceSnapshot {
            evidence_root: SNAPSHOT_ROOT.into(),
            frozen_at: "2026-08-17T00:00:02Z".into(),
            records,
        };
        assert_eq!(
            select_opportunity(OptimizationStepInput {
                eval: &reversed,
                active_policy: &lock,
                active_policy_digest: ACTIVE_DIGEST,
                policy_name: "auto",
                options: &options,
            })?,
            Some(selected)
        );
        Ok(())
    }

    #[test]
    fn opportunity_uses_primary_route_projection_over_matched_fallback() -> Result<()> {
        let route_projection = "agent_route/v1|code:debugging|implement|normal";
        let matched_fallback = "agent_route/v1|unknown|implement|normal";
        let mut lock = lock();
        lock.policies
            .get_mut("auto")
            .ok_or_else(|| anyhow::anyhow!("missing test policy"))?
            .routes
            .insert(route_projection.into(), "strong".into());
        let mut request = request_record(matched_fallback, "projected-request", Some(1_000));
        request.subject.decisions[0].route_projection = route_projection.into();
        let mut task = unit_record(matched_fallback, "projected-task");
        task.subject.decisions[0].route_projection = route_projection.into();
        let snapshot = EvalEvidenceSnapshot {
            evidence_root: SNAPSHOT_ROOT.into(),
            frozen_at: "2026-08-17T00:00:02Z".into(),
            records: vec![request, task],
        };
        let options = options();

        let selected = select_opportunity(OptimizationStepInput {
            eval: &snapshot,
            active_policy: &lock,
            active_policy_digest: ACTIVE_DIGEST,
            policy_name: "auto",
            options: &options,
        })?
        .ok_or_else(|| anyhow::anyhow!("expected projected route opportunity"))?;

        assert_eq!(selected.request_key, route_projection);
        assert_eq!(selected.observed_cost_micro_usd, 1_000);
        assert_eq!(selected.independent_units, 1);
        Ok(())
    }

    #[test]
    fn pruning_preserves_explicit_operator_and_other_compiler_certificates() -> Result<()> {
        let request_key = "agent_route/v1|unknown|implement|normal";
        let mut explicit = lock();
        let artifact = explicit
            .artifact
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("test artifact is missing"))?;
        artifact.compiler.id = HISTORY_OPTIMIZER_ID.into();
        artifact.compiler.version = HISTORY_OPTIMIZER_VERSION;
        artifact.compiler.config_digest = CONFIG_DIGEST.into();
        artifact.evidence_root = SNAPSHOT_ROOT.into();
        let certificate = explicit
            .certificates
            .get_mut("auto")
            .and_then(|certificates| certificates.get_mut(request_key))
            .ok_or_else(|| anyhow::anyhow!("test certificate is missing"))?;
        certificate.owner = RouteOwner::Compiler;
        certificate.source = CertificateSource::Mixed;
        certificate.verdict = PromotionVerdict::Blocked;
        certificate.compiler_config_digest = CONFIG_DIGEST.into();
        certificate.evidence_digest = SNAPSHOT_ROOT.into();

        prune_superseded_route_less_optimizer_certificates(&mut explicit);

        assert!(explicit.certificate("auto", request_key).is_some());

        let mut operator = explicit.clone();
        operator
            .policies
            .get_mut("auto")
            .ok_or_else(|| anyhow::anyhow!("test policy is missing"))?
            .routes
            .remove(request_key);
        let certificate = operator
            .certificates
            .get_mut("auto")
            .and_then(|certificates| certificates.get_mut(request_key))
            .ok_or_else(|| anyhow::anyhow!("test certificate is missing"))?;
        certificate.owner = RouteOwner::Operator;
        certificate.source = CertificateSource::Operator;

        prune_superseded_route_less_optimizer_certificates(&mut operator);

        assert!(operator.certificate("auto", request_key).is_some());

        let mut other_compiler = operator;
        let certificate = other_compiler
            .certificates
            .get_mut("auto")
            .and_then(|certificates| certificates.get_mut(request_key))
            .ok_or_else(|| anyhow::anyhow!("test certificate is missing"))?;
        certificate.owner = RouteOwner::Compiler;
        certificate.source = CertificateSource::Mixed;
        other_compiler
            .artifact
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("test artifact is missing"))?
            .compiler
            .id = "other-optimizer".into();

        prune_superseded_route_less_optimizer_certificates(&mut other_compiler);

        assert!(other_compiler.certificate("auto", request_key).is_some());
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
            successor_policy
                .routes
                .get("agent_route/v1|unknown|implement|normal"),
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
                .certificate("auto", "agent_route/v1|unknown|implement|normal")
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
                .and_then(|policy| policy.routes.get("agent_route/v1|unknown|implement|normal")),
            Some(&"strong".to_string())
        );
        assert_eq!(
            successor
                .certificate("auto", "agent_route/v1|unknown|implement|normal")
                .map(|certificate| certificate.verdict),
            Some(PromotionVerdict::Blocked)
        );
        Ok(())
    }

    #[test]
    fn inferred_evaluator_ambiguity_retreats_at_challenger_budget() -> Result<()> {
        let mut lock = active_lock();
        lock.policies
            .get_mut("auto")
            .and_then(|policy| policy.optimization.as_mut())
            .and_then(|state| state.active.as_mut())
            .ok_or_else(|| anyhow::anyhow!("missing active exploration"))?
            .gate
            .evaluator_config_digest = None;
        let mut snapshot = active_snapshot(3, 20, EvalVerdict::Pass, 1_000, 700);
        for record in snapshot.records.iter_mut().skip(13) {
            record.result.evaluator.config_digest = OTHER_CONFIG_DIGEST.into();
        }

        let step = prepare_step(OptimizationStepInput {
            eval: &snapshot,
            active_policy: &lock,
            active_policy_digest: ACTIVE_DIGEST,
            policy_name: "auto",
            options: &options(),
        })?;

        assert_eq!(step.action, ControllerAction::Retreat);
        assert!(step.successor.is_some());
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
                target_request_key: None,
                treatment_context_digest: Some(format!("sha256:{index:064x}")),
                treatment: None,
                experiment_parent_digest: None,
                source_policy_digest: None,
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
                target_request_key: None,
                treatment_context_digest: Some(context.clone()),
                treatment: None,
                experiment_parent_digest: None,
                source_policy_digest: None,
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
                .filter(|rejection| {
                    rejection.treatment_context_digest.as_deref() == Some(context.as_str())
                })
                .count(),
            1
        );
        assert_eq!(rejections[0].evidence_root, SNAPSHOT_ROOT);
        Ok(())
    }

    #[test]
    fn rejected_treatment_stays_suppressed_across_real_successor_digests() -> Result<()> {
        let initial = lock();
        let initial_digest = semantic_digest(&initial)?;
        let mut history = snapshot();
        history.records.retain(|record| {
            record.subject.decisions.iter().any(|decision| {
                decision.route_projection == "agent_route/v1|code:review|verify|normal"
            })
        });
        set_snapshot_policy_digest(&mut history, &initial_digest);
        let options = options();

        let explore = prepare_step(OptimizationStepInput {
            eval: &history,
            active_policy: &initial,
            active_policy_digest: &initial_digest,
            policy_name: "auto",
            options: &options,
        })?;
        assert_eq!(explore.action, ControllerAction::Explore);
        let exploration_lock = explore
            .successor
            .ok_or_else(|| anyhow::anyhow!("explore must create a successor"))?;
        let exploration_digest = semantic_digest(&exploration_lock)?;
        let experiment_id = exploration_lock
            .policies
            .get("auto")
            .and_then(|policy| policy.optimization.as_ref())
            .and_then(|state| state.active.as_ref())
            .map(|exploration| exploration.experiment_id.clone())
            .ok_or_else(|| anyhow::anyhow!("missing active experiment"))?;

        let mut experiment_history = active_snapshot(3, 20, EvalVerdict::Fail, 1_000, 700);
        set_snapshot_policy_digest(&mut experiment_history, &exploration_digest);
        for record in &mut experiment_history.records {
            record.subject.decisions[0].route_projection =
                "agent_route/v1|code:review|verify|normal".into();
            record.subject.decisions[0].request_key =
                "agent_route/v1|code:review|verify|normal".into();
            if let Some(experiment) = record.subject.decisions[0].experiment.as_mut() {
                experiment.experiment_id = experiment_id.clone();
            }
        }
        let retreat = prepare_step(OptimizationStepInput {
            eval: &experiment_history,
            active_policy: &exploration_lock,
            active_policy_digest: &exploration_digest,
            policy_name: "auto",
            options: &options,
        })?;
        assert_eq!(retreat.action, ControllerAction::Retreat);
        let retreat_lock = retreat
            .successor
            .ok_or_else(|| anyhow::anyhow!("retreat must create a successor"))?;
        let retreat_digest = semantic_digest(&retreat_lock)?;

        set_snapshot_policy_digest(&mut history, &retreat_digest);
        let selected = select_opportunity(OptimizationStepInput {
            eval: &history,
            active_policy: &retreat_lock,
            active_policy_digest: &retreat_digest,
            policy_name: "auto",
            options: &options,
        })?;

        assert!(selected.is_none());
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

    fn set_snapshot_policy_digest(snapshot: &mut EvalEvidenceSnapshot, digest: &str) {
        for record in &mut snapshot.records {
            record.subject.policy_digest = digest.into();
            for decision in &mut record.subject.decisions {
                decision.policy_digest = digest.into();
            }
        }
    }
}

#[cfg(test)]
mod file_tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result};
    use bitrouter_sdk::config::{PolicyModelTarget, PolicyRuntimeMode};
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    use crate::eval::EvalService;
    use crate::eval::admission::SubmissionPrincipal;
    use crate::eval::store::EvalStore;
    use crate::eval::types::{
        EVAL_SCHEMA_VERSION, EvalDecisionRef, EvalExperimentRef, EvalScope, EvalSubject,
        EvalVerdict, EvaluationResult, EvaluatorIdentity, EvaluatorKind, EvidenceItem,
        ExperimentArm, ExperimentAssignmentUnit, MetricUnit, MetricValue, evidence_digest,
    };
    use crate::optimization::controller::{
        ControllerAction, OptimizationOptions, prepare_files, publish_prepared,
        publish_prepared_with_config_writer, read_status,
    };
    use crate::optimization::exploration::{RouteExploration, RouteRejection};
    use crate::policy_lock::{
        CertificateSource, PolicyCertificate, PolicyDefinition, PolicyLock, PromotionVerdict,
        RouteOwner, default_history_dir, deterministic_yaml, load, publish_candidate,
        validate_document,
    };
    use crate::workflow_state::predictive::compiled_predictor_contract;

    const REQUEST_KEY: &str = "agent_route/v1|unknown|implement|normal";
    const SECOND_REQUEST_KEY: &str = "agent_route/v1|code:generation|verify|normal";
    const SHA: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    struct Harness {
        _directory: tempfile::TempDir,
        root: PathBuf,
        config_path: PathBuf,
        policy_path: PathBuf,
        database_url: String,
    }

    impl Harness {
        async fn new(mode: PolicyRuntimeMode) -> Result<Self> {
            let directory = tempfile::tempdir()?;
            let root = directory.path().to_path_buf();
            let config_path = root.join("bitrouter.yaml");
            let policy_path = root.join("policy-lock.yaml");
            let database_path = root.join("eval.db");
            let database_url = format!("sqlite://{}?mode=rwc", database_path.display());
            let mode = match mode {
                PolicyRuntimeMode::Frozen => "frozen",
                PolicyRuntimeMode::Adaptive => "adaptive",
            };
            std::fs::write(
                &config_path,
                format!(
                    "database:\n  url: sqlite://./eval.db\npolicy:\n  mode: {mode}\n  path: policy-lock.yaml\npresets:\n  auto:\n    model: strong-model\n    policy: auto\n"
                ),
            )?;
            let mut policy = PolicyDefinition {
                tiers: BTreeMap::from([
                    (
                        "strong".into(),
                        PolicyModelTarget::Model("strong-model".into()),
                    ),
                    (
                        "economy".into(),
                        PolicyModelTarget::Model("economy-model".into()),
                    ),
                ]),
                default_tier: Some("strong".into()),
                ..Default::default()
            };
            policy.routes.insert(REQUEST_KEY.into(), "strong".into());
            policy.predictor = Some(compiled_predictor_contract());
            policy.adequacy.explore_tier = Some("economy".into());
            let certificate = PolicyCertificate {
                owner: RouteOwner::Compiler,
                selected_tier: "strong".into(),
                baseline_tier: None,
                source: CertificateSource::TaskNative,
                eligible_episodes: 1,
                independent_tasks: 1,
                quality: None,
                economics: None,
                latency: None,
                critical_violations: 0,
                verdict: PromotionVerdict::Retain,
                evaluator_config_digest: None,
                compiler_config_digest: SHA.into(),
                evidence_digest: SHA.into(),
                legacy: None,
            };
            let lock = PolicyLock {
                policies: BTreeMap::from([("auto".into(), policy)]),
                certificates: BTreeMap::from([(
                    "auto".into(),
                    BTreeMap::from([(REQUEST_KEY.into(), certificate)]),
                )]),
                ..PolicyLock::default()
            };
            std::fs::write(&policy_path, deterministic_yaml(&lock)?)?;
            let db = crate::db::connect(&database_url).await?;
            crate::db::run_migrations(&db).await?;
            drop(db);
            Ok(Self {
                _directory: directory,
                root,
                config_path,
                policy_path,
                database_url,
            })
        }

        async fn new_initialized() -> Result<Self> {
            let directory = tempfile::tempdir()?;
            let root = directory.path().to_path_buf();
            let config_path = root.join("bitrouter.yaml");
            let policy_path = root.join("policy-lock.yaml");
            let database_path = root.join("eval.db");
            let database_url = format!("sqlite://{}?mode=rwc", database_path.display());
            std::fs::write(
                &config_path,
                "database:\n  url: sqlite://./eval.db\npolicy:\n  mode: frozen\n  path: policy-lock.yaml\npresets:\n  auto:\n    model: strong-model\n",
            )?;
            let db = crate::db::connect(&database_url).await?;
            crate::db::run_migrations(&db).await?;
            drop(db);
            crate::policy_lock::initialize_files(
                &config_path,
                "auto",
                "auto",
                Some("strong-model"),
                "economy-model",
            )
            .await?;
            Ok(Self {
                _directory: directory,
                root,
                config_path,
                policy_path,
                database_url,
            })
        }

        async fn admit_champion_history(&self) -> Result<()> {
            self.admit_champion_history_for(REQUEST_KEY, "1").await
        }

        async fn admit_champion_history_for(
            &self,
            request_key: &str,
            id_suffix: &str,
        ) -> Result<()> {
            let policy_digest = load(&self.policy_path).await?.digest;
            let db = crate::db::connect(&self.database_url).await?;
            crate::db::run_migrations(&db).await?;
            let store = EvalStore::new(db);
            let service =
                EvalService::new(store.clone(), bitrouter_sdk::config::EvalConfig::default());
            for (scope, id, cost) in [
                (
                    EvalScope::Request,
                    format!("request-{id_suffix}"),
                    Some("900"),
                ),
                (EvalScope::Task, format!("task-{id_suffix}"), None),
            ] {
                let evidence = cost.map_or_else(Vec::new, |cost| {
                    vec![EvidenceItem {
                        evidence_id: format!("evidence-{id}"),
                        kind: "request.outcome".into(),
                        digest: SHA.into(),
                        redacted: true,
                        attributes: BTreeMap::from([("cost_micro_usd".into(), cost.into())]),
                    }]
                });
                let digest = evidence_digest(&evidence)?;
                let eval_id = format!("eval-{id}");
                let subject = EvalSubject {
                    schema_version: EVAL_SCHEMA_VERSION,
                    eval_id: eval_id.clone(),
                    scope,
                    subject_id: id.clone(),
                    policy_digest: policy_digest.clone(),
                    preset: Some("auto".into()),
                    cohort: None,
                    holdout: false,
                    decisions: vec![EvalDecisionRef {
                        decision_id: format!("decision-{id}"),
                        policy: "auto".into(),
                        route_projection: request_key.into(),
                        request_key: request_key.into(),
                        selected_tier: "strong".into(),
                        selected_effort: None,
                        baseline_tier: None,
                        baseline_effort: None,
                        policy_digest: policy_digest.clone(),
                        experiment: None,
                    }],
                    requested_dimensions: BTreeSet::new(),
                    evidence,
                    evidence_digest: digest.clone(),
                    observed_at: "2026-08-17T00:00:00Z".into(),
                };
                store.insert_subject(&subject).await?;
                let result = EvaluationResult {
                    schema_version: EVAL_SCHEMA_VERSION,
                    eval_id,
                    evidence_digest: digest,
                    evaluator: EvaluatorIdentity {
                        authority_id: "local".into(),
                        evaluator_id: "history".into(),
                        kind: EvaluatorKind::TaskNative,
                        version: "1".into(),
                        config_digest: SHA.into(),
                    },
                    verdict: if scope == EvalScope::Request {
                        EvalVerdict::Inconclusive
                    } else {
                        EvalVerdict::Pass
                    },
                    metrics: BTreeMap::new(),
                    hard_violations: Vec::new(),
                    confidence_ppm: None,
                    evidence_refs: Vec::new(),
                    decision_credit: BTreeMap::new(),
                    idempotency_key: format!("result-{id}"),
                    submitted_at: "2026-08-17T00:00:01Z".into(),
                };
                service
                    .submit(result, SubmissionPrincipal::LocalOperator)
                    .await?;
            }
            Ok(())
        }

        async fn admit_challenger_hard_failure(&self) -> Result<()> {
            let active = load(&self.policy_path).await?;
            let exploration = active.document.policies["auto"]
                .optimization
                .as_ref()
                .and_then(|state| state.active.as_ref())
                .context("initialized exploration is not active")?;
            let evidence = Vec::new();
            let digest = evidence_digest(&evidence)?;
            let eval_id = "eval-challenger-hard-failure".to_string();
            let subject = EvalSubject {
                schema_version: EVAL_SCHEMA_VERSION,
                eval_id: eval_id.clone(),
                scope: EvalScope::Task,
                subject_id: "task-challenger-hard-failure".into(),
                policy_digest: active.digest.clone(),
                preset: Some("auto".into()),
                cohort: None,
                holdout: false,
                decisions: vec![EvalDecisionRef {
                    decision_id: "decision-challenger-hard-failure".into(),
                    policy: "auto".into(),
                    route_projection: exploration.target_request_key.clone(),
                    request_key: exploration.target_request_key.clone(),
                    selected_tier: exploration.challenger_tier.clone(),
                    selected_effort: None,
                    baseline_tier: None,
                    baseline_effort: None,
                    policy_digest: active.digest.clone(),
                    experiment: Some(EvalExperimentRef {
                        experiment_id: exploration.experiment_id.clone(),
                        arm: ExperimentArm::Challenger,
                        assignment_unit: ExperimentAssignmentUnit::Task,
                        assignment_id_digest: SHA.into(),
                        challenger_propensity_ppm: exploration.challenger_exposure_ppm,
                    }),
                }],
                requested_dimensions: BTreeSet::new(),
                evidence,
                evidence_digest: digest.clone(),
                observed_at: "2026-08-17T00:00:02Z".into(),
            };
            let store = EvalStore::new(crate::db::connect(&self.database_url).await?);
            store.insert_subject(&subject).await?;
            EvalService::new(store, bitrouter_sdk::config::EvalConfig::default())
                .submit(
                    EvaluationResult {
                        schema_version: EVAL_SCHEMA_VERSION,
                        eval_id,
                        evidence_digest: digest,
                        evaluator: EvaluatorIdentity {
                            authority_id: "local".into(),
                            evaluator_id: "history".into(),
                            kind: EvaluatorKind::TaskNative,
                            version: "1".into(),
                            config_digest: SHA.into(),
                        },
                        verdict: EvalVerdict::Fail,
                        metrics: BTreeMap::new(),
                        hard_violations: vec!["quality.security".into()],
                        confidence_ppm: None,
                        evidence_refs: Vec::new(),
                        decision_credit: BTreeMap::new(),
                        idempotency_key: "result-challenger-hard-failure".into(),
                        submitted_at: "2026-08-17T00:00:03Z".into(),
                    },
                    SubmissionPrincipal::LocalOperator,
                )
                .await?;
            Ok(())
        }

        async fn admit_passing_experiment(&self, id_suffix: &str) -> Result<()> {
            let active = load(&self.policy_path).await?;
            let exploration = active.document.policies["auto"]
                .optimization
                .as_ref()
                .and_then(|state| state.active.as_ref())
                .context("passing experiment is not active")?;
            let store = EvalStore::new(crate::db::connect(&self.database_url).await?);
            let service =
                EvalService::new(store.clone(), bitrouter_sdk::config::EvalConfig::default());
            for (arm, tier, cost) in [
                (ExperimentArm::Control, "strong", 1_000),
                (ExperimentArm::Challenger, "economy", 700),
            ] {
                for index in 0..3 {
                    let subject_id = format!(
                        "task-{id_suffix}-{}-{index}",
                        match arm {
                            ExperimentArm::Control => "control",
                            ExperimentArm::Challenger => "challenger",
                        }
                    );
                    let evidence = Vec::new();
                    let digest = evidence_digest(&evidence)?;
                    let eval_id = format!("eval-{subject_id}");
                    let metrics = BTreeMap::from([
                        (
                            "trajectory.cost.usd_micros".into(),
                            MetricValue::new(cost, MetricUnit::MicroUsd),
                        ),
                        (
                            "trajectory.history_complete".into(),
                            MetricValue::new(1, MetricUnit::Boolean),
                        ),
                    ]);
                    let subject = EvalSubject {
                        schema_version: EVAL_SCHEMA_VERSION,
                        eval_id: eval_id.clone(),
                        scope: EvalScope::Task,
                        subject_id: subject_id.clone(),
                        policy_digest: active.digest.clone(),
                        preset: Some("auto".into()),
                        cohort: None,
                        holdout: false,
                        decisions: vec![EvalDecisionRef {
                            decision_id: format!("decision-{subject_id}"),
                            policy: "auto".into(),
                            route_projection: exploration.target_request_key.clone(),
                            request_key: exploration.target_request_key.clone(),
                            selected_tier: tier.into(),
                            selected_effort: None,
                            baseline_tier: None,
                            baseline_effort: None,
                            policy_digest: active.digest.clone(),
                            experiment: Some(EvalExperimentRef {
                                experiment_id: exploration.experiment_id.clone(),
                                arm,
                                assignment_unit: ExperimentAssignmentUnit::Task,
                                assignment_id_digest: SHA.into(),
                                challenger_propensity_ppm: exploration.challenger_exposure_ppm,
                            }),
                        }],
                        requested_dimensions: metrics.keys().cloned().collect(),
                        evidence,
                        evidence_digest: digest.clone(),
                        observed_at: "2026-08-17T00:00:04Z".into(),
                    };
                    store.insert_subject(&subject).await?;
                    service
                        .submit(
                            EvaluationResult {
                                schema_version: EVAL_SCHEMA_VERSION,
                                eval_id,
                                evidence_digest: digest,
                                evaluator: EvaluatorIdentity {
                                    authority_id: "local".into(),
                                    evaluator_id: "history".into(),
                                    kind: EvaluatorKind::TaskNative,
                                    version: "1".into(),
                                    config_digest: SHA.into(),
                                },
                                verdict: EvalVerdict::Pass,
                                metrics,
                                hard_violations: Vec::new(),
                                confidence_ppm: None,
                                evidence_refs: Vec::new(),
                                decision_credit: BTreeMap::new(),
                                idempotency_key: format!("result-{subject_id}"),
                                submitted_at: "2026-08-17T00:00:05Z".into(),
                            },
                            SubmissionPrincipal::LocalOperator,
                        )
                        .await?;
                }
            }
            Ok(())
        }

        async fn snapshot_count(&self) -> Result<i64> {
            let database = crate::db::connect(&self.database_url).await?;
            let row = database
                .query_one(Statement::from_string(
                    DatabaseBackend::Sqlite,
                    "SELECT COUNT(*) AS count FROM eval_snapshots",
                ))
                .await?
                .context("eval_snapshots count query returned no row")?;
            row.try_get("", "count").map_err(anyhow::Error::from)
        }
    }

    fn file_tree(root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
        let mut files = BTreeMap::new();
        if !root.exists() {
            return Ok(files);
        }
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(&directory)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    pending.push(entry.path());
                } else {
                    files.insert(
                        entry.path().strip_prefix(root)?.into(),
                        std::fs::read(entry.path())?,
                    );
                }
            }
        }
        Ok(files)
    }

    fn active_exploration_mut(document: &mut PolicyLock) -> Result<&mut RouteExploration> {
        document
            .policies
            .get_mut("auto")
            .and_then(|policy| policy.optimization.as_mut())
            .and_then(|state| state.active.as_mut())
            .context("test exploration is missing")
    }

    fn last_rejection_mut(document: &mut PolicyLock) -> Result<&mut RouteRejection> {
        document
            .policies
            .get_mut("auto")
            .and_then(|policy| policy.optimization.as_mut())
            .and_then(|state| state.rejections.last_mut())
            .context("test rejection is missing")
    }

    #[tokio::test]
    async fn champion_history_publishes_one_exploration_descendant() -> Result<()> {
        let harness = Harness::new(PolicyRuntimeMode::Adaptive).await?;
        harness.admit_champion_history().await?;
        let parent = load(&harness.policy_path).await?;
        let snapshots_before = harness.snapshot_count().await?;

        let prepared = prepare_files(&harness.config_path, OptimizationOptions::default()).await?;
        let prepared_root = prepared.step.evidence.eval_snapshot_root.clone();
        assert_eq!(prepared.step.action, ControllerAction::Explore);
        assert_eq!(harness.snapshot_count().await?, snapshots_before);
        let publication = publish_prepared(prepared).await?;

        let active = load(&harness.policy_path).await?;
        assert_eq!(publication.parent_policy_digest, parent.digest);
        assert_eq!(publication.active_policy_digest, active.digest);
        assert_eq!(publication.eval_snapshot_root, prepared_root);
        assert!(publication.published);
        assert_eq!(harness.snapshot_count().await?, snapshots_before + 1);
        assert_eq!(
            active
                .document
                .artifact
                .as_ref()
                .and_then(|artifact| artifact.eval_snapshot_root.as_deref()),
            Some(prepared_root.as_str())
        );
        let store = EvalStore::new(crate::db::connect(&harness.database_url).await?);
        assert_eq!(
            store
                .snapshot_by_root(&prepared_root)
                .await?
                .map(|snapshot| snapshot.evidence_root),
            Some(prepared_root)
        );
        assert_eq!(
            active.document.policies["auto"]
                .optimization
                .as_ref()
                .and_then(|state| state.active.as_ref())
                .map(|exploration| exploration.target_request_key.as_str()),
            Some(REQUEST_KEY)
        );
        Ok(())
    }

    #[tokio::test]
    async fn initialized_default_route_explores_and_retreats_without_materializing_a_route()
    -> Result<()> {
        let harness = Harness::new_initialized().await?;
        harness.admit_champion_history().await?;
        let initialized = load(&harness.policy_path).await?;
        assert!(initialized.document.policies["auto"].routes.is_empty());
        assert!(initialized.document.certificates.is_empty());

        let explore = prepare_files(&harness.config_path, OptimizationOptions::default()).await?;
        assert_eq!(explore.step.action, ControllerAction::Explore);
        assert!(
            explore
                .step
                .successor
                .as_ref()
                .is_some_and(|successor| successor.policies["auto"].routes.is_empty())
        );
        publish_prepared(explore).await?;
        let exploring = load(&harness.policy_path).await?;
        assert!(exploring.document.policies["auto"].routes.is_empty());
        assert_eq!(
            exploring
                .document
                .certificate("auto", REQUEST_KEY)
                .map(|certificate| certificate.verdict),
            Some(PromotionVerdict::Experiment)
        );

        harness.admit_challenger_hard_failure().await?;
        let retreat = prepare_files(&harness.config_path, OptimizationOptions::default()).await?;
        assert_eq!(retreat.step.action, ControllerAction::Retreat);
        assert!(
            retreat
                .step
                .successor
                .as_ref()
                .is_some_and(|successor| successor.policies["auto"].routes.is_empty())
        );
        publish_prepared(retreat).await?;
        let retreated = load(&harness.policy_path).await?;
        assert!(retreated.document.policies["auto"].routes.is_empty());
        assert!(
            retreated.document.policies["auto"]
                .optimization
                .as_ref()
                .is_some_and(|state| state.active.is_none() && state.rejections.len() == 1)
        );
        assert_eq!(
            retreated
                .document
                .certificate("auto", REQUEST_KEY)
                .map(|certificate| certificate.verdict),
            Some(PromotionVerdict::Blocked)
        );
        let mut forged = retreated.document.clone();
        let copied = forged.certificates["auto"][REQUEST_KEY].clone();
        forged
            .certificates
            .get_mut("auto")
            .context("retreated certificates are missing")?
            .insert(
                "agent_route/v1|code:generation|verify|normal".into(),
                copied,
            );
        let forged_error = validate_document(&forged)
            .expect_err("a blocked certificate cannot be copied to another default-derived key");
        assert!(
            forged_error.to_string().contains("no explicit route"),
            "unexpected forged certificate error: {forged_error:#}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn later_default_route_exploration_prunes_a_superseded_blocked_certificate() -> Result<()>
    {
        let harness = Harness::new_initialized().await?;
        harness.admit_champion_history().await?;
        publish_prepared(
            prepare_files(&harness.config_path, OptimizationOptions::default()).await?,
        )
        .await?;
        harness.admit_challenger_hard_failure().await?;
        publish_prepared(
            prepare_files(&harness.config_path, OptimizationOptions::default()).await?,
        )
        .await?;
        let retreated = load(&harness.policy_path).await?;
        assert!(retreated.document.policies["auto"].routes.is_empty());
        assert_eq!(
            retreated
                .document
                .certificate("auto", REQUEST_KEY)
                .map(|certificate| certificate.verdict),
            Some(PromotionVerdict::Blocked)
        );
        harness
            .admit_champion_history_for(SECOND_REQUEST_KEY, "2")
            .await?;

        let next = prepare_files(&harness.config_path, OptimizationOptions::default()).await?;
        let successor = next
            .step
            .successor
            .as_ref()
            .context("second exploration has no successor")?;

        assert_eq!(next.step.action, ControllerAction::Explore);
        assert!(successor.policies["auto"].routes.is_empty());
        assert!(successor.certificate("auto", REQUEST_KEY).is_none());
        assert_eq!(
            successor.policies["auto"]
                .optimization
                .as_ref()
                .map(|state| state.rejections.as_slice()),
            retreated.document.policies["auto"]
                .optimization
                .as_ref()
                .map(|state| state.rejections.as_slice())
        );
        assert_eq!(
            successor.policies["auto"]
                .optimization
                .as_ref()
                .map(|state| state.rejections.len()),
            Some(1)
        );
        assert_eq!(
            successor
                .certificate("auto", SECOND_REQUEST_KEY)
                .map(|certificate| certificate.verdict),
            Some(PromotionVerdict::Experiment)
        );
        validate_document(successor)?;
        Ok(())
    }

    #[tokio::test]
    async fn historical_rejection_survives_changed_context_explore_and_promotion() -> Result<()> {
        let harness = Harness::new_initialized().await?;
        harness.admit_champion_history().await?;
        publish_prepared(
            prepare_files(&harness.config_path, OptimizationOptions::default()).await?,
        )
        .await?;
        harness.admit_challenger_hard_failure().await?;
        publish_prepared(
            prepare_files(&harness.config_path, OptimizationOptions::default()).await?,
        )
        .await?;
        let retreated = load(&harness.policy_path).await?;
        let historical_rejection = retreated.document.policies["auto"]
            .optimization
            .as_ref()
            .and_then(|state| state.rejections.first())
            .context("first retreat did not retain its rejection")?
            .clone();
        harness
            .admit_champion_history_for(REQUEST_KEY, "changed-context")
            .await?;
        let changed_options = OptimizationOptions {
            challenger_exposure_ppm: 200_000,
            ..Default::default()
        };
        let explore = prepare_files(&harness.config_path, changed_options).await?;
        assert_eq!(explore.step.action, ControllerAction::Explore);
        publish_prepared(explore).await?;
        let exploring = load(&harness.policy_path).await?;
        assert_eq!(
            exploring.document.policies["auto"]
                .optimization
                .as_ref()
                .and_then(|state| state.active.as_ref())
                .map(|active| active.challenger_exposure_ppm),
            Some(200_000)
        );
        assert_eq!(
            exploring.document.policies["auto"]
                .optimization
                .as_ref()
                .map(|state| state.rejections.as_slice()),
            Some(std::slice::from_ref(&historical_rejection))
        );
        harness.admit_passing_experiment("changed-context").await?;

        let promote = prepare_files(&harness.config_path, OptimizationOptions::default()).await?;
        assert_eq!(promote.step.action, ControllerAction::Promote);
        let successor = promote
            .step
            .successor
            .as_ref()
            .context("promotion has no successor")?;

        assert_eq!(
            successor.policies["auto"].routes.get(REQUEST_KEY),
            Some(&"economy".to_string())
        );
        assert_eq!(
            successor.policies["auto"]
                .optimization
                .as_ref()
                .map(|state| state.rejections.as_slice()),
            Some(std::slice::from_ref(&historical_rejection))
        );
        validate_document(successor)?;
        Ok(())
    }

    #[tokio::test]
    async fn retreat_persists_exact_treatment_and_policy_provenance() -> Result<()> {
        let harness = Harness::new_initialized().await?;
        harness.admit_champion_history().await?;
        publish_prepared(
            prepare_files(&harness.config_path, OptimizationOptions::default()).await?,
        )
        .await?;
        let exploring = load(&harness.policy_path).await?;
        let treatment = exploring.document.policies["auto"]
            .optimization
            .as_ref()
            .and_then(|state| state.active.as_ref())
            .context("exploration treatment is missing")?
            .clone();
        let experiment_parent_digest = exploring
            .document
            .artifact
            .as_ref()
            .and_then(|artifact| artifact.parent_digest.clone())
            .context("exploration artifact parent is missing")?;
        harness.admit_challenger_hard_failure().await?;
        publish_prepared(
            prepare_files(&harness.config_path, OptimizationOptions::default()).await?,
        )
        .await?;
        let retreated = load(&harness.policy_path).await?;
        let rejection = retreated.document.policies["auto"]
            .optimization
            .as_ref()
            .and_then(|state| state.rejections.last())
            .context("retreat rejection is missing")?;

        assert_eq!(rejection.treatment.as_ref(), Some(&treatment));
        assert_eq!(
            rejection.experiment_parent_digest.as_deref(),
            Some(experiment_parent_digest.as_str())
        );
        assert_eq!(
            rejection.source_policy_digest.as_deref(),
            retreated
                .document
                .artifact
                .as_ref()
                .and_then(|artifact| artifact.parent_digest.as_deref())
        );
        assert_eq!(
            rejection.source_policy_digest.as_deref(),
            Some(exploring.digest.as_str())
        );
        Ok(())
    }

    #[tokio::test]
    async fn default_derived_explore_rejects_independent_identity_mutations() -> Result<()> {
        #[derive(Clone, Copy)]
        enum Mutation {
            CompilerVersion,
            MissingParent,
            MissingEvalRoot,
            ExperimentId,
            Exposure,
            Gate,
            OperatorCertificate,
            ArbitraryRouteLessCertificate,
        }

        let harness = Harness::new_initialized().await?;
        harness.admit_champion_history().await?;
        let valid = prepare_files(&harness.config_path, OptimizationOptions::default())
            .await?
            .step
            .successor
            .context("exploration successor is missing")?;
        validate_document(&valid)?;
        let mut accepted = Vec::new();
        for mutation in [
            Mutation::CompilerVersion,
            Mutation::MissingParent,
            Mutation::MissingEvalRoot,
            Mutation::ExperimentId,
            Mutation::Exposure,
            Mutation::Gate,
            Mutation::OperatorCertificate,
            Mutation::ArbitraryRouteLessCertificate,
        ] {
            let mut forged = valid.clone();
            let label = match mutation {
                Mutation::CompilerVersion => {
                    forged
                        .artifact
                        .as_mut()
                        .context("test artifact is missing")?
                        .compiler
                        .version += 1;
                    "compiler version"
                }
                Mutation::MissingParent => {
                    forged
                        .artifact
                        .as_mut()
                        .context("test artifact is missing")?
                        .parent_digest = None;
                    "missing artifact parent"
                }
                Mutation::MissingEvalRoot => {
                    forged
                        .artifact
                        .as_mut()
                        .context("test artifact is missing")?
                        .eval_snapshot_root = None;
                    "missing eval root"
                }
                Mutation::ExperimentId => {
                    active_exploration_mut(&mut forged)?.experiment_id = SHA.into();
                    "experiment id"
                }
                Mutation::Exposure => {
                    active_exploration_mut(&mut forged)?.challenger_exposure_ppm += 1;
                    "exposure"
                }
                Mutation::Gate => {
                    active_exploration_mut(&mut forged)?
                        .gate
                        .minimum_tasks_per_arm += 1;
                    "gate"
                }
                Mutation::OperatorCertificate => {
                    let certificate = forged
                        .certificates
                        .get_mut("auto")
                        .and_then(|certificates| certificates.get_mut(REQUEST_KEY))
                        .context("test certificate is missing")?;
                    certificate.owner = RouteOwner::Operator;
                    certificate.source = CertificateSource::Operator;
                    "operator route-less certificate"
                }
                Mutation::ArbitraryRouteLessCertificate => {
                    let copied = forged.certificates["auto"][REQUEST_KEY].clone();
                    forged
                        .certificates
                        .get_mut("auto")
                        .context("test certificates are missing")?
                        .insert(SECOND_REQUEST_KEY.into(), copied);
                    "arbitrary route-less certificate"
                }
            };
            if validate_document(&forged).is_ok() {
                accepted.push(label);
            }
        }

        assert!(
            accepted.is_empty(),
            "forged Explore mutations remained valid: {accepted:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn default_derived_blocked_rejects_independent_identity_mutations() -> Result<()> {
        #[derive(Clone, Copy)]
        enum Mutation {
            CompilerVersion,
            MissingParent,
            RejectionId,
            Context,
            TreatmentExperimentId,
            TreatmentExposure,
            ExperimentParent,
            SourcePolicy,
            OperatorCertificate,
            ArbitraryRouteLessCertificate,
        }

        let harness = Harness::new_initialized().await?;
        harness.admit_champion_history().await?;
        publish_prepared(
            prepare_files(&harness.config_path, OptimizationOptions::default()).await?,
        )
        .await?;
        harness.admit_challenger_hard_failure().await?;
        let valid = prepare_files(&harness.config_path, OptimizationOptions::default())
            .await?
            .step
            .successor
            .context("retreat successor is missing")?;
        validate_document(&valid)?;
        let mut accepted = Vec::new();
        for mutation in [
            Mutation::CompilerVersion,
            Mutation::MissingParent,
            Mutation::RejectionId,
            Mutation::Context,
            Mutation::TreatmentExperimentId,
            Mutation::TreatmentExposure,
            Mutation::ExperimentParent,
            Mutation::SourcePolicy,
            Mutation::OperatorCertificate,
            Mutation::ArbitraryRouteLessCertificate,
        ] {
            let mut forged = valid.clone();
            let label = match mutation {
                Mutation::CompilerVersion => {
                    forged
                        .artifact
                        .as_mut()
                        .context("test artifact is missing")?
                        .compiler
                        .version += 1;
                    "compiler version"
                }
                Mutation::MissingParent => {
                    forged
                        .artifact
                        .as_mut()
                        .context("test artifact is missing")?
                        .parent_digest = None;
                    "missing artifact parent"
                }
                Mutation::RejectionId => {
                    last_rejection_mut(&mut forged)?.experiment_id = SHA.into();
                    "rejection experiment id"
                }
                Mutation::Context => {
                    last_rejection_mut(&mut forged)?.treatment_context_digest = Some(SHA.into());
                    "treatment context"
                }
                Mutation::TreatmentExperimentId => {
                    last_rejection_mut(&mut forged)?
                        .treatment
                        .as_mut()
                        .context("test rejected treatment is missing")?
                        .experiment_id = SHA.into();
                    "treatment experiment id"
                }
                Mutation::TreatmentExposure => {
                    last_rejection_mut(&mut forged)?
                        .treatment
                        .as_mut()
                        .context("test rejected treatment is missing")?
                        .challenger_exposure_ppm += 1;
                    "treatment exposure"
                }
                Mutation::ExperimentParent => {
                    last_rejection_mut(&mut forged)?.experiment_parent_digest = Some(SHA.into());
                    "experiment parent"
                }
                Mutation::SourcePolicy => {
                    last_rejection_mut(&mut forged)?.source_policy_digest = Some(SHA.into());
                    "source policy"
                }
                Mutation::OperatorCertificate => {
                    let certificate = forged
                        .certificates
                        .get_mut("auto")
                        .and_then(|certificates| certificates.get_mut(REQUEST_KEY))
                        .context("test certificate is missing")?;
                    certificate.owner = RouteOwner::Operator;
                    certificate.source = CertificateSource::Operator;
                    "operator route-less certificate"
                }
                Mutation::ArbitraryRouteLessCertificate => {
                    let copied = forged.certificates["auto"][REQUEST_KEY].clone();
                    forged
                        .certificates
                        .get_mut("auto")
                        .context("test certificates are missing")?
                        .insert(SECOND_REQUEST_KEY.into(), copied);
                    "arbitrary route-less certificate"
                }
            };
            if validate_document(&forged).is_ok() {
                accepted.push(label);
            }
        }

        assert!(
            accepted.is_empty(),
            "forged Blocked mutations remained valid: {accepted:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn omitted_candidate_tier_uses_the_policy_explore_tier() -> Result<()> {
        let harness = Harness::new(PolicyRuntimeMode::Adaptive).await?;
        harness.admit_champion_history().await?;
        let options = OptimizationOptions {
            candidate_tier: None,
            ..Default::default()
        };

        let prepared = prepare_files(&harness.config_path, options).await?;

        assert_eq!(
            prepared
                .treatment
                .as_ref()
                .map(|exploration| exploration.challenger_tier.as_str()),
            Some("economy")
        );
        Ok(())
    }

    #[tokio::test]
    async fn active_exploration_does_not_resolve_an_unused_candidate_tier() -> Result<()> {
        let harness = Harness::new(PolicyRuntimeMode::Adaptive).await?;
        harness.admit_champion_history().await?;
        publish_prepared(
            prepare_files(
                &harness.config_path,
                OptimizationOptions {
                    candidate_tier: Some("economy".into()),
                    ..Default::default()
                },
            )
            .await?,
        )
        .await?;
        let active = load(&harness.policy_path).await?;
        let mut without_default = active.document;
        without_default
            .policies
            .get_mut("auto")
            .context("test policy is missing")?
            .adequacy
            .explore_tier = None;
        publish_candidate(
            &harness.policy_path,
            &active.digest,
            &without_default,
            &default_history_dir(&harness.policy_path),
        )?;

        let prepared = prepare_files(&harness.config_path, OptimizationOptions::default()).await?;

        assert_eq!(prepared.step.action, ControllerAction::Hold);
        assert_eq!(
            prepared
                .treatment
                .as_ref()
                .map(|exploration| exploration.challenger_tier.as_str()),
            Some("economy")
        );
        Ok(())
    }

    #[tokio::test]
    async fn insufficient_rerun_holds_without_rewriting_policy_bytes() -> Result<()> {
        let harness = Harness::new(PolicyRuntimeMode::Adaptive).await?;
        harness.admit_champion_history().await?;
        publish_prepared(
            prepare_files(&harness.config_path, OptimizationOptions::default()).await?,
        )
        .await?;
        let config_before = std::fs::read(&harness.config_path)?;
        let policy_before = std::fs::read(&harness.policy_path)?;
        let history_dir = default_history_dir(&harness.policy_path);
        let history_before = file_tree(&history_dir)?;
        let snapshots_before = harness.snapshot_count().await?;

        let prepared = prepare_files(&harness.config_path, OptimizationOptions::default()).await?;
        assert_eq!(harness.snapshot_count().await?, snapshots_before);
        assert_eq!(prepared.step.action, ControllerAction::Hold);
        let publication = publish_prepared(prepared).await?;

        assert!(!publication.published);
        assert_eq!(harness.snapshot_count().await?, snapshots_before);
        assert_eq!(std::fs::read(&harness.config_path)?, config_before);
        assert_eq!(std::fs::read(&harness.policy_path)?, policy_before);
        assert_eq!(file_tree(&history_dir)?, history_before);
        Ok(())
    }

    #[tokio::test]
    async fn converged_run_is_database_and_file_read_only() -> Result<()> {
        let harness = Harness::new(PolicyRuntimeMode::Adaptive).await?;
        let config_before = std::fs::read(&harness.config_path)?;
        let policy_before = std::fs::read(&harness.policy_path)?;
        let history_dir = default_history_dir(&harness.policy_path);
        let snapshots_before = harness.snapshot_count().await?;

        let prepared = prepare_files(&harness.config_path, OptimizationOptions::default()).await?;
        assert_eq!(prepared.step.action, ControllerAction::Converged);
        assert_eq!(harness.snapshot_count().await?, snapshots_before);
        let publication = publish_prepared(prepared).await?;

        assert!(!publication.published);
        assert_eq!(harness.snapshot_count().await?, snapshots_before);
        assert_eq!(std::fs::read(&harness.config_path)?, config_before);
        assert_eq!(std::fs::read(&harness.policy_path)?, policy_before);
        assert!(file_tree(&history_dir)?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn stale_prepared_parent_loses_to_competing_policy_publication() -> Result<()> {
        let harness = Harness::new(PolicyRuntimeMode::Adaptive).await?;
        harness.admit_champion_history().await?;
        let snapshots_before = harness.snapshot_count().await?;
        let prepared = prepare_files(&harness.config_path, OptimizationOptions::default()).await?;
        assert_eq!(harness.snapshot_count().await?, snapshots_before);
        let candidate = prepared
            .step
            .successor
            .clone()
            .context("expected exploration successor")?;
        let active = load(&harness.policy_path).await?;
        publish_candidate(
            &harness.policy_path,
            &active.digest,
            &candidate,
            &default_history_dir(&harness.policy_path),
        )?;
        let competing_bytes = std::fs::read(&harness.policy_path)?;

        let error = publish_prepared(prepared)
            .await
            .err()
            .context("stale publish succeeded")?;

        assert!(error.to_string().contains("changed since it was loaded"));
        assert_eq!(harness.snapshot_count().await?, snapshots_before);
        assert_eq!(std::fs::read(&harness.policy_path)?, competing_bytes);
        Ok(())
    }

    #[tokio::test]
    async fn first_mutating_run_activates_a_frozen_config() -> Result<()> {
        let harness = Harness::new(PolicyRuntimeMode::Frozen).await?;
        harness.admit_champion_history().await?;

        publish_prepared(
            prepare_files(&harness.config_path, OptimizationOptions::default()).await?,
        )
        .await?;

        let raw = std::fs::read_to_string(&harness.config_path)?;
        assert_eq!(
            bitrouter_sdk::config::parse(&raw)?.policy.mode,
            PolicyRuntimeMode::Adaptive
        );
        Ok(())
    }

    #[tokio::test]
    async fn publication_failure_restores_exact_config_bytes() -> Result<()> {
        let harness = Harness::new(PolicyRuntimeMode::Frozen).await?;
        harness.admit_champion_history().await?;
        let original_config = std::fs::read(&harness.config_path)?;
        let original_policy = std::fs::read(&harness.policy_path)?;
        std::fs::write(
            default_history_dir(&harness.policy_path),
            b"not a directory",
        )?;

        let error = publish_prepared(
            prepare_files(&harness.config_path, OptimizationOptions::default()).await?,
        )
        .await
        .err()
        .context("publication unexpectedly succeeded")?;

        assert!(error.to_string().contains("policy"));
        assert_eq!(std::fs::read(&harness.config_path)?, original_config);
        assert_eq!(std::fs::read(&harness.policy_path)?, original_policy);
        Ok(())
    }

    #[tokio::test]
    async fn post_rename_config_activation_failure_restores_exact_parent_state() -> Result<()> {
        let harness = Harness::new(PolicyRuntimeMode::Frozen).await?;
        harness.admit_champion_history().await?;
        let original_config = std::fs::read(&harness.config_path)?;
        let original_policy = std::fs::read(&harness.policy_path)?;
        let snapshots_before = harness.snapshot_count().await?;
        let prepared = prepare_files(&harness.config_path, OptimizationOptions::default()).await?;
        let prepared_root = prepared.step.evidence.eval_snapshot_root.clone();

        let error = publish_prepared_with_config_writer(prepared, |path, expected, updated| {
            crate::policy_lock::write_text_atomic_unlocked_with_parent_sync(
                path,
                expected,
                updated,
                |_| anyhow::bail!("injected parent directory sync failure"),
            )
        })
        .await
        .err()
        .context("post-rename config activation unexpectedly succeeded")?;

        assert!(error.to_string().contains("config activation failed"));
        assert_eq!(std::fs::read(&harness.config_path)?, original_config);
        assert_eq!(std::fs::read(&harness.policy_path)?, original_policy);
        assert_eq!(harness.snapshot_count().await?, snapshots_before + 1);
        let store = EvalStore::new(crate::db::connect(&harness.database_url).await?);
        assert!(store.snapshot_by_root(&prepared_root).await?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn status_is_byte_read_only() -> Result<()> {
        let harness = Harness::new(PolicyRuntimeMode::Frozen).await?;
        harness.admit_champion_history().await?;
        let config_before = std::fs::read(&harness.config_path)?;
        let policy_before = std::fs::read(&harness.policy_path)?;
        let snapshots_before = harness.snapshot_count().await?;

        let status = read_status(&harness.config_path, "auto").await?;

        assert_eq!(status.policy, "auto");
        assert_eq!(std::fs::read(&harness.config_path)?, config_before);
        assert_eq!(std::fs::read(&harness.policy_path)?, policy_before);
        assert_eq!(harness.snapshot_count().await?, snapshots_before);
        assert!(!default_history_dir(&harness.policy_path).exists());
        Ok(())
    }

    #[tokio::test]
    async fn controller_creates_no_paired_optimizer_artifacts() -> Result<()> {
        let harness = Harness::new(PolicyRuntimeMode::Adaptive).await?;
        harness.admit_champion_history().await?;
        publish_prepared(
            prepare_files(&harness.config_path, OptimizationOptions::default()).await?,
        )
        .await?;
        read_status(&harness.config_path, "auto").await?;

        let mut pending = vec![harness.root.clone()];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(directory)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    pending.push(entry.path());
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                assert!(!name.starts_with("bitrouter.optimize."), "{name}");
                assert_ne!(name, "bitrouter.eval.md");
                assert_ne!(name, "contract.md");
                assert_ne!(name, "runs");
                assert_ne!(name, "worktrees");
                assert_ne!(name, "private.db");
                assert!(!name.contains("evaluator"), "{name}");
            }
        }
        Ok(())
    }
}
