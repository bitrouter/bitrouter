use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::Digest;

use super::evaluator::{
    AgenticConfidence, AgenticEvaluation, AgenticEvaluationInput, AgenticEvaluatorBackend,
    AgenticVerdict, WorkflowEvidence, content_digest, evaluate_agentic, verify_evaluator_lock,
};
use super::runner::{
    PrivateDaemonPaths, PrivateVariantRequest, VariantEvidence, build_experiment_lock,
    run_private_variant, select_target_request_key,
};
use super::{
    LoadedIntent, OptimizationLock, OptimizationRunLock, OptimizationVerdict, OutcomeSummary,
};
use crate::eval::admission::SubmissionPrincipal;
use crate::eval::types::{
    AdmissionStatus, DecisionCredit, EVAL_SCHEMA_VERSION, EvalScope, EvalSubject, EvalVerdict,
    EvaluationResult, EvaluatorIdentity, EvaluatorKind, EvidenceItem, MetricUnit, MetricValue,
    canonical_digest, evidence_digest,
};
use crate::policy_compile::{CompileInput, LegacyAdequacySnapshot};
use crate::policy_lock::{PromotionVerdict, semantic_digest};

const MAXIMUM_WORKFLOW_OUTPUT_BYTES: usize = 48 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptimizationReport {
    pub run_id: String,
    pub created_at: String,
    pub source_policy_digest: String,
    pub target_request_key: String,
    pub preference: super::OptimizationPreference,
    pub baseline: VariantReport,
    pub candidate: VariantReport,
    pub cost_delta_micro_usd: i64,
    pub cost_delta_ppm: Option<i64>,
    pub latency_observe_only: bool,
    pub eval_snapshot_digest: String,
    pub candidate_digest: String,
    pub candidate_path: PathBuf,
    pub publishable: bool,
    pub caveats: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantReport {
    pub verdict: OptimizationVerdict,
    pub confidence: String,
    pub reason: String,
    pub evidence_digest: String,
    pub policy_digest: String,
    pub request_count: usize,
    pub settled_cost_micro_usd: u64,
    pub observed_latency_ms: u64,
    pub elapsed_ms: u64,
}

pub struct RunOptimizationRequest<'a> {
    pub loaded: &'a LoadedIntent,
    pub optimization_lock: &'a super::LoadedOptimizationLock,
    pub workflow_cwd: &'a Path,
    pub bitrouter_executable: &'a Path,
    pub settlement_bearer: &'a str,
    pub evaluator: &'a dyn AgenticEvaluatorBackend,
}

pub struct RunOptimizationOutcome {
    pub report: OptimizationReport,
    pub report_digest: String,
    pub updated_lock: OptimizationLock,
}

