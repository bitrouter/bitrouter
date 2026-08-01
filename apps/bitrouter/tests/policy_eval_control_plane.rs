use std::collections::{BTreeMap, BTreeSet};

use bitrouter::eval::EvalService;
use bitrouter::eval::admission::SubmissionPrincipal;
use bitrouter::eval::compiler::EvalEvidenceSnapshot;
use bitrouter::eval::store::EvalStore;
use bitrouter::eval::types::{
    EVAL_SCHEMA_VERSION, EvalDecisionRef, EvalScope, EvalSubject, EvalVerdict, EvaluationResult,
    EvaluatorIdentity, EvaluatorKind, evidence_digest,
};
use bitrouter::policy_compile::{CompileInput, LegacyAdequacySnapshot, compile_candidate};
use bitrouter::policy_lock::{PolicyDefinition, PolicyLock, deterministic_yaml, semantic_digest};

fn base_lock() -> PolicyLock {
    PolicyLock {
        lockfile_version: 1,
        artifact: None,
        policies: BTreeMap::from([(
            "auto".to_string(),
            PolicyDefinition {
                tiers: BTreeMap::from([
                    ("economy".to_string(), "vendor:economy".to_string()),
                    ("strong".to_string(), "vendor:strong".to_string()),
                ]),
                default_tier: Some("strong".to_string()),
                tool_use_tier: Some("strong".to_string()),
                tool_safe_tiers: vec!["strong".to_string(), "economy".to_string()],
                ..PolicyDefinition::default()
            },
        )]),
        certificates: BTreeMap::new(),
    }
}

#[test]
fn policy_publish_cli_promotes_the_exact_compiled_candidate() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config_path = directory.path().join("bitrouter.yaml");
    let active_path = directory.path().join("policy-lock.yaml");
    let candidate_path = directory.path().join("candidate.yaml");
    std::fs::write(
        &config_path,
        r#"policy:
  mode: adaptive
presets:
  auto:
    model: vendor:strong
    policy: auto
"#,
    )?;
    let active = base_lock();
    std::fs::write(&active_path, deterministic_yaml(&active)?)?;
    let legacy = LegacyAdequacySnapshot {
        snapshot_time_unix_ms: 1_785_369_600_000,
        pins: Vec::new(),
        exploration: Vec::new(),
        semantic_successes: Vec::new(),
        reliability_events: Vec::new(),
    };
    let candidate = compile_candidate(CompileInput {
        current: &active,
        parent_digest: Some(&semantic_digest(&active)?),
        legacy: &legacy,
        eval: None,
        proposed_progress_guards: None,
    })?
    .document;
    std::fs::write(&candidate_path, deterministic_yaml(&candidate)?)?;

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_bitrouter"))
        .args([
            "policy",
            "publish",
            candidate_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("candidate path is not UTF-8"))?,
            "--config",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("config path is not UTF-8"))?,
        ])
        .output()?;

    assert!(
        output.status.success(),
        "publish failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let published: PolicyLock = serde_saphyr::from_str(&std::fs::read_to_string(active_path)?)?;
    assert_eq!(semantic_digest(&published)?, semantic_digest(&candidate)?);
    Ok(())
}

#[test]
fn frozen_policy_rejects_candidate_without_touching_active_bytes() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config_path = directory.path().join("bitrouter.yaml");
    let active_path = directory.path().join("policy-lock.yaml");
    let candidate_path = directory.path().join("candidate.yaml");
    std::fs::write(
        &config_path,
        "policy:\n  mode: frozen\npresets:\n  auto:\n    model: vendor:strong\n    policy: auto\n",
    )?;
    let active = base_lock();
    let active_bytes = deterministic_yaml(&active)?;
    std::fs::write(&active_path, &active_bytes)?;
    std::fs::write(&candidate_path, deterministic_yaml(&compile(&active)?)?)?;

    let output = publish_command(&candidate_path, &config_path)?;

    assert!(!output.status.success());
    assert_eq!(std::fs::read_to_string(active_path)?, active_bytes);
    Ok(())
}

