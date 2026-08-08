use std::path::PathBuf;

#[cfg(not(windows))]
use std::path::Path;

use anyhow::{Context, Result};

#[cfg(not(windows))]
use super::{EvaluatorLock, ResolvedEvaluator, WorkflowCommand};
use super::{
    EvaluatorRoute, OptimizationIntent, OptimizationLock, OptimizationPaths, OptimizationPreference,
};

pub struct SetupOptimizationRequest {
    pub intent_path: PathBuf,
    pub source_config: PathBuf,
    pub workflow_command: Vec<String>,
    pub workflow_inputs: Vec<PathBuf>,
    pub timeout_secs: u64,
    pub contract: PathBuf,
    pub contract_contents: Option<String>,
    pub policy: String,
    pub preset: String,
    pub strong: String,
    pub economy: String,
    pub normalized_price_overrides: Vec<String>,
    pub preference: OptimizationPreference,
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

/// Resolve the current strong/economy routes for a named policy, when one is active.
pub async fn existing_tier_routes(
    source_config: &std::path::Path,
    policy: &str,
) -> Result<(Option<String>, Option<String>)> {
    let raw = tokio::fs::read_to_string(source_config)
        .await
        .with_context(|| format!("reading source config {}", source_config.display()))?;
    let parsed = bitrouter_sdk::config::parse(&raw).context("parsing source BitRouter config")?;
    let Some(active) = crate::policy_lock::load_for_config(&parsed, Some(source_config)).await?
    else {
        return Ok((None, None));
    };
    let Some(definition) = active.document.policies.get(policy) else {
        return Ok((None, None));
    };
    Ok((
        definition.tiers.get("strong").cloned(),
        definition.tiers.get("economy").cloned(),
    ))
}

pub fn ensure_workflow_optimization_compatible(
    policy_lock: &crate::policy_lock::PolicyLock,
) -> Result<()> {
    let guarded = policy_lock
        .policies
        .iter()
        .filter_map(|(name, definition)| definition.progress_guard.is_some().then_some(name))
        .cloned()
        .collect::<Vec<_>>();
    if !guarded.is_empty() {
        anyhow::bail!(
            "workflow optimization cannot preserve the active progress guard for policies [{}] in its exact two-tier experiment; use an unguarded @auto policy for this optimization lineage",
            guarded.join(", ")
        );
    }
    Ok(())
}

#[cfg(windows)]
pub async fn setup_optimization(
    _request: SetupOptimizationRequest,
) -> Result<SetupOptimizationOutcome> {
    anyhow::bail!(
        "controlled workflow optimization is not supported on Windows until Job Object process-tree isolation is available"
    )
}

#[cfg(not(windows))]
pub async fn setup_optimization(
    request: SetupOptimizationRequest,
) -> Result<SetupOptimizationOutcome> {
    let paths = OptimizationPaths::for_intent(request.intent_path);
    let root = paths
        .intent
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let source_config = resolve(&root, &request.source_config);
    let contract_path = resolve(&root, &request.contract);
    if paths.intent.exists() {
        if !paths.lock.exists() {
            anyhow::bail!(
                "{} exists without its lock; run `bitrouter optimize resolve --config {}` to recover the interrupted setup",
                paths.intent.display(),
                paths.intent.display()
            );
        }
        anyhow::bail!(
            "{} already exists; use `bitrouter optimize resolve` after editing version-controlled inputs",
            paths.intent.display()
        );
    }
    if paths.lock.exists() {
        anyhow::bail!(
            "{} already exists; refusing to overwrite",
            paths.lock.display()
        );
    }
    if !source_config.is_file() {
        anyhow::bail!(
            "source config '{}' does not exist; run `bitrouter init --yes --write-config` first",
            source_config.display()
        );
    }
    let route_providers = [&request.strong, &request.economy]
        .into_iter()
        .map(|route| {
            route
                .split_once(':')
                .map(|(provider, _)| provider.to_string())
                .filter(|provider| !provider.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!("optimization tier route '{route}' must be provider-qualified")
                })
        })
        .collect::<Result<Vec<_>>>()?;
    for value in &request.normalized_price_overrides {
        crate::metering::UsagePriceOverride::parse(value)
            .map_err(anyhow::Error::from)
            .with_context(|| format!("validating normalized price override {value:?}"))?;
    }
    let evaluator_model = resolve_evaluator_model(
        request.evaluator_route,
        request.evaluator_model,
        &request.evaluator_agent,
    )?;
    validate_resolved_evaluator_model(request.evaluator_route, &evaluator_model)?;
    let evaluator_identity =
        super::evaluator::resolve_catalog_evaluator_identity(&request.evaluator_agent).await?;
    let contract_text = if let Some(contents) = request.contract_contents {
        if contract_path.is_file() {
            let existing = tokio::fs::read_to_string(&contract_path).await?;
            if existing != contents {
                anyhow::bail!(
                    "contract contents differ from existing '{}'; refusing to overwrite",
                    contract_path.display()
                );
            }
            existing
        } else {
            contents
        }
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

    let _config_lock = crate::policy_lock::acquire_publication_lock(&source_config)?;
    let source_raw = tokio::fs::read_to_string(&source_config).await?;
    let source_parsed =
        bitrouter_sdk::config::parse(&source_raw).context("parsing source BitRouter config")?;
    let policy_path = crate::policy_lock::resolve_path(&source_parsed, Some(&source_config))
        .ok_or_else(|| anyhow::anyhow!("cannot resolve source policy lock"))?;
    let _policy_lock = crate::policy_lock::acquire_publication_lock(&policy_path)?;
    if policy_path.is_file()
        && let Some(active) =
            crate::policy_lock::load_for_config(&source_parsed, Some(&source_config)).await?
    {
        ensure_workflow_optimization_compatible(&active.document)?;
    }
    let policy_original = if policy_path.is_file() {
        Some(
            std::fs::read(&policy_path)
                .with_context(|| format!("backing up {}", policy_path.display()))?,
        )
    } else {
        None
    };
    let mut rollback = SetupRollback::new(
        source_config.clone(),
        source_raw.as_bytes().to_vec(),
        policy_path.clone(),
        policy_original,
        contract_path.clone(),
        paths.intent.clone(),
        paths.lock.clone(),
    );
    let outcome: Result<SetupOptimizationOutcome> = async {
        let source_with_providers =
            crate::policy_lock::edit_config_provider_stubs(&source_raw, &route_providers)?;
        if source_with_providers != source_raw {
            crate::policy_lock::write_text_atomic_unlocked(
                &source_config,
                &source_raw,
                &source_with_providers,
            )?;
            rollback.record_source_owned()?;
        }
        let source_raw = tokio::fs::read_to_string(&source_config).await?;
        let source_parsed = bitrouter_sdk::config::parse(&source_raw)
            .context("parsing provider-complete source BitRouter config")?;
        validate_routable_model(&source_parsed, &request.strong).await?;
        validate_routable_model(&source_parsed, &request.economy).await?;
        let policy_exists = if policy_path.is_file() {
            let existing = crate::policy_lock::load(&policy_path).await?;
            if let Some(definition) = existing.document.policies.get(&request.policy) {
                if definition.tiers.get("strong") != Some(&request.strong)
                    || definition.tiers.get("economy") != Some(&request.economy)
                {
                    anyhow::bail!(
                        "existing policy does not match the requested strong and economy routes"
                    );
                }
                true
            } else {
                false
            }
        } else {
            false
        };
        if !policy_exists {
            crate::policy_lock::initialize_files_unlocked(
                &source_config,
                &request.policy,
                &request.preset,
                Some(&request.strong),
                &request.economy,
            )
            .await?;
            rollback.record_source_policy_owned()?;
        } else if source_parsed
            .presets
            .get(&request.preset)
            .and_then(|item| item.policy.as_deref())
            != Some(request.policy.as_str())
        {
            let model = (!source_parsed.presets.contains_key(&request.preset))
                .then_some(request.strong.as_str());
            let repaired = crate::policy_lock::edit_config_policy_with_model(
                &source_raw,
                &request.preset,
                &request.policy,
                model,
                bitrouter_sdk::config::PolicyRuntimeMode::Frozen,
            )?;
            crate::policy_lock::write_text_atomic_unlocked(&source_config, &source_raw, &repaired)?;
            rollback.record_source_policy_owned()?;
        }
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
            crate::policy_lock::write_new_file_atomic(&contract_path, contract_text.as_bytes())?;
            rollback.contract_owned = Some(contract_text.as_bytes().to_vec());
        }
        let intent = OptimizationIntent {
            version: super::OPTIMIZATION_SCHEMA_VERSION,
            workflow: WorkflowCommand {
                command: request.workflow_command,
                inputs: request.workflow_inputs,
                timeout_secs: request.timeout_secs,
            },
            contract: path_relative_to(&root, &contract_path),
            source_config: path_relative_to(&root, &source_config),
            policy: request.policy,
            preset: request.preset,
            strong: request.strong,
            economy: request.economy,
            normalized_price_overrides: request.normalized_price_overrides,
            preference: request.preference,
            evaluator: ResolvedEvaluator {
                agent: request.evaluator_agent.clone(),
                model: evaluator_model.clone(),
                route: request.evaluator_route,
            },
        };
        super::write_intent_create_new(&paths.intent, &intent).await?;
        rollback.intent_owned = Some(std::fs::read(&paths.intent)?);
        let lock = OptimizationLock {
            lockfile_version: super::OPTIMIZATION_SCHEMA_VERSION,
            intent_digest: intent.semantic_digest()?,
            active_policy_digest: active.digest,
            evaluator: EvaluatorLock {
                agent: request.evaluator_agent,
                agent_version: evaluator_identity.adapter_version,
                adapter_integrity: evaluator_identity.adapter_integrity,
                runtime_executable: evaluator_identity.runtime_executable,
                runtime_version: evaluator_identity.runtime_version,
                runtime_digest: evaluator_identity.runtime_digest,
                model: evaluator_model,
                route: request.evaluator_route,
                skill_digest: super::evaluator::embedded_evaluator_digest()?,
                contract_digest: super::evaluator::content_digest(&contract_text),
            },
            latest_run: None,
        };
        super::write_lock_compare_and_swap(&paths.lock, None, &lock).await?;
        rollback.lock_owned = Some(std::fs::read(&paths.lock)?);
        Ok(SetupOptimizationOutcome {
            paths,
            contract_path,
            intent,
            lock,
        })
    }
    .await;
    match outcome {
        Ok(outcome) => {
            rollback.commit();
            Ok(outcome)
        }
        Err(error) => match rollback.restore_checked() {
            Ok(()) => Err(error),
            Err(cleanup) => Err(error.context(format!("setup rollback also failed: {cleanup:#}"))),
        },
    }
}