pub async fn run_optimization(
    request: RunOptimizationRequest<'_>,
) -> Result<RunOptimizationOutcome> {
    request.loaded.intent.validate()?;
    request.optimization_lock.document.validate()?;
    if request.optimization_lock.document.intent_digest != request.loaded.digest {
        anyhow::bail!("optimization intent changed; run optimize setup to resolve a new lock");
    }
    let source_raw = tokio::fs::read_to_string(&request.loaded.paths.source_config)
        .await
        .with_context(|| {
            format!(
                "reading source config {}",
                request.loaded.paths.source_config.display()
            )
        })?;
    let source_config =
        bitrouter_sdk::config::parse(&source_raw).context("parsing source optimization config")?;
    let active = crate::policy_lock::load_for_config(
        &source_config,
        Some(&request.loaded.paths.source_config),
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("source config has no active policy lock"))?;
    if active.digest != request.optimization_lock.document.active_policy_digest {
        anyhow::bail!("active policy changed; run optimize setup before starting a new experiment");
    }
    let contract = tokio::fs::read_to_string(&request.loaded.paths.contract)
        .await
        .with_context(|| {
            format!(
                "reading success contract {}",
                request.loaded.paths.contract.display()
            )
        })?;
    let now = chrono::Utc::now();
    let run_id = format!(
        "opt-{}-{}",
        now.format("%Y%m%dT%H%M%S%.3fZ"),
        uuid::Uuid::new_v4().simple()
    );
    let run_root = request.loaded.paths.private_runs.join(&run_id);
    tokio::fs::create_dir_all(&run_root)
        .await
        .with_context(|| format!("creating optimization run root {}", run_root.display()))?;

    let baseline = run_private_variant(PrivateVariantRequest {
        variant: "baseline",
        paths: &PrivateDaemonPaths::new(run_root.join("baseline")),
        intent: &request.loaded.intent,
        policy: &active.document,
        policy_digest: &active.digest,
        workflow_cwd: request.workflow_cwd,
        bitrouter_executable: request.bitrouter_executable,
        settlement_bearer: request.settlement_bearer,
        maximum_output_bytes: MAXIMUM_WORKFLOW_OUTPUT_BYTES,
    })
    .await
    .context("running controlled baseline")?;
    let target_request_key = select_target_request_key(
        &active.document,
        &request.loaded.intent.policy,
        "economy",
        request.loaded.intent.preference,
        &baseline.observations,
    )?;
    let experiment = build_experiment_lock(
        &active.document,
        &active.digest,
        &request.loaded.intent.policy,
        &target_request_key,
        "economy",
    )?;
    let experiment_digest = semantic_digest(&experiment)?;
    let candidate = run_private_variant(PrivateVariantRequest {
        variant: "candidate",
        paths: &PrivateDaemonPaths::new(run_root.join("candidate")),
        intent: &request.loaded.intent,
        policy: &experiment,
        policy_digest: &experiment_digest,
        workflow_cwd: request.workflow_cwd,
        bitrouter_executable: request.bitrouter_executable,
        settlement_bearer: request.settlement_bearer,
        maximum_output_bytes: MAXIMUM_WORKFLOW_OUTPUT_BYTES,
    })
    .await
    .context("running one-key routing candidate")?;
    verify_controlled_candidate(&candidate, &target_request_key)?;

    let baseline_input = evaluation_input(&run_id, "baseline", &contract, &baseline);
    let candidate_input = evaluation_input(&run_id, "candidate", &contract, &candidate);
    verify_evaluator_lock(
        &request.optimization_lock.document.evaluator,
        &baseline_input,
    )?;
    verify_evaluator_lock(
        &request.optimization_lock.document.evaluator,
        &candidate_input,
    )?;
    let baseline_evaluation = evaluate_agentic(request.evaluator, &baseline_input)
        .await
        .context("evaluating baseline workflow outcome")?;
    let candidate_evaluation = evaluate_agentic(request.evaluator, &candidate_input)
        .await
        .context("evaluating candidate workflow outcome")?;

    let database_url = writable_database_url(
        &source_config.database.url,
        &request.loaded.paths.source_config,
    )?;
    let db = crate::db::connect(&database_url)
        .await
        .map_err(anyhow::Error::from)
        .context("opening source Eval database")?;
    crate::db::run_migrations(&db)
        .await
        .map_err(anyhow::Error::from)
        .context("migrating source Eval database")?;
    let eval_store = crate::eval::store::EvalStore::new(db.clone());
    let eval_service =
        crate::eval::EvalService::new(eval_store.clone(), source_config.eval.clone());
    submit_variant_result(
        &eval_service,
        &eval_store,
        &run_id,
        &target_request_key,
        &baseline,
        &baseline_evaluation,
        &request.optimization_lock.document.evaluator,
    )
    .await?;
    submit_variant_result(
        &eval_service,
        &eval_store,
        &run_id,
        &target_request_key,
        &candidate,
        &candidate_evaluation,
        &request.optimization_lock.document.evaluator,
    )
    .await?;
    let frozen_at = chrono::Utc::now().to_rfc3339();
    let snapshot = eval_store.freeze_snapshot(&frozen_at).await?;
    let eval_snapshot =
        crate::eval::compiler::EvalEvidenceSnapshot::load(&eval_store, &snapshot.evidence_root)
            .await?;
    let snapshot_time = chrono::DateTime::parse_from_rfc3339(&frozen_at)
        .context("parsing frozen Eval timestamp")?
        .timestamp_millis();
    let legacy = LegacyAdequacySnapshot::load(
        &crate::adequacy::store::AdequacyStore::new(db),
        snapshot_time,
    )
    .await?;
    let compiled = crate::policy_compile::compile_candidate_with_quality(
        CompileInput {
            current: &active.document,
            parent_digest: Some(&active.digest),
            legacy: &legacy,
            eval: Some(&eval_snapshot),
            proposed_progress_guards: None,
        },
        &request.loaded.intent.promotion_quality_criteria()?,
    )?;
    let candidate_digest = semantic_digest(&compiled.document)?;
    let candidate_path = run_root.join("candidate-policy-lock.yaml");
    tokio::fs::write(
        &candidate_path,
        crate::policy_lock::deterministic_yaml(&compiled.document)?,
    )
    .await
    .with_context(|| format!("writing compiled candidate {}", candidate_path.display()))?;
    let publishable = !candidate.execution.timed_out
        && candidate_evaluation.verdict == AgenticVerdict::Pass
        && !candidate_evaluation.critical_failure
        && compiled.conflicts.is_empty()
        && compiled.changes.iter().any(|change| {
            change.policy == request.loaded.intent.policy
                && change.request_key == target_request_key
                && change.selected_tier == "economy"
                && change.verdict == PromotionVerdict::Promote
        });
    let mut caveats = Vec::new();
    if !publishable {
        caveats
            .push("candidate did not satisfy the pinned quality gate or compiler authority".into());
    }
    let cost_delta_micro_usd = signed_delta(
        candidate.settled_cost_micro_usd,
        baseline.settled_cost_micro_usd,
    );
    let report = OptimizationReport {
        run_id: run_id.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        source_policy_digest: active.digest.clone(),
        target_request_key: target_request_key.clone(),
        preference: request.loaded.intent.preference,
        baseline: variant_report(&baseline, &baseline_evaluation)?,
        candidate: variant_report(&candidate, &candidate_evaluation)?,
        cost_delta_micro_usd,
        cost_delta_ppm: percentage_delta_ppm(
            candidate.settled_cost_micro_usd,
            baseline.settled_cost_micro_usd,
        ),
        latency_observe_only: true,
        eval_snapshot_digest: snapshot.evidence_root.clone(),
        candidate_digest: candidate_digest.clone(),
        candidate_path,
        publishable,
        caveats,
    };
    let report_bytes =
        serde_json::to_vec_pretty(&report).context("serializing optimization report")?;
    let report_digest = format!(
        "sha256:{}",
        hex::encode(sha2::Sha256::digest(&report_bytes))
    );
    tokio::fs::write(run_root.join("report.json"), &report_bytes)
        .await
        .context("writing private optimization report")?;
    let mut updated_lock = request.optimization_lock.document.clone();
    updated_lock.latest_run = Some(OptimizationRunLock {
        run_id,
        source_policy_digest: active.digest.clone(),
        target_request_key,
        baseline: outcome_summary(&report.baseline),
        candidate: outcome_summary(&report.candidate),
        eval_snapshot_digest: snapshot.evidence_root,
        candidate_digest,
        report_digest: report_digest.clone(),
        publishable,
        published: false,
    });
    updated_lock.validate()?;
    Ok(RunOptimizationOutcome {
        report,
        report_digest,
        updated_lock,
    })
}