#[tokio::test]
async fn stale_candidate_loses_compare_and_swap_without_touching_active_bytes() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let config_path = directory.path().join("bitrouter.yaml");
    let active_path = directory.path().join("policy-lock.yaml");
    let candidate_path = directory.path().join("candidate.yaml");
    std::fs::write(
        &config_path,
        "policy:\n  mode: adaptive\npresets:\n  auto:\n    model: vendor:strong\n    policy: auto\n",
    )?;
    let original = base_lock();
    std::fs::write(&candidate_path, deterministic_yaml(&compile(&original)?)?)?;
    let mut newer = original;
    newer
        .policies
        .get_mut("auto")
        .ok_or_else(|| anyhow::anyhow!("base fixture must contain the auto policy"))?
        .routes
        .insert("agent_trace/v1|edit|normal".into(), "strong".into());
    let newer_bytes = deterministic_yaml(&newer)?;
    std::fs::write(&active_path, &newer_bytes)?;

    let error = bitrouter::policy_lock::publish_candidate_file(&config_path, &candidate_path)
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("stale publication unexpectedly succeeded"))?;

    assert_eq!(std::fs::read_to_string(active_path)?, newer_bytes);
    assert!(error.to_string().contains("parent digest"));
    Ok(())
}

#[test]
fn concurrent_publishers_allow_exactly_one_parent_transition() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let active_path = directory.path().join("policy-lock.yaml");
    let history_path = directory.path().join("history");
    let active = base_lock();
    let candidate = compile(&active)?;
    let parent_digest = semantic_digest(&active)?;
    std::fs::write(&active_path, deterministic_yaml(&active)?)?;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));

    let publishers = (0..2)
        .map(|_| {
            let active_path = active_path.clone();
            let history_path = history_path.clone();
            let candidate = candidate.clone();
            let parent_digest = parent_digest.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                bitrouter::policy_lock::publish_candidate(
                    &active_path,
                    &parent_digest,
                    &candidate,
                    &history_path,
                )
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();

    let mut successes = 0;
    let mut failures = Vec::new();
    for publisher in publishers {
        match publisher.join() {
            Ok(Ok(_)) => successes += 1,
            Ok(Err(error)) => failures.push(error.to_string()),
            Err(_) => anyhow::bail!("publisher thread panicked"),
        }
    }

    assert_eq!(successes, 1);
    assert_eq!(failures.len(), 1);
    assert!(failures[0].contains("policy lock changed"));
    let published: PolicyLock = serde_saphyr::from_str(&std::fs::read_to_string(active_path)?)?;
    assert_eq!(semantic_digest(&published)?, semantic_digest(&candidate)?);
    Ok(())
}

fn compile(active: &PolicyLock) -> anyhow::Result<PolicyLock> {
    let legacy = LegacyAdequacySnapshot {
        snapshot_time_unix_ms: 1_785_369_600_000,
        pins: Vec::new(),
        exploration: Vec::new(),
        semantic_successes: Vec::new(),
        reliability_events: Vec::new(),
    };
    Ok(compile_candidate(CompileInput {
        current: active,
        parent_digest: Some(&semantic_digest(active)?),
        legacy: &legacy,
        eval: None,
        proposed_progress_guards: None,
    })?
    .document)
}

fn publish_command(
    candidate_path: &std::path::Path,
    config_path: &std::path::Path,
) -> anyhow::Result<std::process::Output> {
    Ok(std::process::Command::new(env!("CARGO_BIN_EXE_bitrouter"))
        .args([
            "policy",
            "publish",
            candidate_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("candidate path is not UTF-8"))?,
            "--config",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("config path is not UTF-8"))?,
        ])
        .output()?)
}

