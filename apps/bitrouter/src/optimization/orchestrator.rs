use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use tokio::io::AsyncReadExt;

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
use crate::policy_lock::{
    CertificateSource, POLICY_COMPILER_ID, POLICY_COMPILER_VERSION, PromotionVerdict, RouteOwner,
    semantic_digest,
};

const MAXIMUM_WORKFLOW_OUTPUT_BYTES: usize = 48 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowFingerprint {
    pub argv_digest: String,
    pub referenced_files: BTreeMap<String, String>,
    pub workspace_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptimizationReport {
    pub run_id: String,
    pub created_at: String,
    pub source_policy_digest: String,
    pub source_config_digest: String,
    pub workflow_fingerprint: WorkflowFingerprint,
    pub target_request_key: String,
    pub preference: super::OptimizationPreference,
    pub baseline: VariantReport,
    pub candidate: VariantReport,
    pub normalized_cost_delta_micro_usd: i64,
    pub normalized_cost_delta_ppm: Option<i64>,
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
    pub normalized_cost_micro_usd: u64,
    pub observed_latency_ms: u64,
    pub elapsed_ms: u64,
}

pub struct RunOptimizationRequest<'a> {
    pub loaded: &'a LoadedIntent,
    pub optimization_lock: &'a super::LoadedOptimizationLock,
    pub workflow_cwd: &'a Path,
    pub bitrouter_executable: &'a Path,
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
        anyhow::bail!("optimization intent changed; run `bitrouter optimize resolve`");
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
    let source_config_digest = content_digest(&source_raw);
    let active = crate::policy_lock::load_for_config(
        &source_config,
        Some(&request.loaded.paths.source_config),
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("source config has no active policy lock"))?;
    if active.digest != request.optimization_lock.document.active_policy_digest {
        anyhow::bail!(
            "active policy changed; run `bitrouter optimize rollback <digest>` or `bitrouter optimize resolve`"
        );
    }
    super::validate_policy_contract(&request.loaded.intent, &source_config, &active.document)?;
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
    if let Some(project_root) = request.loaded.paths.private_runs.parent() {
        super::secure_private_directory(project_root).await?;
    }
    super::secure_private_directory(&request.loaded.paths.private_runs).await?;
    super::secure_private_directory(&run_root).await?;
    let workflow_fingerprint =
        fingerprint_workflow(&request.loaded.intent.workflow, request.workflow_cwd).await?;
    let mut workflow_workspaces = FrozenWorkflowWorkspaces::prepare(
        request.workflow_cwd,
        &run_root.join("workflow-workspaces"),
        &request.loaded.intent.workflow.command,
        &request.loaded.intent.workflow.inputs,
    )
    .await?;

    let baseline = run_private_variant(PrivateVariantRequest {
        variant: "baseline",
        paths: &PrivateDaemonPaths::new(run_root.join("baseline")),
        intent: &request.loaded.intent,
        policy: &active.document,
        policy_digest: &active.digest,
        source_config_raw: &source_raw,
        workflow_cwd: &workflow_workspaces.baseline_cwd,
        bitrouter_executable: request.bitrouter_executable,
        maximum_output_bytes: MAXIMUM_WORKFLOW_OUTPUT_BYTES,
    })
    .await
    .context("running controlled baseline")?;
    verify_workflow_unchanged(
        &workflow_fingerprint,
        &request.loaded.intent.workflow,
        request.workflow_cwd,
        "baseline",
    )
    .await?;
    verify_source_config_unchanged(
        &request.loaded.paths.source_config,
        &source_config_digest,
        "baseline",
    )
    .await?;
    verify_active_policy_unchanged(
        &source_config,
        &request.loaded.paths.source_config,
        &active.digest,
        "baseline",
    )
    .await?;
    let target_request_key = select_target_request_key(
        &active.document,
        &request.loaded.intent.policy,
        "strong",
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
        source_config_raw: &source_raw,
        workflow_cwd: &workflow_workspaces.candidate_cwd,
        bitrouter_executable: request.bitrouter_executable,
        maximum_output_bytes: MAXIMUM_WORKFLOW_OUTPUT_BYTES,
    })
    .await
    .context("running one-key routing candidate")?;
    verify_workflow_unchanged(
        &workflow_fingerprint,
        &request.loaded.intent.workflow,
        request.workflow_cwd,
        "candidate",
    )
    .await?;
    verify_source_config_unchanged(
        &request.loaded.paths.source_config,
        &source_config_digest,
        "candidate",
    )
    .await?;
    verify_active_policy_unchanged(
        &source_config,
        &request.loaded.paths.source_config,
        &active.digest,
        "candidate",
    )
    .await?;
    verify_controlled_candidate(&candidate, &target_request_key)?;
    workflow_workspaces
        .cleanup()
        .context("removing isolated workflow workspaces")?;

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
    verify_source_config_unchanged(
        &request.loaded.paths.source_config,
        &source_config_digest,
        "evaluation",
    )
    .await?;
    verify_active_policy_unchanged(
        &source_config,
        &request.loaded.paths.source_config,
        &active.digest,
        "evaluation",
    )
    .await?;

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
    let mut run_result_ids = submit_variant_result(
        &eval_service,
        &eval_store,
        &run_id,
        &target_request_key,
        &baseline,
        &baseline_evaluation,
        &request.optimization_lock.document.evaluator,
    )
    .await?;
    run_result_ids.extend(
        submit_variant_result(
            &eval_service,
            &eval_store,
            &run_id,
            &target_request_key,
            &candidate,
            &candidate_evaluation,
            &request.optimization_lock.document.evaluator,
        )
        .await?,
    );
    let frozen_at = chrono::Utc::now().to_rfc3339();
    let snapshot = eval_store
        .freeze_snapshot_for_result_ids(&frozen_at, "local", &run_result_ids)
        .await?;
    let eval_snapshot =
        crate::eval::compiler::EvalEvidenceSnapshot::load(&eval_store, &snapshot.evidence_root)
            .await?;
    let snapshot_time = chrono::DateTime::parse_from_rfc3339(&frozen_at)
        .context("parsing frozen Eval timestamp")?
        .timestamp_millis();
    let legacy = LegacyAdequacySnapshot {
        snapshot_time_unix_ms: snapshot_time,
        pins: Vec::new(),
        exploration: Vec::new(),
        semantic_successes: Vec::new(),
        reliability_events: Vec::new(),
    };
    let mut compiled = crate::policy_compile::compile_candidate_with_quality(
        CompileInput {
            current: &active.document,
            parent_digest: Some(&active.digest),
            legacy: &legacy,
            eval: Some(&eval_snapshot),
            proposed_progress_guards: None,
        },
        &request.loaded.intent.promotion_quality_criteria()?,
    )?;
    if let Some(migration) = compiled
        .document
        .artifact
        .as_mut()
        .and_then(|artifact| artifact.migration.as_mut())
    {
        migration.source = crate::policy_lock::LegacyEvidenceSource::SealedEmpty;
    }
    let candidate_digest = semantic_digest(&compiled.document)?;
    let candidate_path = run_root.join("candidate-policy-lock.yaml");
    tokio::fs::write(
        &candidate_path,
        crate::policy_lock::deterministic_yaml(&compiled.document)?,
    )
    .await
    .with_context(|| format!("writing compiled candidate {}", candidate_path.display()))?;
    super::secure_private_file(&candidate_path).await?;
    let exact_change = compiled.changes.as_slice()
        == [crate::policy_compile::CompileChange {
            policy: request.loaded.intent.policy.clone(),
            request_key: target_request_key.clone(),
            previous_tier: active.document.policies[&request.loaded.intent.policy]
                .routes
                .get(&target_request_key)
                .cloned(),
            selected_tier: "economy".into(),
            verdict: PromotionVerdict::Promote,
        }];
    let mut expected_policies = active.document.policies.clone();
    expected_policies
        .get_mut(&request.loaded.intent.policy)
        .ok_or_else(|| anyhow::anyhow!("optimization policy disappeared"))?
        .routes
        .insert(target_request_key.clone(), "economy".into());
    let exact_policy_semantics = compiled.document.policies == expected_policies;
    let target_certificate = compiled
        .document
        .certificate(&request.loaded.intent.policy, &target_request_key);
    let exact_target_certificate = target_certificate.is_some_and(|certificate| {
        certificate.owner == RouteOwner::Compiler
            && certificate.selected_tier == "economy"
            && certificate.baseline_tier.as_deref() == Some("strong")
            && matches!(
                certificate.source,
                CertificateSource::Agentic | CertificateSource::Mixed
            )
            && certificate.verdict == PromotionVerdict::Promote
            && certificate.critical_violations == 0
            && certificate.legacy.is_none()
            && certificate
                .economics
                .as_ref()
                .is_some_and(|economics| economics.normalized_cost_delta_ppm < 0)
            && certificate.evaluator_config_digest.is_some()
            && !certificate.evidence_digest.is_empty()
    });
    let exact_certificates = if let Some(target_certificate) = target_certificate {
        let mut expected = active.document.certificates.clone();
        expected
            .entry(request.loaded.intent.policy.clone())
            .or_default()
            .insert(target_request_key.clone(), target_certificate.clone());
        compiled.document.certificates == expected
    } else {
        false
    };
    let exact_artifact = compiled.document.artifact.as_ref().is_some_and(|artifact| {
        artifact.parent_digest.as_deref() == Some(active.digest.as_str())
            && artifact.eval_snapshot_root.as_deref() == Some(snapshot.evidence_root.as_str())
            && artifact.compiler.id == POLICY_COMPILER_ID
            && artifact.compiler.version == POLICY_COMPILER_VERSION
            && artifact.migration.as_ref().is_some_and(|migration| {
                migration.source == crate::policy_lock::LegacyEvidenceSource::SealedEmpty
            })
    });
    let cost_improved = candidate.normalized_cost_micro_usd < baseline.normalized_cost_micro_usd;
    let publishable = !baseline.execution.timed_out
        && !candidate.execution.timed_out
        && candidate_evaluation.verdict == AgenticVerdict::Pass
        && !candidate_evaluation.critical_failure
        && compiled.conflicts.is_empty()
        && exact_change
        && exact_policy_semantics
        && exact_target_certificate
        && exact_certificates
        && exact_artifact
        && cost_improved;
    let mut caveats = Vec::new();
    if !publishable {
        caveats
            .push("candidate did not satisfy the pinned quality gate or compiler authority".into());
    }
    if baseline.execution.timed_out {
        caveats.push("baseline workflow timed out".into());
    }
    if candidate.execution.timed_out {
        caveats.push("candidate workflow timed out".into());
    }
    if !cost_improved {
        caveats.push("candidate did not reduce normalized showback cost".into());
    }
    if !exact_change {
        caveats.push("compiler output was not an exact one-route change".into());
    }
    if !exact_policy_semantics {
        caveats.push("compiled policy semantics changed outside the controlled route".into());
    }
    if !exact_target_certificate || !exact_certificates || !exact_artifact {
        caveats.push(
            "compiled provenance changed outside the controlled route or its pinned evidence"
                .into(),
        );
    }
    let normalized_cost_delta_micro_usd = signed_delta(
        candidate.normalized_cost_micro_usd,
        baseline.normalized_cost_micro_usd,
    );
    let report = OptimizationReport {
        run_id: run_id.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        source_policy_digest: active.digest.clone(),
        source_config_digest: source_config_digest.clone(),
        workflow_fingerprint,
        target_request_key: target_request_key.clone(),
        preference: request.loaded.intent.preference,
        baseline: variant_report(&baseline, &baseline_evaluation)?,
        candidate: variant_report(&candidate, &candidate_evaluation)?,
        normalized_cost_delta_micro_usd,
        normalized_cost_delta_ppm: percentage_delta_ppm(
            candidate.normalized_cost_micro_usd,
            baseline.normalized_cost_micro_usd,
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
    let report_path = run_root.join("report.json");
    tokio::fs::write(&report_path, &report_bytes)
        .await
        .context("writing private optimization report")?;
    super::secure_private_file(&report_path).await?;
    let mut updated_lock = request.optimization_lock.document.clone();
    updated_lock.latest_run = Some(OptimizationRunLock {
        run_id,
        source_policy_digest: active.digest.clone(),
        source_config_digest,
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

async fn verify_workflow_unchanged(
    expected: &WorkflowFingerprint,
    workflow: &super::WorkflowCommand,
    cwd: &Path,
    variant: &str,
) -> Result<()> {
    let actual = fingerprint_workflow(workflow, cwd).await?;
    if &actual != expected {
        anyhow::bail!(
            "workflow argv or referenced file inputs changed during the {variant} experiment"
        );
    }
    Ok(())
}

async fn verify_source_config_unchanged(
    path: &Path,
    expected_digest: &str,
    phase: &str,
) -> Result<()> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading source config {} after {phase}", path.display()))?;
    let actual = content_digest(&raw);
    if actual != expected_digest {
        anyhow::bail!(
            "source BitRouter config changed during {phase}; refusing mixed-lineage evidence"
        );
    }
    Ok(())
}

async fn verify_active_policy_unchanged(
    source_config: &bitrouter_sdk::config::Config,
    config_path: &Path,
    expected_digest: &str,
    phase: &str,
) -> Result<()> {
    let active = crate::policy_lock::load_for_config(source_config, Some(config_path))
        .await?
        .ok_or_else(|| anyhow::anyhow!("source config lost its active policy during {phase}"))?;
    if active.digest != expected_digest {
        anyhow::bail!("active policy changed during {phase}; refusing mixed-lineage evidence");
    }
    Ok(())
}

async fn fingerprint_workflow(
    workflow: &super::WorkflowCommand,
    cwd: &Path,
) -> Result<WorkflowFingerprint> {
    let command = &workflow.command;
    let argv_digest = canonical_digest(&command.to_vec())?;
    let mut candidates = command
        .iter()
        .enumerate()
        .filter_map(|(index, argument)| {
            let path = PathBuf::from(argument);
            let resolved = if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            };
            resolved
                .is_file()
                .then(|| (format!("argv[{index}]"), resolved))
        })
        .collect::<Vec<_>>();
    if !command.is_empty()
        && !Path::new(&command[0]).is_absolute()
        && !command[0].contains(std::path::MAIN_SEPARATOR)
        && let Some(path) = executable_in_path(&command[0])
    {
        candidates.push(("executable".into(), path));
    }
    let mut referenced_files = BTreeMap::new();
    for (label, path) in candidates {
        referenced_files.insert(label, digest_file(&path).await?);
    }
    for (index, input) in workflow.inputs.iter().enumerate() {
        let path = if input.is_absolute() {
            input.clone()
        } else {
            cwd.join(input)
        };
        fingerprint_declared_input(&format!("input[{index}]"), &path, &mut referenced_files)
            .await?;
    }
    Ok(WorkflowFingerprint {
        argv_digest,
        referenced_files,
        workspace_digest: git_workspace_digest(cwd).await?,
    })
}

async fn fingerprint_declared_input(
    label: &str,
    root: &Path,
    digests: &mut BTreeMap<String, String>,
) -> Result<()> {
    let mut pending = vec![(label.to_string(), root.to_path_buf())];
    let mut entries = 0_usize;
    while let Some((entry_label, path)) = pending.pop() {
        entries = entries.saturating_add(1);
        if entries > 100_000 {
            anyhow::bail!("declared workflow input exceeds the 100000-entry safety limit");
        }
        let metadata = tokio::fs::symlink_metadata(&path)
            .await
            .with_context(|| format!("reading declared workflow input {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            let target = tokio::fs::read_link(&path)
                .await
                .with_context(|| format!("reading workflow input symlink {}", path.display()))?;
            digests.insert(entry_label, content_digest(&target.to_string_lossy()));
        } else if metadata.is_file() {
            digests.insert(entry_label, digest_file(&path).await?);
        } else if metadata.is_dir() {
            digests.insert(entry_label.clone(), content_digest("directory"));
            let mut children = std::fs::read_dir(&path)
                .with_context(|| format!("listing declared workflow input {}", path.display()))?
                .collect::<std::io::Result<Vec<_>>>()?;
            children.sort_by_key(std::fs::DirEntry::file_name);
            for child in children.into_iter().rev() {
                let name = child.file_name().to_string_lossy().to_string();
                pending.push((format!("{entry_label}/{name}"), child.path()));
            }
        } else {
            anyhow::bail!(
                "declared workflow input is not a regular file, directory, or symlink: {}",
                path.display()
            );
        }
    }
    Ok(())
}

async fn git_workspace_digest(cwd: &Path) -> Result<String> {
    let root = git_output(cwd, &["rev-parse", "--show-toplevel"])
        .await
        .context("workflow optimization requires a Git worktree so baseline and candidate inputs can be compared exactly")?;
    let root = String::from_utf8(root).context("decoding Git worktree root")?;
    let root = PathBuf::from(root.trim());
    if root.as_os_str().is_empty() || !root.is_dir() {
        anyhow::bail!("Git returned an invalid workflow worktree root");
    }
    let head = git_output(&root, &["rev-parse", "HEAD"]).await?;
    let diff = git_output(&root, &["diff", "--binary", "--no-ext-diff", "HEAD", "--"]).await?;
    let submodules = git_output(&root, &["submodule", "status", "--recursive"]).await?;
    let untracked =
        git_output(&root, &["ls-files", "-z", "--others", "--exclude-standard"]).await?;
    let mut digest = sha2::Sha256::new();
    for (label, bytes) in [
        (b"head".as_slice(), head.as_slice()),
        (b"diff".as_slice(), diff.as_slice()),
        (b"submodules".as_slice(), submodules.as_slice()),
        (b"untracked".as_slice(), untracked.as_slice()),
    ] {
        digest.update(label);
        digest.update([0]);
        digest.update(bytes);
        digest.update([0]);
    }
    for relative in untracked
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let relative = std::str::from_utf8(relative).context("decoding untracked Git path")?;
        let path = root.join(relative);
        if path.is_file() {
            digest.update(relative.as_bytes());
            digest.update([0]);
            digest.update(digest_file(&path).await?.as_bytes());
            digest.update([0]);
        }
    }
    Ok(format!("sha256:{}", hex::encode(digest.finalize())))
}

async fn git_output(cwd: &Path, arguments: &[&str]) -> Result<Vec<u8>> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .with_context(|| format!("running git {}", arguments.join(" ")))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {} failed: {}", arguments.join(" "), detail.trim());
    }
    Ok(output.stdout)
}