fn evaluation_input(
    run_id: &str,
    variant: &str,
    contract: &str,
    evidence: &VariantEvidence,
) -> AgenticEvaluationInput {
    AgenticEvaluationInput {
        run_id: run_id.into(),
        variant: variant.into(),
        success_contract: contract.into(),
        evidence: WorkflowEvidence {
            exit_code: evidence.execution.exit_code,
            timed_out: evidence.execution.timed_out,
            elapsed_ms: duration_ms(evidence.execution.elapsed),
            stdout: evidence.execution.stdout.clone(),
            stderr: evidence.execution.stderr.clone(),
        },
    }
}

fn verify_controlled_candidate(candidate: &VariantEvidence, target: &str) -> Result<()> {
    if !candidate
        .attributions
        .iter()
        .any(|item| item.decision.request_key == target && item.decision.selected_tier == "economy")
    {
        anyhow::bail!("candidate did not exercise the one changed economy route");
    }
    Ok(())
}

async fn submit_variant_result(
    service: &crate::eval::EvalService,
    store: &crate::eval::store::EvalStore,
    run_id: &str,
    target_request_key: &str,
    evidence: &VariantEvidence,
    evaluation: &AgenticEvaluation,
    evaluator_lock: &super::EvaluatorLock,
) -> Result<()> {
    let selected = evidence
        .attributions
        .iter()
        .filter(|item| item.decision.request_key == target_request_key)
        .collect::<Vec<_>>();
    if selected.is_empty() {
        anyhow::bail!(
            "{} has no decision for the controlled route",
            evidence.variant
        );
    }
    let evidence_attributes = BTreeMap::from([
        ("variant".into(), evidence.variant.clone()),
        (
            "output_digest".into(),
            content_digest(&format!(
                "{}\0{}",
                evidence.execution.stdout, evidence.execution.stderr
            )),
        ),
        ("request_count".into(), evidence.request_count.to_string()),
        (
            "elapsed_ms".into(),
            duration_ms(evidence.execution.elapsed).to_string(),
        ),
    ]);
    let evidence_items = vec![EvidenceItem {
        evidence_id: "workflow-output".into(),
        kind: "workflow.outcome".into(),
        digest: canonical_digest(&evidence_attributes)?,
        redacted: true,
        attributes: evidence_attributes,
    }];
    let eval_id = format!("optimize:{run_id}:{}", evidence.variant);
    let subject = EvalSubject {
        schema_version: EVAL_SCHEMA_VERSION,
        eval_id: eval_id.clone(),
        scope: EvalScope::Task,
        subject_id: format!("{run_id}:{}", evidence.variant),
        policy_digest: evidence.policy_digest.clone(),
        preset: None,
        cohort: Some(format!("optimization:{run_id}")),
        holdout: false,
        decisions: selected.iter().map(|item| item.decision.clone()).collect(),
        requested_dimensions: BTreeSet::from([
            "quality.pass".into(),
            "quality.critical_failure".into(),
            "cost.usd_micros".into(),
            "latency.ms".into(),
        ]),
        evidence_digest: evidence_digest(&evidence_items)?,
        evidence: evidence_items,
        observed_at: chrono::Utc::now().to_rfc3339(),
    };
    store.insert_subject(&subject).await?;
    let conclusive = evaluation.verdict != AgenticVerdict::Inconclusive;
    let mut metrics = BTreeMap::from([
        (
            "cost.usd_micros".into(),
            MetricValue::new(
                i64::try_from(evidence.settled_cost_micro_usd)
                    .context("encoding settled workflow cost")?,
                MetricUnit::MicroUsd,
            ),
        ),
        (
            "latency.ms".into(),
            MetricValue::new(
                i64::try_from(evidence.observed_latency_ms)
                    .context("encoding observed workflow latency")?,
                MetricUnit::Milliseconds,
            ),
        ),
    ]);
    if conclusive {
        metrics.insert(
            "quality.pass".into(),
            MetricValue::new(
                i64::from(evaluation.verdict == AgenticVerdict::Pass),
                MetricUnit::Boolean,
            ),
        );
    }
    let hard_violations = evaluation
        .critical_failure
        .then(|| vec!["quality.critical_failure".into()])
        .unwrap_or_default();
    let metric_ids = metrics
        .keys()
        .cloned()
        .chain(hard_violations.iter().cloned())
        .collect::<BTreeSet<_>>();
    let weights = split_weight(selected.len())?;
    let decision_credit = selected
        .iter()
        .zip(weights)
        .map(|(item, weight_ppm)| {
            (
                item.decision.decision_id.clone(),
                DecisionCredit {
                    weight_ppm,
                    metric_ids: metric_ids.clone(),
                },
            )
        })
        .collect();
    let result = EvaluationResult {
        schema_version: EVAL_SCHEMA_VERSION,
        eval_id,
        evidence_digest: subject.evidence_digest.clone(),
        evaluator: EvaluatorIdentity {
            authority_id: "bitrouter.agentic.local".into(),
            evaluator_id: evaluator_lock.agent.clone(),
            kind: EvaluatorKind::Agentic,
            version: evaluator_lock.agent_version.clone(),
            config_digest: canonical_digest(evaluator_lock)?,
        },
        verdict: match evaluation.verdict {
            AgenticVerdict::Pass => EvalVerdict::Pass,
            AgenticVerdict::Fail => EvalVerdict::Fail,
            AgenticVerdict::Inconclusive => EvalVerdict::Inconclusive,
        },
        metrics,
        hard_violations,
        confidence_ppm: Some(match evaluation.confidence {
            AgenticConfidence::High => 900_000,
            AgenticConfidence::Medium => 650_000,
            AgenticConfidence::Low => 350_000,
        }),
        evidence_refs: vec!["workflow-output".into()],
        decision_credit,
        idempotency_key: format!("optimize:{run_id}:{}", evidence.variant),
        submitted_at: chrono::Utc::now().to_rfc3339(),
    };
    let admission = service
        .submit(result, SubmissionPrincipal::LocalOperator)
        .await?;
    if admission.status != AdmissionStatus::Admitted {
        anyhow::bail!(
            "agentic evaluation was not admitted: {} ({})",
            admission.reason,
            evidence.variant
        );
    }
    Ok(())
}