/// Restores every setup-owned write if setup errors, is cancelled, or
/// unwinds. The CLI holds the project operation lock while this guard exists,
/// so these exact preflight bytes cannot race another optimization command.
#[cfg(not(windows))]
struct SetupRollback {
    source_config: PathBuf,
    source_original: Vec<u8>,
    policy_path: PathBuf,
    policy_original: Option<Vec<u8>>,
    contract_path: PathBuf,
    intent_path: PathBuf,
    lock_path: PathBuf,
    source_owned: Option<Vec<u8>>,
    policy_owned: Option<Vec<u8>>,
    contract_owned: Option<Vec<u8>>,
    intent_owned: Option<Vec<u8>>,
    lock_owned: Option<Vec<u8>>,
    committed: bool,
}

#[cfg(not(windows))]
impl SetupRollback {
    fn new(
        source_config: PathBuf,
        source_original: Vec<u8>,
        policy_path: PathBuf,
        policy_original: Option<Vec<u8>>,
        contract_path: PathBuf,
        intent_path: PathBuf,
        lock_path: PathBuf,
    ) -> Self {
        Self {
            source_config,
            source_original,
            policy_path,
            policy_original,
            contract_path,
            intent_path,
            lock_path,
            source_owned: None,
            policy_owned: None,
            contract_owned: None,
            intent_owned: None,
            lock_owned: None,
            committed: false,
        }
    }