struct FrozenWorkflowWorkspaces {
    root: PathBuf,
    git_root: PathBuf,
    baseline_root: PathBuf,
    candidate_root: PathBuf,
    baseline_cwd: PathBuf,
    candidate_cwd: PathBuf,
    baseline_registered: bool,
    candidate_registered: bool,
    cleaned: bool,
}

impl FrozenWorkflowWorkspaces {
    async fn prepare(
        source_cwd: &Path,
        root: &Path,
        command: &[String],
        declared_inputs: &[PathBuf],
    ) -> Result<Self> {
        let source_cwd = canonicalize_workflow_path(source_cwd)
            .with_context(|| format!("canonicalizing workflow cwd {}", source_cwd.display()))?;
        let git_root = git_output(&source_cwd, &["rev-parse", "--show-toplevel"])
            .await
            .context("resolving workflow Git root")?;
        let git_root = PathBuf::from(
            String::from_utf8(git_root)
                .context("decoding workflow Git root")?
                .trim(),
        );
        let git_root = canonicalize_workflow_path(&git_root)
            .with_context(|| format!("canonicalizing Git root {}", git_root.display()))?;
        let relative_cwd = source_cwd.strip_prefix(&git_root).with_context(|| {
            format!(
                "workflow cwd {} is outside Git root {}",
                source_cwd.display(),
                git_root.display()
            )
        })?;
        let listed = git_output(
            &git_root,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
        )
        .await?;
        let mut files = listed
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| {
                std::str::from_utf8(path)
                    .map(PathBuf::from)
                    .context("decoding Git workflow input path")
            })
            .collect::<Result<BTreeSet<_>>>()?;
        for argument in command {
            let path = PathBuf::from(argument);
            let source = if path.is_absolute() {
                path.clone()
            } else {
                source_cwd.join(&path)
            };
            if source.is_file() {
                let canonical = canonicalize_workflow_path(&source).with_context(|| {
                    format!("canonicalizing workflow input {}", source.display())
                })?;
                if canonical.starts_with(&git_root) {
                    if path.is_absolute() {
                        anyhow::bail!(
                            "workflow arguments that reference project files must be relative so isolated variants cannot escape to the mutable source workspace: {}",
                            path.display()
                        );
                    }
                    files.insert(
                        canonical
                            .strip_prefix(&git_root)
                            .context("project workflow input escaped Git root")?
                            .to_path_buf(),
                    );
                }
            }
        }
        for input in declared_inputs {
            collect_declared_input(&git_root, &source_cwd, input, &mut files)?;
        }
        if files.len() > 100_000 {
            anyhow::bail!("workflow snapshot exceeds the 100000-file safety limit");
        }
        super::secure_private_directory(root).await?;
        let staging_root = root.join("staging");
        let baseline_root = root.join("baseline");
        let candidate_root = root.join("candidate");
        let baseline_cwd = baseline_root.join(relative_cwd);
        let candidate_cwd = candidate_root.join(relative_cwd);
        let mut frozen = Self {
            root: root.to_path_buf(),
            git_root: git_root.clone(),
            baseline_root,
            candidate_root,
            baseline_cwd,
            candidate_cwd,
            baseline_registered: false,
            candidate_registered: false,
            cleaned: false,
        };
        super::secure_private_directory(&staging_root).await?;
        for (index, destination) in [frozen.baseline_root.clone(), frozen.candidate_root.clone()]
            .into_iter()
            .enumerate()
        {
            let destination_text = destination.to_string_lossy().to_string();
            git_output(
                &git_root,
                &["worktree", "add", "--detach", &destination_text, "HEAD"],
            )
            .await
            .with_context(|| format!("creating frozen Git worktree {}", destination.display()))?;
            if index == 0 {
                frozen.baseline_registered = true;
            } else {
                frozen.candidate_registered = true;
            }
        }
        let mut total_bytes = 0_u64;
        for relative in files {
            let source = git_root.join(&relative);
            let metadata = match tokio::fs::symlink_metadata(&source).await {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error).context(format!("reading {}", source.display())),
            };
            if metadata.is_dir() {
                anyhow::bail!(
                    "workflow snapshots require files; initialize submodules or declare a materialized directory instead: {}",
                    source.display()
                );
            }
            if metadata.file_type().is_symlink() {
                let target = tokio::fs::read_link(&source)
                    .await
                    .with_context(|| format!("reading snapshot symlink {}", source.display()))?;
                if target.is_absolute() {
                    anyhow::bail!(
                        "workflow snapshot rejects absolute symlinks that could share mutable state: {}",
                        source.display()
                    );
                }
                let resolved = canonicalize_workflow_path(
                    &source
                        .parent()
                        .ok_or_else(|| anyhow::anyhow!("workflow symlink has no parent"))?
                        .join(&target),
                )
                .with_context(|| format!("resolving snapshot symlink {}", source.display()))?;
                if !resolved.starts_with(&git_root) {
                    anyhow::bail!(
                        "workflow snapshot rejects symlinks outside the Git worktree: {}",
                        source.display()
                    );
                }
            }
            total_bytes = total_bytes.saturating_add(metadata.len());
            if total_bytes > 2 * 1024 * 1024 * 1024 {
                anyhow::bail!("workflow snapshot exceeds the 2 GiB safety limit");
            }
            let staged = staging_root.join(&relative);
            copy_snapshot_entry(&source, &staged, &metadata).await?;
            for destination_root in [&frozen.baseline_root, &frozen.candidate_root] {
                let destination = destination_root.join(&relative);
                remove_snapshot_entry(&destination).await?;
                copy_snapshot_entry(&staged, &destination, &metadata).await?;
            }
        }
        let deleted = git_output(&git_root, &["ls-files", "-z", "--deleted"]).await?;
        for relative in deleted
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            let relative =
                PathBuf::from(std::str::from_utf8(relative).context("decoding deleted Git path")?);
            for destination_root in [&frozen.baseline_root, &frozen.candidate_root] {
                remove_snapshot_entry(&destination_root.join(&relative)).await?;
            }
        }
        super::secure_private_directory(&frozen.baseline_cwd).await?;
        super::secure_private_directory(&frozen.candidate_cwd).await?;
        Ok(frozen)
    }

    fn cleanup(&mut self) -> Result<()> {
        if self.cleaned {
            return Ok(());
        }
        for (worktree, registered) in [
            (&self.baseline_root, self.baseline_registered),
            (&self.candidate_root, self.candidate_registered),
        ] {
            if registered {
                let worktree_text = worktree.to_string_lossy().to_string();
                let status = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&self.git_root)
                    .args(["worktree", "remove", "--force", &worktree_text])
                    .status()
                    .with_context(|| format!("removing Git worktree {}", worktree.display()))?;
                if !status.success() {
                    anyhow::bail!("Git refused to remove worktree {}", worktree.display());
                }
            }
        }
        if self.root.exists() {
            std::fs::remove_dir_all(&self.root).with_context(|| {
                format!("removing frozen workflow root {}", self.root.display())
            })?;
        }
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for FrozenWorkflowWorkspaces {
    fn drop(&mut self) {
        if !self.cleaned {
            for (worktree, registered) in [
                (&self.baseline_root, self.baseline_registered),
                (&self.candidate_root, self.candidate_registered),
            ] {
                if registered {
                    let _ = std::process::Command::new("git")
                        .arg("-C")
                        .arg(&self.git_root)
                        .args([
                            "worktree",
                            "remove",
                            "--force",
                            worktree.to_string_lossy().as_ref(),
                        ])
                        .status();
                }
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

fn collect_declared_input(
    git_root: &Path,
    source_cwd: &Path,
    input: &Path,
    files: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let source = if input.is_absolute() {
        input.to_path_buf()
    } else {
        source_cwd.join(input)
    };
    let canonical = canonicalize_workflow_path(&source)
        .with_context(|| format!("resolving declared workflow input {}", source.display()))?;
    if !canonical.starts_with(git_root) {
        anyhow::bail!(
            "declared workflow inputs must be inside the Git worktree: {}",
            source.display()
        );
    }
    let metadata = std::fs::symlink_metadata(&canonical)?;
    if metadata.is_dir() {
        for entry in std::fs::read_dir(&canonical)? {
            let entry = entry?;
            collect_declared_input(git_root, source_cwd, &entry.path(), files)?;
        }
    } else {
        files.insert(canonical.strip_prefix(git_root)?.to_path_buf());
    }
    Ok(())
}

fn canonicalize_workflow_path(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path).map(normalize_canonical_path)
}

#[cfg(not(windows))]
fn normalize_canonical_path(path: PathBuf) -> PathBuf {
    path
}

#[cfg(windows)]
fn normalize_canonical_path(path: PathBuf) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path
    }
}

async fn remove_snapshot_entry(path: &Path) -> Result<()> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.is_dir() => tokio::fs::remove_dir_all(path).await?,
        Ok(_) => tokio::fs::remove_file(path).await?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context(format!("reading {}", path.display())),
    }
    Ok(())
}

