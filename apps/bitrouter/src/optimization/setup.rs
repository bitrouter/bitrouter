use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;

use super::{
    CustomQualityGate, EvaluatorLock, EvaluatorRoute, OptimizationIntent, OptimizationLock,
    OptimizationPaths, OptimizationPreference, ResolvedEvaluator, WorkflowCommand,
};

pub struct SetupOptimizationRequest {
    pub intent_path: PathBuf,
    pub source_config: PathBuf,
    pub workflow_command: Vec<String>,
    pub timeout_secs: u64,
    pub contract: PathBuf,
    pub contract_contents: Option<String>,
    pub policy: String,
    pub preset: String,
    pub strong: String,
    pub economy: String,
    pub preference: OptimizationPreference,
    pub custom_quality: Option<CustomQualityGate>,
    pub evaluator_agent: String,
    pub evaluator_model: Option<String>,
    pub evaluator_route: EvaluatorRoute,
}

pub struct SetupOptimizationOutcome {
    pub paths: OptimizationPaths,
    pub contract_path: PathBuf,
    pub intent: OptimizationIntent,
    pub lock: OptimizationLock,
}

pub async fn setup_optimization(
    request: SetupOptimizationRequest,
) -> Result<SetupOptimizationOutcome> {
    let paths = OptimizationPaths::for_intent(request.intent_path);
    let root = paths.intent.parent().unwrap_or_else(|| Path::new("."));
    let source_config = resolve(root, &request.source_config);
    let contract_path = resolve(root, &request.contract);
    if !source_config.is_file() {
        anyhow::bail!(
            "source config '{}' does not exist; run `bitrouter init --yes --write-config` first",
            source_config.display()
        );
    }
    if !request.strong.starts_with("bitrouter:") || !request.economy.starts_with("bitrouter:") {
        anyhow::bail!("workflow optimization currently requires two BitRouter Cloud routes");
    }
    validate_catalog_model(&request.strong)?;
    validate_catalog_model(&request.economy)?;
    match (request.preference, request.custom_quality.as_ref()) {
        (OptimizationPreference::Custom, None) => {
            anyhow::bail!("custom preference requires explicit PPM quality gates")
        }
        (OptimizationPreference::Custom, Some(_)) | (_, None) => {}
        (_, Some(_)) => anyhow::bail!("explicit PPM quality gates require custom preference"),
    }
    let evaluator_model = resolve_evaluator_model(
        request.evaluator_route,
        request.evaluator_model,
        &request.strong,
        &request.evaluator_agent,
    )?;
    let agent_version =
        super::evaluator::resolve_catalog_adapter_version(&request.evaluator_agent).await?;
    let contract_text = if let Some(contents) = request.contract_contents {
        if contract_path.is_file() {
            anyhow::bail!(
                "contract contents were provided but '{}' already exists",
                contract_path.display()
            );
        }
        contents
    } else if contract_path.is_file() {
        tokio::fs::read_to_string(&contract_path)
            .await
            .with_context(|| format!("reading {}", contract_path.display()))?
    } else {
        "# Workflow success contract\n\nDescribe the observable conditions that mean this agent workflow succeeded. The evaluator must return inconclusive when the captured output cannot prove them.\n".into()
    };
    if contract_text.trim().is_empty() {
        anyhow::bail!("workflow success contract must not be empty");
    }

    let source_raw = tokio::fs::read_to_string(&source_config).await?;
    let source_parsed =
        bitrouter_sdk::config::parse(&source_raw).context("parsing source BitRouter config")?;
    let policy_path = crate::policy_lock::resolve_path(&source_parsed, Some(&source_config))
        .ok_or_else(|| anyhow::anyhow!("cannot resolve source policy lock"))?;
    let policy_exists = if policy_path.is_file() {
        crate::policy_lock::load(&policy_path)
            .await?
            .document
            .policies
            .contains_key(&request.policy)
    } else {
        false
    };
    if !policy_exists {
        crate::policy_lock::initialize_files(
            &source_config,
            &request.policy,
            &request.preset,
            Some(&request.strong),
            &request.economy,
        )
        .await?;
    }
    crate::policy_lock::set_mode_file(
        &source_config,
        bitrouter_sdk::config::PolicyRuntimeMode::Adaptive,
    )
    .await?;
    let source_raw = tokio::fs::read_to_string(&source_config).await?;
    let source_parsed = bitrouter_sdk::config::parse(&source_raw)?;
    let active = crate::policy_lock::load_for_config(&source_parsed, Some(&source_config))
        .await?
        .ok_or_else(|| anyhow::anyhow!("source config has no active policy lock"))?;
    let definition = active
        .document
        .policies
        .get(&request.policy)
        .ok_or_else(|| anyhow::anyhow!("active policy '{}' is missing", request.policy))?;
    if definition.tiers.get("strong") != Some(&request.strong)
        || definition.tiers.get("economy") != Some(&request.economy)
        || source_parsed
            .presets
            .get(&request.preset)
            .and_then(|item| item.policy.as_deref())
            != Some(request.policy.as_str())
    {
        anyhow::bail!(
            "existing policy/preset does not match the requested strong and economy routes"
        );
    }
    if !contract_path.exists() {
        if let Some(parent) = contract_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&contract_path)
            .await?;
        file.write_all(contract_text.as_bytes()).await?;
        file.flush().await?;
    }
    let intent = OptimizationIntent {
        version: super::OPTIMIZATION_SCHEMA_VERSION,
        workflow: WorkflowCommand {
            command: request.workflow_command,
            timeout_secs: request.timeout_secs,
        },
        contract: path_relative_to(root, &contract_path),
        source_config: path_relative_to(root, &source_config),
        policy: request.policy,
        preset: request.preset,
        strong: request.strong,
        economy: request.economy,
        preference: request.preference,
        custom_quality: request.custom_quality,
        evaluator: ResolvedEvaluator {
            agent: request.evaluator_agent.clone(),
            model: evaluator_model.clone(),
            route: request.evaluator_route,
        },
    };
    super::write_intent_create_new(&paths.intent, &intent).await?;
    let lock = OptimizationLock {
        lockfile_version: super::OPTIMIZATION_SCHEMA_VERSION,
        intent_digest: intent.semantic_digest()?,
        active_policy_digest: active.digest,
        evaluator: EvaluatorLock {
            agent: request.evaluator_agent,
            agent_version,
            model: evaluator_model,
            route: request.evaluator_route,
            skill_digest: super::evaluator::embedded_evaluator_digest()?,
            contract_digest: super::evaluator::content_digest(&contract_text),
        },
        latest_run: None,
    };
    super::write_lock_compare_and_swap(&paths.lock, None, &lock).await?;
    Ok(SetupOptimizationOutcome {
        paths,
        contract_path,
        intent,
        lock,
    })
}