    fn commit(&mut self) {
        self.committed = true;
    }

    fn record_source_owned(&mut self) -> Result<()> {
        self.source_owned = Some(std::fs::read(&self.source_config)?);
        Ok(())
    }

    fn record_source_policy_owned(&mut self) -> Result<()> {
        self.record_source_owned()?;
        self.policy_owned = Some(std::fs::read(&self.policy_path)?);
        Ok(())
    }

    fn restore_checked(&mut self) -> Result<()> {
        let mut failures = Vec::new();
        for (path, owned) in [
            (&self.lock_path, self.lock_owned.as_ref()),
            (&self.intent_path, self.intent_owned.as_ref()),
            (&self.contract_path, self.contract_owned.as_ref()),
        ] {
            if let Some(owned) = owned {
                match std::fs::read(path) {
                    Ok(current) if current == *owned => {
                        if let Err(error) = std::fs::remove_file(path) {
                            failures.push(format!("removing {}: {error}", path.display()));
                        }
                    }
                    Ok(_) => failures.push(format!(
                        "{} changed outside setup; refusing to remove it",
                        path.display()
                    )),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => failures.push(format!("reading {}: {error}", path.display())),
                }
            }
        }
        if let Some(owned) = self.policy_owned.as_ref() {
            match std::fs::read(&self.policy_path) {
                Ok(current) if current == *owned => match &self.policy_original {
                    Some(original) => {
                        if let Err(error) = crate::policy_lock::write_bytes_atomic_unlocked(
                            &self.policy_path,
                            original,
                        ) {
                            failures.push(format!("restoring policy: {error:#}"));
                        }
                    }
                    None => {
                        if let Err(error) = std::fs::remove_file(&self.policy_path) {
                            failures.push(format!("removing created policy: {error}"));
                        }
                    }
                },
                Ok(_) => failures.push(
                    "policy changed outside setup; refusing to overwrite it during rollback".into(),
                ),
                Err(error) => failures.push(format!("reading policy during rollback: {error}")),
            }
        }
        if let Some(owned) = self.source_owned.as_ref() {
            match std::fs::read(&self.source_config) {
                Ok(current) if current == *owned => {
                    let current = String::from_utf8(current)
                        .context("source config became non-UTF-8 during rollback")?;
                    let original = std::str::from_utf8(&self.source_original)
                        .context("original source config was not UTF-8")?;
                    if let Err(error) = crate::policy_lock::write_text_atomic_unlocked(
                        &self.source_config,
                        &current,
                        original,
                    ) {
                        failures.push(format!("restoring source config: {error:#}"));
                    }
                }
                Ok(_) => failures.push(
                    "source config changed outside setup; refusing to overwrite it during rollback"
                        .into(),
                ),
                Err(error) => {
                    failures.push(format!("reading source config during rollback: {error}"))
                }
            }
        }
        self.committed = true;
        if failures.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(failures.join("; "))
        }
    }
}