async fn copy_snapshot_entry(
    source: &Path,
    destination: &Path,
    metadata: &std::fs::Metadata,
) -> Result<()> {
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        {
            let target = tokio::fs::read_link(source).await?;
            std::os::unix::fs::symlink(target, destination)?;
            return Ok(());
        }
        #[cfg(not(unix))]
        anyhow::bail!("workflow snapshots with symlinks are unsupported on this platform");
    }
    tokio::fs::copy(source, destination)
        .await
        .with_context(|| {
            format!(
                "copying frozen workflow input {} to {}",
                source.display(),
                destination.display()
            )
        })?;
    tokio::fs::set_permissions(destination, metadata.permissions()).await?;
    Ok(())
}

fn executable_in_path(command: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(command))
        .find(|path| path.is_file())
}

async fn digest_file(path: &Path) -> Result<String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("opening workflow input {}", path.display()))?;
    let mut digest = sha2::Sha256::new();
    let mut buffer = [0_u8; 32 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .with_context(|| format!("hashing workflow input {}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("sha256:{}", hex::encode(digest.finalize())))
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
    let selected = candidate
        .attributions
        .iter()
        .filter(|item| item.decision.request_key == target)
        .collect::<Vec<_>>();
    if selected.is_empty()
        || selected
            .iter()
            .any(|item| item.decision.selected_tier != "economy")
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
) -> Result<Vec<String>> {
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
    let evidence_items = vec![
        EvidenceItem {
            evidence_id: "workflow-output".into(),
            kind: "workflow.outcome".into(),
            digest: canonical_digest(&evidence_attributes)?,
            redacted: true,
            attributes: evidence_attributes,
        },
        EvidenceItem {
            evidence_id: "normalized-showback".into(),
            kind: "request.normalized_showback".into(),
            digest: canonical_digest(&evidence.attributions)?,
            redacted: true,
            attributes: BTreeMap::from([(
                "request_count".into(),
                evidence.attributions.len().to_string(),
            )]),
        },
    ];
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
    let operational_metrics = BTreeMap::from([
        (
            "cost.usd_micros".into(),
            MetricValue::new(
                i64::try_from(evidence.normalized_cost_micro_usd)
                    .context("encoding normalized workflow cost")?,
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
    let mut quality_metrics = BTreeMap::new();
    if conclusive {
        quality_metrics.insert(
            "quality.pass".into(),
            MetricValue::new(
                i64::from(evaluation.verdict == AgenticVerdict::Pass),
                MetricUnit::Boolean,
            ),
        );
    }
    let hard_violations = if evaluation.critical_failure {
        vec!["quality.critical_failure".into()]
    } else {
        Vec::new()
    };
    let quality_metric_ids = quality_metrics
        .keys()
        .cloned()
        .chain(hard_violations.iter().cloned())
        .collect::<BTreeSet<_>>();
    let weights = split_weight(selected.len())?;
    let quality_credit = selected
        .iter()
        .zip(weights.iter().copied())
        .map(|(item, weight_ppm)| {
            (
                item.decision.decision_id.clone(),
                DecisionCredit {
                    weight_ppm,
                    metric_ids: quality_metric_ids.clone(),
                },
            )
        })
        .collect();
    let operational_metric_ids = operational_metrics.keys().cloned().collect::<BTreeSet<_>>();
    let operational_credit = selected
        .iter()
        .zip(weights)
        .map(|(item, weight_ppm)| {
            (
                item.decision.decision_id.clone(),
                DecisionCredit {
                    weight_ppm,
                    metric_ids: operational_metric_ids.clone(),
                },
            )
        })
        .collect();
    let agentic_result = EvaluationResult {
        schema_version: EVAL_SCHEMA_VERSION,
        eval_id: eval_id.clone(),
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
        metrics: quality_metrics,
        hard_violations,
        confidence_ppm: Some(match evaluation.confidence {
            AgenticConfidence::High => 900_000,
            AgenticConfidence::Medium => 650_000,
            AgenticConfidence::Low => 350_000,
        }),
        evidence_refs: evaluation.evidence_refs.clone(),
        decision_credit: quality_credit,
        idempotency_key: format!("optimize:{run_id}:{}:quality", evidence.variant),
        submitted_at: chrono::Utc::now().to_rfc3339(),
    };
    let operational_result = EvaluationResult {
        schema_version: EVAL_SCHEMA_VERSION,
        eval_id,
        evidence_digest: subject.evidence_digest.clone(),
        evaluator: EvaluatorIdentity {
            authority_id: "bitrouter.optimization.metering".into(),
            evaluator_id: "bitrouter.normalized-showback".into(),
            kind: EvaluatorKind::Generic,
            version: "1".into(),
            config_digest: canonical_digest(&("bitrouter.normalized-showback", 1_u32))?,
        },
        verdict: EvalVerdict::Inconclusive,
        metrics: operational_metrics,
        hard_violations: Vec::new(),
        confidence_ppm: None,
        evidence_refs: vec!["normalized-showback".into()],
        decision_credit: operational_credit,
        idempotency_key: format!("optimize:{run_id}:{}:metering", evidence.variant),
        submitted_at: chrono::Utc::now().to_rfc3339(),
    };
    let operational_admission = service
        .submit(operational_result, SubmissionPrincipal::LocalOperator)
        .await?;
    if operational_admission.status != AdmissionStatus::Admitted {
        anyhow::bail!(
            "normalized metering result was not admitted: {} ({})",
            operational_admission.reason,
            evidence.variant
        );
    }
    let agentic_admission = service
        .submit(agentic_result, SubmissionPrincipal::LocalOperator)
        .await?;
    if agentic_admission.status != AdmissionStatus::Admitted {
        anyhow::bail!(
            "agentic evaluation was not admitted: {} ({})",
            agentic_admission.reason,
            evidence.variant
        );
    }
    Ok(vec![
        operational_admission.result_id,
        agentic_admission.result_id,
    ])
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
        reason: super::evaluator::redact_and_bound(&evaluation.reason, 4096),
        evidence_digest: canonical_digest(&evidence.attributions)?,
        policy_digest: evidence.policy_digest.clone(),
        request_count: evidence.request_count,
        normalized_cost_micro_usd: evidence.normalized_cost_micro_usd,
        observed_latency_ms: evidence.observed_latency_ms,
        elapsed_ms: duration_ms(evidence.execution.elapsed),
    })
}

fn outcome_summary(report: &VariantReport) -> OutcomeSummary {
    OutcomeSummary {
        verdict: report.verdict,
        evidence_digest: report.evidence_digest.clone(),
        policy_digest: report.policy_digest.clone(),
        normalized_cost_micro_usd: Some(report.normalized_cost_micro_usd),
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
    let database_path = absolute.to_string_lossy().replace('\\', "/");
    Ok(format!("sqlite://{database_path}?{params}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    use bitrouter_sdk::config::{AdequacyConfig, EvalConfig};

    use super::{
        FrozenWorkflowWorkspaces, fingerprint_workflow, git_output, percentage_delta_ppm,
        split_weight, submit_variant_result, writable_database_url,
    };
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
        let directory = tempfile::tempdir()?;
        let config = directory.path().join("bitrouter.yaml");
        assert_eq!(
            writable_database_url("sqlite://./state.db", &config)?,
            format!(
                "sqlite://{}?mode=rwc",
                directory
                    .path()
                    .join("state.db")
                    .to_string_lossy()
                    .replace('\\', "/")
            )
        );
        Ok(())
    }

    #[tokio::test]
    async fn workflow_fingerprint_covers_argv_and_referenced_files() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        tokio::fs::write(directory.path().join("runner"), b"runner-v1").await?;
        tokio::fs::write(directory.path().join("suite.json"), b"suite-v1").await?;
        for arguments in [
            vec!["init"],
            vec!["add", "."],
            vec![
                "-c",
                "user.name=BitRouter Test",
                "-c",
                "user.email=test@bitrouter.invalid",
                "commit",
                "-m",
                "fixture",
            ],
        ] {
            let status = tokio::process::Command::new("git")
                .arg("-C")
                .arg(directory.path())
                .args(arguments)
                .status()
                .await?;
            assert!(status.success());
        }
        let command = crate::optimization::WorkflowCommand {
            command: vec!["./runner".into(), "suite.json".into()],
            inputs: Vec::new(),
            timeout_secs: 60,
        };
        let before = fingerprint_workflow(&command, directory.path()).await?;
        assert_eq!(before.referenced_files.len(), 2);

        tokio::fs::write(directory.path().join("suite.json"), b"suite-v2").await?;
        let after = fingerprint_workflow(&command, directory.path()).await?;
        assert_eq!(before.argv_digest, after.argv_digest);
        assert_ne!(before.referenced_files, after.referenced_files);
        assert_ne!(
            before.argv_digest,
            fingerprint_workflow(
                &crate::optimization::WorkflowCommand {
                    command: vec!["./runner".into()],
                    inputs: Vec::new(),
                    timeout_secs: 60,
                },
                directory.path(),
            )
            .await?
            .argv_digest
        );
        Ok(())
    }

    #[tokio::test]
    async fn frozen_variants_share_one_manifest_but_not_mutable_state() -> anyhow::Result<()> {
        let source = tempfile::tempdir()?;
        let private = tempfile::tempdir()?;
        tokio::fs::write(source.path().join("tracked.txt"), b"tracked-v1").await?;
        tokio::fs::write(source.path().join(".gitignore"), b"node_modules/\n").await?;
        tokio::fs::create_dir_all(source.path().join("node_modules/pkg")).await?;
        tokio::fs::write(
            source.path().join("node_modules/pkg/index.js"),
            b"dependency-v1",
        )
        .await?;
        for arguments in [
            vec!["init"],
            vec!["add", "."],
            vec![
                "-c",
                "user.name=BitRouter Test",
                "-c",
                "user.email=test@bitrouter.invalid",
                "commit",
                "-m",
                "fixture",
            ],
        ] {
            let status = tokio::process::Command::new("git")
                .arg("-C")
                .arg(source.path())
                .args(arguments)
                .status()
                .await?;
            assert!(status.success());
        }
        tokio::fs::write(source.path().join("tracked.txt"), b"tracked-v2").await?;

        let mut workspaces = FrozenWorkflowWorkspaces::prepare(
            source.path(),
            &private.path().join("workspaces"),
            &["git".into(), "status".into(), "--short".into()],
            &[PathBuf::from("node_modules")],
        )
        .await?;
        for root in [&workspaces.baseline_root, &workspaces.candidate_root] {
            assert!(root.join(".git").is_file());
            assert_eq!(
                tokio::fs::read(root.join("tracked.txt")).await?,
                b"tracked-v2"
            );
            assert_eq!(
                tokio::fs::read(root.join("node_modules/pkg/index.js")).await?,
                b"dependency-v1"
            );
        }
        tokio::fs::write(
            workspaces.baseline_root.join("tracked.txt"),
            b"baseline-only",
        )
        .await?;
        assert_eq!(
            tokio::fs::read(workspaces.candidate_root.join("tracked.txt")).await?,
            b"tracked-v2"
        );
        workspaces.cleanup()?;
        let listed = git_output(source.path(), &["worktree", "list", "--porcelain"]).await?;
        assert!(!String::from_utf8(listed)?.contains(&private.path().display().to_string()));
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_canonical_paths_remove_only_the_verbatim_prefix() {
        assert_eq!(
            super::normalize_canonical_path(PathBuf::from(r"\\?\C:\work\repo")),
            PathBuf::from(r"C:\work\repo")
        );
        assert_eq!(
            super::normalize_canonical_path(PathBuf::from(r"\\?\UNC\server\share\repo")),
            PathBuf::from(r"\\server\share\repo")
        );
    }

    fn active_policy() -> PolicyLock {
        let mut lock = PolicyLock {
            lockfile_version: 1,
            artifact: None,
            ..Default::default()
        };
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
            normalized_cost_micro_usd: if name == "baseline" { 100 } else { 60 },
            observed_latency_ms: 200,
            observations: vec![RouteObservation {
                request_key: decision.request_key.clone(),
                selected_tier: tier.into(),
                normalized_cost_micro_usd: Some(60),
            }],
            attributions: vec![VariantAttribution {
                request_id: format!("req-{name}"),
                decision,
                usage_origin: bitrouter_sdk::language_model::UsageOrigin::ProviderReported,
                pricing_source: crate::metering::PricingSource::Configured,
                pricing_version:
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                normalized_cost_micro_usd: 60,
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
            adapter_integrity: "sha512-test".into(),
            runtime_executable: "codex".into(),
            runtime_version: "1.0.0".into(),
            runtime_digest:
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
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