fn split_weight(count: usize) -> Result<Vec<i64>> {
    let count = i64::try_from(count).context("counting credited decisions")?;
    if count == 0 {
        anyhow::bail!("cannot credit zero decisions");
    }
    let base = 1_000_000 / count;
    let remainder = 1_000_000 % count;
    Ok((0..count)
        .map(|index| base + i64::from(index < remainder))
        .collect())
}

fn variant_report(
    evidence: &VariantEvidence,
    evaluation: &AgenticEvaluation,
) -> Result<VariantReport> {
    Ok(VariantReport {
        verdict: match evaluation.verdict {
            AgenticVerdict::Pass => OptimizationVerdict::Pass,
            AgenticVerdict::Fail => OptimizationVerdict::Fail,
            AgenticVerdict::Inconclusive => OptimizationVerdict::Inconclusive,
        },
        confidence: match evaluation.confidence {
            AgenticConfidence::High => "high",
            AgenticConfidence::Medium => "medium",
            AgenticConfidence::Low => "low",
        }
        .into(),
        reason: evaluation.reason.clone(),
        evidence_digest: canonical_digest(&evidence.attributions)?,
        policy_digest: evidence.policy_digest.clone(),
        request_count: evidence.request_count,
        settled_cost_micro_usd: evidence.settled_cost_micro_usd,
        observed_latency_ms: evidence.observed_latency_ms,
        elapsed_ms: duration_ms(evidence.execution.elapsed),
    })
}