#[cfg(not(windows))]
impl Drop for SetupRollback {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.restore_checked();
        }
    }
}

#[cfg(not(windows))]
fn resolve(root: &Path, value: &Path) -> PathBuf {
    if value.is_absolute() {
        value.to_path_buf()
    } else {
        root.join(value)
    }
}

#[cfg(not(windows))]
fn path_relative_to(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

pub fn validate_cloud_catalog_model(route: &str) -> Result<()> {
    let model = route.strip_prefix("bitrouter:").ok_or_else(|| {
        anyhow::anyhow!("Cloud evaluator route must use the bitrouter: provider prefix")
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

#[cfg(not(windows))]
fn resolve_evaluator_model(
    route: EvaluatorRoute,
    requested: Option<String>,
    agent: &str,
) -> Result<String> {
    match (route, requested) {
        (EvaluatorRoute::Cloud, Some(model)) => Ok(model),
        (EvaluatorRoute::Cloud, None) => Ok("bitrouter:openai/gpt-5.6-terra".into()),
        (EvaluatorRoute::Direct, Some(model)) => validate_direct_model(model),
        (EvaluatorRoute::Direct, None) if agent == "codex-acp" => detect_codex_model(),
        (EvaluatorRoute::Direct, None) if agent == "claude-acp" => detect_claude_model(),
        (EvaluatorRoute::Direct, None) => anyhow::bail!(
            "could not infer a model for direct evaluator '{agent}'; pass --evaluator-model"
        ),
    }
}

pub fn validate_resolved_evaluator_model(route: EvaluatorRoute, model: &str) -> Result<()> {
    match route {
        EvaluatorRoute::Cloud => validate_cloud_catalog_model(model),
        EvaluatorRoute::Direct => validate_direct_model(model.to_string()).map(|_| ()),
    }
}

pub async fn validate_routable_model(
    source: &bitrouter_sdk::config::Config,
    route: &str,
) -> Result<()> {
    let (provider, model) = route.split_once(':').ok_or_else(|| {
        anyhow::anyhow!("optimization tier route '{route}' must be provider-qualified")
    })?;
    if provider.is_empty() || model.is_empty() || model.starts_with('@') {
        anyhow::bail!("optimization tier route '{route}' is not a concrete provider model");
    }
    let mut resolved = source.clone();
    crate::merge_registry_into(&mut resolved).await;
    bitrouter_providers::apply_builtin_defaults(&mut resolved);
    let provider_config = resolved.providers.get(provider).ok_or_else(|| {
        anyhow::anyhow!(
            "optimization provider '{provider}' is not available from the source config or provider registry"
        )
    })?;
    if !provider_config.active || provider_config.api_base.trim().is_empty() {
        anyhow::bail!(
            "optimization provider '{provider}' is not active and routable; configure its credential or provider entry first"
        );
    }
    bitrouter_sdk::config::routing_table::resolve_route_chain(
        &resolved,
        route,
        &bitrouter_sdk::language_model::RoutingPrefs::default(),
    )
    .with_context(|| format!("resolving optimization tier route '{route}'"))?;
    Ok(())
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

#[cfg(not(windows))]
fn detect_codex_model() -> Result<String> {
    if let Some(model) = std::env::var_os("CODEX_MODEL").filter(|value| !value.is_empty()) {
        return validate_direct_model(model.to_string_lossy().into_owned());
    }
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
    let raw = match std::fs::read_to_string(&config_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok("gpt-5.6-sol".into());
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "reading detected Codex model from {}; pass --evaluator-model to choose explicitly",
                    config_path.display()
                )
            });
        }
    };
    codex_model_from_config(&raw).with_context(|| {
        format!(
            "detecting the Codex judge model from {}; pass --evaluator-model to choose explicitly",
            config_path.display()
        )
    })
}

#[cfg(not(windows))]
fn codex_model_from_config(raw: &str) -> Result<String> {
    let config: toml::Value = toml::from_str(raw).context("parsing Codex config")?;
    let model = config
        .get("model")
        .and_then(toml::Value::as_str)
        .unwrap_or("gpt-5.6-sol");
    validate_direct_model(model.to_string())
}

#[cfg(not(windows))]
fn detect_claude_model() -> Result<String> {
    if let Some(model) = std::env::var_os("ANTHROPIC_MODEL").filter(|value| !value.is_empty()) {
        return validate_direct_model(model.to_string_lossy().into_owned());
    }
    let claude_home = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .map(|home| home.join(".claude"))
        })
        .ok_or_else(|| anyhow::anyhow!("cannot locate the detected Claude configuration"))?;
    let settings_path = claude_home.join("settings.json");
    let raw = match std::fs::read_to_string(&settings_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok("claude-sonnet-4-6".into());
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "reading detected Claude model from {}; pass --evaluator-model to choose explicitly",
                    settings_path.display()
                )
            });
        }
    };
    claude_model_from_settings(&raw).with_context(|| {
        format!(
            "detecting the Claude judge model from {}; pass --evaluator-model to choose explicitly",
            settings_path.display()
        )
    })
}