#[tokio::test]
async fn snapshot_compile_publish_preserves_exact_eval_lineage() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config_path = directory.path().join("bitrouter.yaml");
    let active_path = directory.path().join("policy-lock.yaml");
    let candidate_path = directory.path().join("candidate.yaml");
    std::fs::write(
        &config_path,
        "policy:\n  mode: adaptive\npresets:\n  auto:\n    model: vendor:strong\n    policy: auto\n",
    )?;
    let active = base_lock();
    std::fs::write(&active_path, deterministic_yaml(&active)?)?;
    let db = bitrouter::db::connect("sqlite::memory:").await?;
    bitrouter::db::run_migrations(&db).await?;
    let store = EvalStore::new(db);
    let service = EvalService::new(store.clone(), Default::default());
    let evidence = Vec::new();
    let subject = EvalSubject {
        schema_version: EVAL_SCHEMA_VERSION,
        eval_id: "eval-publication".into(),
        scope: EvalScope::Task,
        subject_id: "task-publication".into(),
        policy_digest: semantic_digest(&active)?,
        preset: Some("auto".into()),
        cohort: None,
        holdout: false,
        decisions: vec![EvalDecisionRef {
            decision_id: "decision-publication".into(),
            policy: "auto".into(),
            request_key: "agent_trace/v1|edit|normal".into(),
            selected_tier: "economy".into(),
            baseline_tier: Some("strong".into()),
            policy_digest: semantic_digest(&active)?,
        }],
        requested_dimensions: BTreeSet::from(["quality.pass".into()]),
        evidence_digest: evidence_digest(&evidence)?,
        evidence,
        observed_at: "2026-07-30T00:00:00Z".into(),
    };
    store.insert_subject(&subject).await?;
    service
        .submit(
            EvaluationResult {
                schema_version: EVAL_SCHEMA_VERSION,
                eval_id: subject.eval_id.clone(),
                evidence_digest: subject.evidence_digest.clone(),
                evaluator: EvaluatorIdentity {
                    authority_id: "task-native".into(),
                    evaluator_id: "terminal-bench".into(),
                    kind: EvaluatorKind::TaskNative,
                    version: "1".into(),
                    config_digest: semantic_digest(&active)?,
                },
                verdict: EvalVerdict::Pass,
                metrics: BTreeMap::new(),
                hard_violations: Vec::new(),
                confidence_ppm: Some(1_000_000),
                evidence_refs: Vec::new(),
                decision_credit: BTreeMap::new(),
                idempotency_key: "result-publication".into(),
                submitted_at: "2026-07-30T00:01:00Z".into(),
            },
            SubmissionPrincipal::LocalOperator,
        )
        .await?;
    let manifest = store
        .freeze_snapshot_for_owner("2026-07-30T00:02:00Z", "local")
        .await?;
    let eval = EvalEvidenceSnapshot::load(&store, &manifest.evidence_root).await?;
    let legacy = LegacyAdequacySnapshot {
        snapshot_time_unix_ms: 1_785_369_600_000,
        pins: Vec::new(),
        exploration: Vec::new(),
        semantic_successes: Vec::new(),
        reliability_events: Vec::new(),
    };
    let candidate = compile_candidate(CompileInput {
        current: &active,
        parent_digest: Some(&semantic_digest(&active)?),
        legacy: &legacy,
        eval: Some(&eval),
        proposed_progress_guards: None,
    })?
    .document;
    std::fs::write(&candidate_path, deterministic_yaml(&candidate)?)?;

    bitrouter::policy_lock::publish_candidate_file(&config_path, &candidate_path).await?;

    let published: PolicyLock = serde_saphyr::from_str(&std::fs::read_to_string(active_path)?)?;
    assert_eq!(semantic_digest(&published)?, semantic_digest(&candidate)?);
    assert_eq!(
        published
            .artifact
            .as_ref()
            .and_then(|artifact| artifact.eval_snapshot_root.as_deref()),
        Some(manifest.evidence_root.as_str())
    );
    assert_eq!(
        published.policies["auto"].routes["agent_trace/v1|edit|normal"],
        "economy"
    );
    Ok(())
}