fn resolve(root: &Path, value: &Path) -> PathBuf {
    if value.is_absolute() {
        value.to_path_buf()
    } else {
        root.join(value)
    }
}

fn path_relative_to(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

fn validate_catalog_model(route: &str) -> Result<()> {
    let model = route.strip_prefix("bitrouter:").ok_or_else(|| {
        anyhow::anyhow!("optimization route must use the bitrouter: provider prefix")
    })?;
    let catalog: serde_json::Value =
        serde_json::from_str(include_str!("../../../../dist/registry/models.json"))
            .context("parsing embedded model catalog")?;
    let exists = catalog
        .get("data")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|models| {
            models
                .iter()
                .any(|entry| entry.get("id").and_then(serde_json::Value::as_str) == Some(model))
        });
    if !exists {
        anyhow::bail!(
            "Cloud model '{model}' is not in this BitRouter build's catalog; run `bitrouter models --provider bitrouter` and choose a concrete id"
        );
    }
    Ok(())
}

fn resolve_evaluator_model(
    route: EvaluatorRoute,
    requested: Option<String>,
    strong: &str,
    agent: &str,
) -> Result<String> {
    match (route, requested) {
        (EvaluatorRoute::Cloud, Some(model)) => Ok(model),
        (EvaluatorRoute::Cloud, None) => Ok(strong.to_string()),
        (EvaluatorRoute::Direct, Some(model)) => validate_direct_model(model),
        (EvaluatorRoute::Direct, None) if agent == "codex-acp" => detect_codex_model(),
        (EvaluatorRoute::Direct, None) => anyhow::bail!(
            "could not infer a model for direct evaluator '{agent}'; pass --evaluator-model"
        ),
    }
}

fn validate_direct_model(model: String) -> Result<String> {
    if model.starts_with("bitrouter:")
        || model.trim().is_empty()
        || model.len() > 512
        || model.chars().any(char::is_control)
    {
        anyhow::bail!("a direct evaluator requires a bounded non-BitRouter model id");
    }
    Ok(model)
}

fn detect_codex_model() -> Result<String> {
    let codex_home = std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .map(|home| home.join(".codex"))
        })
        .ok_or_else(|| anyhow::anyhow!("cannot locate the detected Codex configuration"))?;
    let config_path = codex_home.join("config.toml");
    let raw = std::fs::read_to_string(&config_path).with_context(|| {
        format!(
            "reading detected Codex model from {}; pass --evaluator-model to choose explicitly",
            config_path.display()
        )
    })?;
    codex_model_from_config(&raw).with_context(|| {
        format!(
            "detecting the Codex judge model from {}; pass --evaluator-model to choose explicitly",
            config_path.display()
        )
    })
}

fn codex_model_from_config(raw: &str) -> Result<String> {
    let config: toml::Value = toml::from_str(raw).context("parsing Codex config")?;
    let model = config
        .get("model")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Codex config has no top-level model"))?;
    validate_direct_model(model.to_string())
}

#[cfg(test)]
mod tests {
    use super::{codex_model_from_config, validate_catalog_model};

    #[test]
    fn detects_exact_local_codex_model_without_importing_agent_settings() -> anyhow::Result<()> {
        let model = codex_model_from_config(
            r#"
model = "gpt-5.6-sol"
model_reasoning_effort = "xhigh"
[projects."/tmp/example"]
trust_level = "trusted"
"#,
        )?;
        assert_eq!(model, "gpt-5.6-sol");
        assert!(codex_model_from_config("model_provider = \"custom\"").is_err());
        Ok(())
    }

    #[test]
    fn setup_models_must_exist_in_the_embedded_cloud_catalog() {
        assert!(validate_catalog_model("bitrouter:openai/gpt-5.6-terra").is_ok());
        assert!(validate_catalog_model("bitrouter:deepseek/deepseek-v4-flash-0731").is_ok());
        assert!(validate_catalog_model("bitrouter:openai/gpt-5.6").is_err());
    }
}