#[cfg(not(windows))]
fn claude_model_from_settings(raw: &str) -> Result<String> {
    let settings: serde_json::Value =
        serde_json::from_str(raw).context("parsing Claude settings")?;
    let model = settings
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("claude-sonnet-4-6");
    validate_direct_model(model.to_string())
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::{
        SetupRollback, claude_model_from_settings, codex_model_from_config,
        ensure_workflow_optimization_compatible, validate_cloud_catalog_model,
        validate_routable_model,
    };

    #[test]
    fn setup_can_record_a_source_edit_before_the_policy_exists() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("bitrouter.yaml");
        let policy = directory.path().join("policy-lock.yaml");
        std::fs::write(&source, b"inherit_defaults: true\n")?;
        let mut rollback = SetupRollback::new(
            source.clone(),
            Vec::new(),
            policy,
            None,
            directory.path().join("bitrouter.eval.md"),
            directory.path().join("bitrouter.optimize.yaml"),
            directory.path().join("bitrouter.optimize.lock.yaml"),
        );

        rollback.record_source_owned()?;
        rollback.commit();
        Ok(())
    }

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
        assert_eq!(
            codex_model_from_config("model_provider = \"custom\"")?,
            "gpt-5.6-sol"
        );
        Ok(())
    }

    #[test]
    fn cloud_evaluator_models_must_exist_in_the_embedded_cloud_catalog() {
        assert!(validate_cloud_catalog_model("bitrouter:openai/gpt-5.6-terra").is_ok());
        assert!(validate_cloud_catalog_model("bitrouter:deepseek/deepseek-v4-flash-0731").is_ok());
        assert!(validate_cloud_catalog_model("bitrouter:openai/gpt-5.6").is_err());
    }

    #[tokio::test]
    async fn tier_models_may_use_distinct_daemon_providers() -> anyhow::Result<()> {
        let source = bitrouter_sdk::config::parse(
            r#"
providers:
  openai-codex:
    api_base: https://chatgpt.example.test/backend-api/codex
    models: [{ id: gpt-5.6-sol }]
  bitrouter:
    api_base: https://api.bitrouter.example.test/v1
    models: [{ id: deepseek/deepseek-v4-flash-0731 }]
inherit_defaults: false
"#,
        )?;

        validate_routable_model(&source, "openai-codex:gpt-5.6-sol").await?;
        validate_routable_model(&source, "bitrouter:deepseek/deepseek-v4-flash-0731").await?;
        Ok(())
    }

    #[test]
    fn detects_exact_local_claude_model_setting() -> anyhow::Result<()> {
        assert_eq!(
            claude_model_from_settings(r#"{"model":"sonnet[1m]","effortLevel":"high"}"#)?,
            "sonnet[1m]"
        );
        assert_eq!(claude_model_from_settings("{}")?, "claude-sonnet-4-6");
        Ok(())
    }

    #[test]
    fn progress_guard_is_rejected_by_setup_preflight() {
        let mut document = crate::policy_lock::PolicyLock::default();
        let definition = crate::policy_lock::PolicyDefinition {
            progress_guard: Some(crate::trajectory::guard::ProgressGuardPolicy {
                escalation_tier: "strong".into(),
                protected_tiers: std::collections::BTreeSet::from(["strong".into()]),
                max_consecutive_unprotected: Some(3),
                max_same_projection_unprotected: None,
                max_recovery_count: None,
                max_episode_requests: None,
                max_episode_elapsed_ms: None,
                max_episode_cost_micro_usd: None,
                hold_for_requests: 2,
                incomplete_history: crate::trajectory::guard::IncompleteHistoryAction::Escalate,
            }),
            ..Default::default()
        };
        document.policies.insert("auto".into(), definition);

        let error = ensure_workflow_optimization_compatible(&document);

        assert!(error.is_err());
        assert!(
            error
                .err()
                .map(|value| value.to_string())
                .is_some_and(|message| message.contains("progress guard"))
        );
    }
}
