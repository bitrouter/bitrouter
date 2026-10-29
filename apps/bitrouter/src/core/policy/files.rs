//! Evidence compilation and explicit publication of routing policy files.

use crate::commands::invocation;
use crate::config::{Config, PolicyRuntimeMode};
use crate::policy_lock::{
    PolicyFileUpdate, PolicyLock, PromotionVerdict, default_history_dir, diff_explanations, load,
    load_for_config, publish_candidate, publish_candidate_unlocked, semantic_digest,
    validate_for_config,
};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Compile a candidate from a caller-frozen database snapshot time.
pub async fn compile_files(
    config_path: &Path,
    snapshot_time_unix_ms: i64,
) -> Result<PolicyFileUpdate> {
    compile_files_with_eval(config_path, snapshot_time_unix_ms, None).await
}

/// Compile against an explicit immutable eval snapshot. Omitting the root
/// compiles from the frozen adequacy snapshot alone.
pub async fn compile_files_with_eval(
    config_path: &Path,
    snapshot_time_unix_ms: i64,
    eval_evidence_root: Option<&str>,
) -> Result<PolicyFileUpdate> {
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config = crate::config::parse(&raw).context("parsing bitrouter.yaml")?;
    let loaded = load_for_config(&config, Some(config_path))
        .await?
        .ok_or_else(|| anyhow::anyhow!("no policy lock is configured"))?;
    let database_url = readonly_database_url(&config.database.url, config_path)?;
    let db = crate::db::connect(&database_url)
        .await
        .map_err(anyhow::Error::from)?;
    let eval = match eval_evidence_root {
        Some(root) => Some(
            crate::core::eval::compiler::EvalEvidenceSnapshot::load(
                &crate::core::eval::store::EvalStore::new(db),
                root,
            )
            .await?,
        ),
        None => None,
    };
    let compiled = crate::core::policy::compile_candidate(crate::core::policy::CompileInput {
        current: &loaded.document,
        parent_digest: Some(&loaded.digest),
        snapshot_time_unix_ms,
        eval: eval.as_ref(),
        proposed_progress_guards: None,
    })?;
    let digest = semantic_digest(&compiled.document)?;
    let changes = compiled
        .changes
        .iter()
        .map(|change| {
            let previous = change.previous_tier.as_deref().unwrap_or("default");
            format!(
                "{}: {} {} -> {}",
                change.policy, change.request_key, previous, change.selected_tier
            )
        })
        .collect();
    let conflicts = compiled
        .conflicts
        .iter()
        .map(|conflict| {
            format!(
                "{}: {} keeps operator tier {} (compiler recommended {}): {}",
                conflict.policy,
                conflict.request_key,
                conflict.operator_tier,
                conflict.recommended_tier,
                conflict.reason
            )
        })
        .collect();
    Ok(PolicyFileUpdate {
        path: loaded.file_path()?.to_path_buf(),
        digest,
        document: compiled.document,
        changes,
        conflicts,
    })
}

/// Validate and publish one exact precompiled candidate. The candidate's
/// parent digest is the compare-and-swap token, so a stale compiler can never
/// overwrite a newer active lock.
pub async fn publish_candidate_file(
    config_path: &Path,
    candidate_path: &Path,
) -> Result<PolicyFileUpdate> {
    publish_candidate_file_inner(config_path, candidate_path, false).await
}

/// Publish a candidate while the caller holds the active policy publication lock.
pub async fn publish_candidate_file_unlocked(
    config_path: &Path,
    candidate_path: &Path,
) -> Result<PolicyFileUpdate> {
    publish_candidate_file_inner(config_path, candidate_path, true).await
}