fn outcome_summary(report: &VariantReport) -> OutcomeSummary {
    OutcomeSummary {
        verdict: report.verdict,
        evidence_digest: report.evidence_digest.clone(),
        policy_digest: report.policy_digest.clone(),
        settled_cost_micro_usd: Some(report.settled_cost_micro_usd),
        elapsed_ms: report.elapsed_ms,
    }
}

fn duration_ms(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn signed_delta(candidate: u64, baseline: u64) -> i64 {
    i128::from(candidate)
        .saturating_sub(i128::from(baseline))
        .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn percentage_delta_ppm(candidate: u64, baseline: u64) -> Option<i64> {
    if baseline == 0 {
        return None;
    }
    let delta = i128::from(candidate).saturating_sub(i128::from(baseline));
    i64::try_from(delta.saturating_mul(1_000_000) / i128::from(baseline)).ok()
}

fn writable_database_url(url: &str, config_path: &Path) -> Result<String> {
    let Some(after_scheme) = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))
    else {
        return Ok(url.into());
    };
    let (path_part, query) = after_scheme
        .split_once('?')
        .map_or((after_scheme, None), |(path, query)| (path, Some(query)));
    if path_part.is_empty() || path_part == ":memory:" {
        anyhow::bail!("workflow optimization requires a persistent source database");
    }
    let path = Path::new(path_part.strip_prefix("./").unwrap_or(path_part));
    let home = config_path.parent().unwrap_or_else(|| Path::new("."));
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        home.join(path)
    };
    let mut params = query.unwrap_or_default().to_string();
    if !params.split('&').any(|part| part.starts_with("mode=")) {
        if !params.is_empty() {
            params.push('&');
        }
        params.push_str("mode=rwc");
    }
    Ok(format!("sqlite://{}?{params}", absolute.display()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::time::Duration;

    use bitrouter_sdk::config::{AdequacyConfig, EvalConfig};

    use super::{percentage_delta_ppm, split_weight, submit_variant_result, writable_database_url};
    use crate::eval::types::{AdmissionStatus, EvalDecisionRef};
    use crate::optimization::evaluator::{
        AgenticConfidence, AgenticEvaluation, AgenticVerdict, embedded_evaluator_digest,
    };
    use crate::optimization::runner::{
        RouteObservation, VariantAttribution, VariantEvidence, WorkflowExecution,
    };
    use crate::optimization::{EvaluatorLock, EvaluatorRoute};
    use crate::policy_compile::{CompileInput, LegacyAdequacySnapshot, PromotionQualityCriteria};
    use crate::policy_lock::{PolicyDefinition, PolicyLock, PromotionVerdict, semantic_digest};

    #[test]
    fn decision_credit_is_exactly_one_and_deterministic() -> anyhow::Result<()> {
        let weights = split_weight(3)?;
        assert_eq!(weights, vec![333_334, 333_333, 333_333]);
        assert_eq!(weights.iter().sum::<i64>(), 1_000_000);
        assert!(split_weight(0).is_err());
        Ok(())
    }

    #[test]
    fn cost_delta_is_signed_and_zero_baseline_is_explicit() {
        assert_eq!(percentage_delta_ppm(75, 100), Some(-250_000));
        assert_eq!(percentage_delta_ppm(125, 100), Some(250_000));
        assert_eq!(percentage_delta_ppm(0, 0), None);
    }

    #[test]
    fn source_database_is_anchored_next_to_the_config() -> anyhow::Result<()> {
        assert_eq!(
            writable_database_url(
                "sqlite://./state.db",
                Path::new("/tmp/project/bitrouter.yaml")
            )?,
            "sqlite:///tmp/project/state.db?mode=rwc"
        );
        Ok(())
    }

    fn active_policy() -> PolicyLock {
        let mut lock = PolicyLock::default();
        lock.lockfile_version = 1;
        lock.artifact = None;
        lock.policies.insert(
            "auto".into(),
            PolicyDefinition {
                tiers: BTreeMap::from([
                    ("strong".into(), "bitrouter:openai/gpt-5.6".into()),
                    (
                        "economy".into(),
                        "bitrouter:deepseek/deepseek-v4-flash-0731".into(),
                    ),
                ]),
                default_tier: Some("strong".into()),
                tool_use_tier: Some("strong".into()),
                tool_safe_tiers: vec!["strong".into(), "economy".into()],
                adequacy: AdequacyConfig {
                    explore_tier: Some("economy".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        lock
    }

    fn variant(policy_digest: &str, name: &str, tier: &str) -> VariantEvidence {
        let decision = EvalDecisionRef {
            decision_id: format!("req-{name}:auto"),
            policy: "auto".into(),
            request_key: "agent_trace/v2|edit|normal".into(),
            selected_tier: tier.into(),
            baseline_tier: Some("strong".into()),
            policy_digest: policy_digest.into(),
        };
        VariantEvidence {
            variant: name.into(),
            policy_digest: policy_digest.into(),
            execution: WorkflowExecution {
                exit_code: Some(0),
                timed_out: false,
                elapsed: Duration::from_secs(1),
                stdout: "workflow passed".into(),
                stderr: String::new(),
                launches: 1,
                cwd: "/tmp/project".into(),
            },
            request_count: 1,
            settled_cost_micro_usd: if name == "baseline" { 100 } else { 60 },
            observed_latency_ms: 200,
            observations: vec![RouteObservation {
                request_key: decision.request_key.clone(),
                selected_tier: tier.into(),
                settled_cost_micro_usd: Some(60),
            }],
            attributions: vec![VariantAttribution {
                request_id: format!("req-{name}"),
                decision,
                settled_cost_micro_usd: 60,
                latency_ms: 200,
            }],
        }
    }

    #[tokio::test]
    async fn admitted_agentic_pair_compiles_the_one_key_candidate() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = crate::eval::store::EvalStore::new(db.clone());
        let service = crate::eval::EvalService::new(store.clone(), EvalConfig::default());
        let active = active_policy();
        let active_digest = semantic_digest(&active)?;
        let experiment = crate::optimization::runner::build_experiment_lock(
            &active,
            &active_digest,
            "auto",
            "agent_trace/v2|edit|normal",
            "economy",
        )?;
        let evaluator = EvaluatorLock {
            agent: "codex-acp".into(),
            agent_version: "1.0.0".into(),
            model: "bitrouter:openai/gpt-5.6".into(),
            route: EvaluatorRoute::Cloud,
            skill_digest: embedded_evaluator_digest()?,
            contract_digest:
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        };
        let opinion = AgenticEvaluation {
            verdict: AgenticVerdict::Pass,
            confidence: AgenticConfidence::High,
            critical_failure: false,
            evidence_refs: vec!["workflow-output".into()],
            reason: "The workflow contract passed.".into(),
        };
        submit_variant_result(
            &service,
            &store,
            "run-1",
            "agent_trace/v2|edit|normal",
            &variant(&active_digest, "baseline", "strong"),
            &opinion,
            &evaluator,
        )
        .await?;
        submit_variant_result(
            &service,
            &store,
            "run-1",
            "agent_trace/v2|edit|normal",
            &variant(&semantic_digest(&experiment)?, "candidate", "economy"),
            &opinion,
            &evaluator,
        )
        .await?;
        assert!(
            store
                .latest_admissions()
                .await?
                .values()
                .all(|event| event.status == AdmissionStatus::Admitted)
        );
        let snapshot = store.freeze_snapshot("2026-08-08T00:00:00Z").await?;
        let eval =
            crate::eval::compiler::EvalEvidenceSnapshot::load(&store, &snapshot.evidence_root)
                .await?;
        let legacy = LegacyAdequacySnapshot {
            snapshot_time_unix_ms: 1_786_147_200_000,
            pins: Vec::new(),
            exploration: Vec::new(),
            semantic_successes: Vec::new(),
            reliability_events: Vec::new(),
        };
        let compiled = crate::policy_compile::compile_candidate_with_quality(
            CompileInput {
                current: &active,
                parent_digest: Some(&active_digest),
                legacy: &legacy,
                eval: Some(&eval),
                proposed_progress_guards: None,
            },
            &PromotionQualityCriteria::manual_review(),
        )?;
        assert_eq!(
            compiled.document.policies["auto"].routes["agent_trace/v2|edit|normal"],
            "economy"
        );
        assert!(compiled.changes.iter().any(|change| {
            change.request_key == "agent_trace/v2|edit|normal"
                && change.verdict == PromotionVerdict::Promote
        }));
        Ok(())
    }
}