async fn publish_candidate_file_inner(
    config_path: &Path,
    candidate_path: &Path,
    lock_held: bool,
) -> Result<PolicyFileUpdate> {
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config = crate::config::parse(&raw).context("parsing bitrouter.yaml")?;
    if config.policy.mode == PolicyRuntimeMode::Frozen {
        anyhow::bail!(
            "policy runtime mode is frozen; set `policy.mode: adaptive` before publishing"
        );
    }
    let active = load_for_config(&config, Some(config_path))
        .await?
        .ok_or_else(|| anyhow::anyhow!("no policy lock is configured"))?;
    let candidate = load(candidate_path).await?;
    validate_for_config(&config, &candidate.document)?;
    let artifact = candidate
        .document
        .artifact
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("policy publish requires a compiled candidate"))?;
    let parent_digest = artifact
        .parent_digest
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("compiled candidate has no parent digest"))?;
    if parent_digest != active.digest {
        anyhow::bail!(
            "candidate parent digest {parent_digest} does not match active policy digest {}; recompile against the current lock",
            active.digest
        );
    }
    if candidate
        .document
        .certificates
        .values()
        .flat_map(BTreeMap::values)
        .any(|certificate| certificate.verdict == PromotionVerdict::Blocked)
    {
        anyhow::bail!("compiled candidate contains blocked route conflicts");
    }
    let differences = diff_explanations(&active.document, &candidate.document);
    let history_dir = default_history_dir(active.file_path()?);
    let record = if lock_held {
        publish_candidate_unlocked(
            active.file_path()?,
            parent_digest,
            &candidate.document,
            &history_dir,
        )?
    } else {
        publish_candidate(
            active.file_path()?,
            parent_digest,
            &candidate.document,
            &history_dir,
        )?
    };
    Ok(PolicyFileUpdate {
        path: active.file_path()?.to_path_buf(),
        digest: record.child_digest,
        document: candidate.document,
        changes: differences,
        conflicts: Vec::new(),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct EvidenceVerification {
    pub policy_digest: String,
    pub evidence_root: String,
    pub eval_snapshot_root: Option<String>,
    pub eval_results: usize,
}

/// Reconstruct the artifact-level evidence root from the local append-only
/// ledger and content-addressed eval snapshot. This never changes the lock.
pub async fn verify_evidence_files(config_path: &Path) -> Result<EvidenceVerification> {
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config = crate::config::parse(&raw).context("parsing bitrouter.yaml")?;
    let loaded = load_for_config(&config, Some(config_path))
        .await?
        .ok_or_else(|| anyhow::anyhow!("no policy lock is configured"))?;
    verify_document_evidence_with_config(config_path, &config, &loaded.document, loaded.digest)
        .await
}

/// Reconstruct evidence for a reviewed candidate before it becomes active.
pub async fn verify_document_evidence(
    config_path: &Path,
    document: &PolicyLock,
) -> Result<EvidenceVerification> {
    validate_for_config(
        &crate::config::parse(
            &tokio::fs::read_to_string(config_path)
                .await
                .with_context(|| format!("reading {}", config_path.display()))?,
        )
        .context("parsing bitrouter.yaml")?,
        document,
    )?;
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config = crate::config::parse(&raw).context("parsing bitrouter.yaml")?;
    verify_document_evidence_with_config(config_path, &config, document, semantic_digest(document)?)
        .await
}

async fn verify_document_evidence_with_config(
    config_path: &Path,
    config: &Config,
    document: &PolicyLock,
    policy_digest: String,
) -> Result<EvidenceVerification> {
    let artifact = document
        .artifact
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("policy lock v1 has no verifiable evidence artifact"))?;
    let database_url = readonly_database_url(&config.database.url, config_path)?;
    let db = crate::db::connect(&database_url)
        .await
        .map_err(anyhow::Error::from)?;
    let eval = match artifact.eval_snapshot_root.as_deref() {
        Some(root) => Some(
            crate::core::eval::compiler::EvalEvidenceSnapshot::load(
                &crate::core::eval::store::EvalStore::new(db),
                root,
            )
            .await?,
        ),
        None => None,
    };
    let reconstructed = eval
        .as_ref()
        .map(|eval| eval.evidence_root.clone())
        .unwrap_or(crate::storage::evaluation::canonical_digest(&(
            "policy-evidence-v4",
            "empty",
        ))?);
    if reconstructed != artifact.evidence_root {
        anyhow::bail!("policy artifact evidence_root does not match local evidence");
    }
    Ok(EvidenceVerification {
        policy_digest,
        evidence_root: reconstructed,
        eval_snapshot_root: eval.as_ref().map(|snapshot| snapshot.evidence_root.clone()),
        eval_results: eval.map_or(0, |snapshot| snapshot.records.len()),
    })
}

/// Read adequacy evidence and project it into a candidate policy lock. Dry-run
/// is the default. Applying is permitted only in adaptive process mode.
pub async fn evolve_files(config_path: &Path, apply: bool) -> Result<PolicyFileUpdate> {
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config = crate::config::parse(&raw).context("parsing bitrouter.yaml")?;
    if apply && config.policy.mode == PolicyRuntimeMode::Frozen {
        anyhow::bail!(
            "policy runtime mode is frozen; set `policy.mode: adaptive` before `policy evolve --apply`"
        );
    }
    let update = compile_files(config_path, chrono::Utc::now().timestamp_millis()).await?;
    if apply {
        if !update.conflicts.is_empty() {
            anyhow::bail!(
                "compiled policy has unresolved conflicts; inspect `{} policy compile` before \
                 applying",
                invocation::name()
            );
        }
        let loaded = load_for_config(&config, Some(config_path))
            .await?
            .ok_or_else(|| anyhow::anyhow!("no policy lock is configured"))?;
        if loaded.digest != update.digest {
            let history_dir = default_history_dir(loaded.file_path()?);
            publish_candidate(
                loaded.file_path()?,
                &loaded.digest,
                &update.document,
                &history_dir,
            )?;
        }
    }
    Ok(update)
}

fn readonly_database_url(url: &str, config_path: &Path) -> Result<String> {
    let Some(after_scheme) = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))
    else {
        return Ok(url.to_string());
    };
    let (path_part, query) = after_scheme
        .split_once('?')
        .map_or((after_scheme, None), |(path, query)| (path, Some(query)));
    if path_part.is_empty() || path_part == ":memory:" {
        anyhow::bail!("policy evolution requires a persistent adequacy database");
    }
    let path = Path::new(path_part.strip_prefix("./").unwrap_or(path_part));
    let home = config_path.parent().unwrap_or_else(|| Path::new("."));
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        home.join(path)
    };
    if !absolute.is_file() {
        anyhow::bail!("adequacy database '{}' does not exist", absolute.display());
    }
    let mut params = query
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_default();
    if !params.split('&').any(|part| part.starts_with("mode=")) {
        if !params.is_empty() {
            params.push('&');
        }
        params.push_str("mode=ro");
    }
    Ok(format!("sqlite://{}?{params}", absolute.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy_lock::{export_candidate_file, initialize_files, set_mode_file};

    #[tokio::test]
    async fn frozen_config_refuses_evolution_apply() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("bitrouter.yaml");
        tokio::fs::write(
            &config_path,
            r#"database:
  url: sqlite://./bitrouter.db
presets:
  coding:
    model: anthropic/claude-opus-4.8
"#,
        )
        .await
        .unwrap();
        initialize_files(
            &config_path,
            "terminal-bench",
            "coding",
            None,
            "moonshotai/kimi-k2.7-code",
        )
        .await
        .unwrap();
        let initialized = tokio::fs::read_to_string(&config_path).await.unwrap();
        tokio::fs::write(
            &config_path,
            initialized.replace("mode: adaptive", "mode: frozen"),
        )
        .await
        .unwrap();

        let error = evolve_files(&config_path, true).await.unwrap_err();

        assert!(error.to_string().contains("runtime mode is frozen"));
        assert!(!dir.path().join("bitrouter.db").exists());
    }

    #[tokio::test]
    async fn frozen_config_can_export_without_mutating_active_files() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("bitrouter.yaml");
        tokio::fs::write(
            &config_path,
            r#"database:
  url: sqlite://./bitrouter.db
presets:
  coding:
    model: anthropic/claude-opus-4.8
"#,
        )
        .await
        .unwrap();
        let initialized = initialize_files(
            &config_path,
            "terminal-bench",
            "coding",
            None,
            "moonshotai/kimi-k2.7-code",
        )
        .await
        .unwrap();
        let db_path = dir.path().join("bitrouter.db");
        let db = crate::db::connect(&format!("sqlite://{}", db_path.display()))
            .await
            .unwrap();
        crate::db::run_migrations(&db).await.unwrap();
        db.close().await.unwrap();
        let before_config = tokio::fs::read(&config_path).await.unwrap();
        let before_lock = tokio::fs::read(&initialized.path).await.unwrap();
        let candidate_path = dir.path().join("candidate.yaml");

        let update = evolve_files(&config_path, false).await.unwrap();
        export_candidate_file(&update.path, &candidate_path, &update.document).unwrap();

        assert_eq!(tokio::fs::read(&config_path).await.unwrap(), before_config);
        assert_eq!(
            tokio::fs::read(&initialized.path).await.unwrap(),
            before_lock
        );
        let candidate = load(&candidate_path).await.unwrap();
        assert_eq!(candidate.document, update.document);
    }

    #[tokio::test]
    async fn legacy_adequacy_rows_do_not_create_semantic_routes() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("bitrouter.yaml");
        tokio::fs::write(
            &config_path,
            r#"database:
  url: sqlite://./bitrouter.db
presets:
  coding:
    model: anthropic/claude-opus-4.8
"#,
        )
        .await?;
        initialize_files(
            &config_path,
            "coding",
            "coding",
            None,
            "moonshotai/kimi-k2.7-code",
        )
        .await?;
        set_mode_file(&config_path, PolicyRuntimeMode::Adaptive).await?;
        let db_path = dir.path().join("bitrouter.db");
        let db = crate::db::connect(&format!("sqlite://{}", db_path.display())).await?;
        crate::db::run_migrations(&db).await?;
        let store = crate::core::adequacy::store::AdequacyStore::new(db);
        let request_key = "semantic_route/v1|unknown|implement|normal";
        let ledger_key = format!("coding\0{request_key}");
        store.upsert_exploration(&ledger_key, 4, 3, true).await?;
        store
            .record_semantic_success(&ledger_key, "terminal-bench/task-a")
            .await?;

        let added = evolve_files(&config_path, true).await?;

        assert!(added.changes.is_empty());
        assert!(
            load(&added.path).await?.document.policies["coding"]
                .routes
                .is_empty()
        );

        store.upsert_exploration(&ledger_key, 5, 0, false).await?;
        store.clear_semantic_successes(&ledger_key).await?;
        let unchanged = evolve_files(&config_path, true).await?;

        assert!(unchanged.changes.is_empty());
        assert!(
            load(&unchanged.path).await?.document.policies["coding"]
                .routes
                .is_empty()
        );
        Ok(())
    }
}
