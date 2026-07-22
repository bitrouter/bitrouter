//! File-backed, preset-bound adaptive routing policies.
//!
//! `policy-lock.yaml` is the current effective policy artifact. Git owns file
//! history; the adequacy database owns evolution evidence. The file contains no
//! runtime generation chain, timestamps, or database ids, so serialising the
//! same semantic policy is deterministic.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use anyhow::{Context, Result};
use bitrouter_sdk::config::{
    AdequacyConfig, Config, PolicyKeyStrategy, PolicyTableConfig, PolicyWriteback,
    validate_policy_table_config,
};
use bitrouter_sdk::language_model::{ModelSelector, PipelineContext};
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::adequacy::AdequacyLedger;
use crate::adequacy::settlement::PendingAdequacyStore;
use crate::adequacy::store::{AdequacyStore, PersistedExplorationState};
use crate::policy_table_router::{PolicyTable, PolicyTableRouter};
use crate::workflow_state::decision::PolicyDecisionJsonlRecorder;

pub const DEFAULT_POLICY_LOCK_FILENAME: &str = "policy-lock.yaml";
pub const POLICY_LOCKFILE_VERSION: u32 = 1;

/// The complete deterministic policy artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyLock {
    /// File-format version only. Evolution history lives in Git and the DB.
    #[serde(rename = "lockfileVersion")]
    pub lockfile_version: u32,
    /// Named policies referenced by `presets.<name>.policy`.
    #[serde(default)]
    pub policies: BTreeMap<String, PolicyDefinition>,
}

impl Default for PolicyLock {
    fn default() -> Self {
        Self {
            lockfile_version: POLICY_LOCKFILE_VERSION,
            policies: BTreeMap::new(),
        }
    }
}

/// One named effective routing policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyDefinition {
    pub key_strategy: PolicyKeyStrategy,
    pub tiers: BTreeMap<String, String>,
    /// Workflow-state/fingerprint key to tier. `fingerprints` is accepted as a
    /// migration alias, while deterministic output always uses `routes`.
    #[serde(alias = "fingerprints")]
    pub routes: BTreeMap<String, String>,
    pub default_tier: Option<String>,
    pub tool_use_tier: Option<String>,
    pub tool_safe_tiers: Vec<String>,
    pub adequacy: AdequacyConfig,
}

impl Default for PolicyDefinition {
    fn default() -> Self {
        Self {
            key_strategy: PolicyKeyStrategy::WorkflowState,
            tiers: BTreeMap::new(),
            routes: BTreeMap::new(),
            default_tier: None,
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
            adequacy: AdequacyConfig::default(),
        }
    }
}

impl PolicyDefinition {
    pub fn as_table_config(&self) -> PolicyTableConfig {
        PolicyTableConfig {
            key_strategy: self.key_strategy,
            tiers: self
                .tiers
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            fingerprints: self
                .routes
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            default_tier: self.default_tier.clone(),
            tool_use_tier: self.tool_use_tier.clone(),
            tool_safe_tiers: self.tool_safe_tiers.clone(),
            adequacy: self.adequacy.clone(),
        }
    }
}

/// Parsed lock plus its runtime-computed identity.
#[derive(Debug, Clone)]
pub struct LoadedPolicyLock {
    pub path: PathBuf,
    pub digest: String,
    pub document: PolicyLock,
}

/// Resolve a configured policy path against the file that supplied the config.
/// Zero-config has no source directory and therefore never auto-discovers a
/// policy lock.
pub fn resolve_path(config: &Config, config_path: Option<&Path>) -> Option<PathBuf> {
    let config_path = config_path?;
    let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
    let configured = config.policy.path.as_deref();
    Some(match configured {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => parent.join(path),
        None => parent.join(DEFAULT_POLICY_LOCK_FILENAME),
    })
}

pub fn bound_policy_names(config: &Config) -> BTreeSet<String> {
    config
        .presets
        .values()
        .filter_map(|preset| preset.policy.clone())
        .collect()
}

/// Load and cross-validate the lock used by `config`. A missing default lock is
/// a no-op when no preset binds a policy; an explicit path or binding makes it
/// required.
pub async fn load_for_config(
    config: &Config,
    config_path: Option<&Path>,
) -> Result<Option<LoadedPolicyLock>> {
    let required = bound_policy_names(config);
    let explicit_path = config.policy.path.is_some();
    let Some(path) = resolve_path(config, config_path) else {
        if required.is_empty() {
            return Ok(None);
        }
        anyhow::bail!(
            "preset policy bindings require a file-backed bitrouter.yaml and policy-lock.yaml"
        );
    };
    if !path.is_file() {
        if required.is_empty() && !explicit_path {
            return Ok(None);
        }
        anyhow::bail!("policy lock '{}' does not exist", path.display());
    }
    let loaded = load(&path).await?;
    validate_for_config(config, &loaded.document)?;
    Ok(Some(loaded))
}

pub async fn load(path: &Path) -> Result<LoadedPolicyLock> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading policy lock {}", path.display()))?;
    let document: PolicyLock = serde_saphyr::from_str(&raw)
        .with_context(|| format!("parsing policy lock {}", path.display()))?;
    validate_document(&document)?;
    let digest = semantic_digest(&document)?;
    Ok(LoadedPolicyLock {
        path: path.to_path_buf(),
        digest,
        document,
    })
}

pub fn validate_document(document: &PolicyLock) -> Result<()> {
    if document.lockfile_version != POLICY_LOCKFILE_VERSION {
        anyhow::bail!(
            "unsupported policy lockfileVersion {}; expected {}",
            document.lockfile_version,
            POLICY_LOCKFILE_VERSION
        );
    }
    for (name, policy) in &document.policies {
        validate_name(name)?;
        let config = policy.as_table_config();
        validate_policy_table_config(&config)
            .map_err(anyhow::Error::from)
            .with_context(|| format!("validating policy '{name}'"))?;
        if config.tiers.is_empty() {
            anyhow::bail!("policy '{name}' must define at least one tier");
        }
        let mut model_tiers = BTreeMap::new();
        for (tier, model) in &config.tiers {
            if model.trim().is_empty() {
                anyhow::bail!("policy '{name}' tier '{tier}' must use a non-empty model id");
            }
            if model.starts_with('@') {
                anyhow::bail!(
                    "policy '{name}' tier target '{model}' cannot reference another preset"
                );
            }
            if let Some(previous) = model_tiers.insert(model, tier) {
                anyhow::bail!(
                    "policy '{name}' tiers '{previous}' and '{tier}' use the same model '{model}'"
                );
            }
        }
    }
    Ok(())
}

pub fn validate_for_config(config: &Config, document: &PolicyLock) -> Result<()> {
    validate_document(document)?;
    for (preset_name, preset) in &config.presets {
        let Some(policy_name) = &preset.policy else {
            continue;
        };
        if preset
            .model
            .as_deref()
            .is_none_or(|model| model.trim().is_empty())
        {
            anyhow::bail!(
                "preset '@{preset_name}' must define a base model before binding policy '{policy_name}'"
            );
        }
        if !document.policies.contains_key(policy_name) {
            anyhow::bail!("preset '@{preset_name}' references missing policy '{policy_name}'");
        }
    }
    Ok(())
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('.')
        || name.chars().any(|c| {
            c.is_whitespace() || !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        })
    {
        anyhow::bail!(
            "invalid policy name '{name}' (use letters, digits, '.', '_' or '-', without a leading '.')"
        );
    }
    Ok(())
}

/// Stable semantic identity. YAML comments and map presentation do not affect
/// this digest because every map in the lock model is ordered.
pub fn semantic_digest(document: &PolicyLock) -> Result<String> {
    let canonical = serde_json::to_vec(document).context("serializing canonical policy lock")?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(canonical))))
}

pub fn deterministic_yaml(document: &PolicyLock) -> Result<String> {
    validate_document(document)?;
    let mut rendered = serde_saphyr::to_string(document).context("serializing policy lock")?;
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    Ok(rendered)
}

/// One deterministic database-to-lock projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvolutionChange {
    pub policy: String,
    pub request_key: String,
    pub tier: String,
}

/// Candidate lock produced from positive adequacy evidence.
#[derive(Debug, Clone)]
pub struct EvolutionResult {
    pub document: PolicyLock,
    pub changes: Vec<EvolutionChange>,
}

/// Materialize qualified positive exploration locks into the current policy
/// artifact. Only namespaced rows are considered, and an explicit route in the
/// file is never overwritten by the optimizer.
pub fn evolve_document(
    current: &PolicyLock,
    exploration: &[PersistedExplorationState],
    semantic_successes: &BTreeMap<String, u32>,
) -> Result<EvolutionResult> {
    validate_document(current)?;
    let mut document = current.clone();
    let mut rows = exploration.iter().collect::<Vec<_>>();
    rows.sort_by(|left, right| left.fingerprint.cmp(&right.fingerprint));
    let mut changes = Vec::new();

    for row in rows {
        let Some((policy_name, request_key)) = row.fingerprint.split_once('\0') else {
            continue;
        };
        let Some(policy) = document.policies.get_mut(policy_name) else {
            continue;
        };
        if !policy.adequacy.enabled || !policy.adequacy.explore_enabled {
            continue;
        }
        let Some(explore_tier) = policy.adequacy.explore_tier.clone() else {
            continue;
        };
        let opening_minimum = if is_opening_key(request_key) {
            policy.adequacy.min_semantic_successes_for_opening
        } else {
            0
        };
        let minimum = policy
            .adequacy
            .min_semantic_successes_for_lock
            .max(opening_minimum);
        let observed = semantic_successes
            .get(&row.fingerprint)
            .copied()
            .unwrap_or_default();
        let qualified = row.locked && observed >= minimum;
        match policy.routes.get(request_key) {
            None if qualified => {
                policy
                    .routes
                    .insert(request_key.to_string(), explore_tier.clone());
                changes.push(EvolutionChange {
                    policy: policy_name.to_string(),
                    request_key: request_key.to_string(),
                    tier: explore_tier,
                });
            }
            Some(_) | None => {}
        }
    }

    validate_document(&document)?;
    Ok(EvolutionResult { document, changes })
}

/// Turn an evolved candidate into a deployment artifact. Existing routes and
/// hard-failure adequacy protection remain active; only future exploration is
/// disabled so holdout and production routing are deterministic.
pub fn freeze_document(mut document: PolicyLock) -> PolicyLock {
    for policy in document.policies.values_mut() {
        policy.adequacy.explore_enabled = false;
    }
    document
}

/// Atomically publish a candidate without permitting it to replace the lock
/// currently selected by `bitrouter.yaml`.
pub fn export_candidate_file(
    active_lock_path: &Path,
    candidate_path: &Path,
    document: &PolicyLock,
) -> Result<String> {
    let active = resolved_file_location(active_lock_path)?;
    let candidate = resolved_file_location(candidate_path)?;
    if active == candidate {
        anyhow::bail!(
            "candidate output '{}' is the active policy lock; choose a separate path",
            candidate_path.display()
        );
    }
    write_atomic(candidate_path, None, document)
}

fn resolved_file_location(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return std::fs::canonicalize(path)
            .with_context(|| format!("resolving policy path {}", path.display()));
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolving current directory")?
            .join(path)
    };
    let file_name = absolute
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("policy output must name a file: {}", path.display()))?;
    let parent = absolute.parent().unwrap_or_else(|| Path::new("."));
    let resolved_parent = if parent.exists() {
        std::fs::canonicalize(parent)
            .with_context(|| format!("resolving policy output parent {}", parent.display()))?
    } else {
        parent.to_path_buf()
    };
    Ok(resolved_parent.join(file_name))
}

fn is_opening_key(request_key: &str) -> bool {
    request_key == "opening" || request_key.split('|').nth(2) == Some("opening")
}

/// Bind an existing preset to a routing policy and set the optimizer's
/// publication permission without reserializing the rest of `bitrouter.yaml`.
/// This keeps comments and operator formatting intact.
pub fn edit_config_policy(
    raw: &str,
    preset: &str,
    policy: &str,
    writeback: PolicyWriteback,
) -> Result<String> {
    edit_config_policy_with_model(raw, preset, policy, None, writeback)
}

/// Change only the optimizer publication mode in `bitrouter.yaml`.
pub fn edit_config_writeback(raw: &str, writeback: PolicyWriteback) -> Result<String> {
    bitrouter_sdk::config::parse(raw).context("parsing bitrouter.yaml")?;
    let mut lines = source_lines(raw);
    set_policy_writeback(&mut lines, writeback)?;
    let edited = render_source_lines(lines, raw.ends_with('\n'));
    bitrouter_sdk::config::parse(&edited).context("validating edited bitrouter.yaml")?;
    Ok(edited)
}

/// Variant used by `policy init`, which may create the preset when a strong
/// base model is supplied.
pub fn edit_config_policy_with_model(
    raw: &str,
    preset: &str,
    policy: &str,
    model: Option<&str>,
    writeback: PolicyWriteback,
) -> Result<String> {
    validate_name(preset).context("validating preset name")?;
    validate_name(policy)?;
    let parsed = bitrouter_sdk::config::parse(raw).context("parsing bitrouter.yaml")?;
    if let Some(existing) = parsed
        .presets
        .get(preset)
        .and_then(|item| item.policy.as_deref())
        && existing != policy
    {
        anyhow::bail!("preset '@{preset}' already binds policy '{existing}'");
    }
    if !parsed.presets.contains_key(preset) && model.is_none() {
        anyhow::bail!("preset '@{preset}' does not exist; provide its strong base model");
    }

    let mut lines = source_lines(raw);
    set_policy_writeback(&mut lines, writeback)?;
    bind_preset(&mut lines, preset, policy, model)?;
    let edited = render_source_lines(lines, raw.ends_with('\n'));
    let checked =
        bitrouter_sdk::config::parse(&edited).context("validating edited bitrouter.yaml")?;
    if checked
        .presets
        .get(preset)
        .and_then(|item| item.policy.as_deref())
        != Some(policy)
    {
        anyhow::bail!("edited config did not bind preset '@{preset}' to policy '{policy}'");
    }
    Ok(edited)
}

fn source_lines(raw: &str) -> Vec<String> {
    raw.lines().map(ToString::to_string).collect()
}

fn render_source_lines(lines: Vec<String>, had_trailing_newline: bool) -> String {
    let mut rendered = lines.join("\n");
    if had_trailing_newline || !rendered.is_empty() {
        rendered.push('\n');
    }
    rendered
}

fn set_policy_writeback(lines: &mut Vec<String>, writeback: PolicyWriteback) -> Result<()> {
    let value = match writeback {
        PolicyWriteback::Locked => "locked",
        PolicyWriteback::Evolve => "evolve",
    };
    if let Some((start, end)) = block_range(lines, "policy", 0) {
        require_block_header(&lines[start], "policy")?;
        if let Some(index) = child_key(lines, start + 1, end, "writeback", 2) {
            lines[index] = format!("  writeback: {value}");
        } else {
            lines.insert(end, format!("  writeback: {value}"));
        }
    } else {
        if !lines.is_empty() && !lines.last().is_some_and(|line| line.is_empty()) {
            lines.push(String::new());
        }
        lines.push("policy:".into());
        lines.push(format!("  writeback: {value}"));
    }
    Ok(())
}

fn bind_preset(
    lines: &mut Vec<String>,
    preset: &str,
    policy: &str,
    model: Option<&str>,
) -> Result<()> {
    let presets = block_range(lines, "presets", 0);
    if let Some((start, end)) = presets {
        require_block_header(&lines[start], "presets")?;
        if let Some(preset_start) = child_key(lines, start + 1, end, preset, 2) {
            require_block_header(&lines[preset_start], preset)?;
            let preset_end = nested_block_end(lines, preset_start, end, 2);
            if let Some(index) = child_key(lines, preset_start + 1, preset_end, "policy", 4) {
                lines[index] = format!("    policy: {policy}");
            } else {
                lines.insert(preset_end, format!("    policy: {policy}"));
            }
            return Ok(());
        }
        lines.insert(end, format!("  {preset}:"));
        let mut offset = 1;
        if let Some(model) = model {
            lines.insert(end + offset, format!("    model: {model}"));
            offset += 1;
        }
        lines.insert(end + offset, format!("    policy: {policy}"));
        return Ok(());
    }

    if !lines.is_empty() && !lines.last().is_some_and(|line| line.is_empty()) {
        lines.push(String::new());
    }
    lines.push("presets:".into());
    lines.push(format!("  {preset}:"));
    if let Some(model) = model {
        lines.push(format!("    model: {model}"));
    }
    lines.push(format!("    policy: {policy}"));
    Ok(())
}

fn require_block_header(line: &str, key: &str) -> Result<()> {
    let Some((_, tail)) = line.trim_start().split_once(':') else {
        anyhow::bail!("expected YAML block for '{key}'");
    };
    if !tail.trim().is_empty() {
        anyhow::bail!("inline YAML for '{key}' cannot be edited safely; expand it to a block");
    }
    Ok(())
}

fn block_range(lines: &[String], key: &str, indent: usize) -> Option<(usize, usize)> {
    let start = lines
        .iter()
        .position(|line| line_key(line, indent) == Some(key))?;
    Some((start, nested_block_end(lines, start, lines.len(), indent)))
}

fn nested_block_end(lines: &[String], start: usize, limit: usize, indent: usize) -> usize {
    (start + 1..limit)
        .find(|&index| {
            let line = lines[index].as_str();
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#') && leading_spaces(line) <= indent
        })
        .unwrap_or(limit)
}

fn child_key(
    lines: &[String],
    start: usize,
    end: usize,
    key: &str,
    indent: usize,
) -> Option<usize> {
    (start..end).find(|&index| line_key(&lines[index], indent) == Some(key))
}

fn line_key(line: &str, indent: usize) -> Option<&str> {
    if leading_spaces(line) != indent {
        return None;
    }
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return None;
    }
    trimmed.split_once(':').map(|(key, _)| key.trim())
}

fn leading_spaces(line: &str) -> usize {
    line.bytes().take_while(|byte| *byte == b' ').count()
}

#[derive(Debug, Clone)]
pub struct PolicyFileUpdate {
    pub path: PathBuf,
    pub digest: String,
    pub document: PolicyLock,
    pub changes: Vec<String>,
}

/// Create one named adaptive policy and bind it to a preset. The candidate
/// main config and lock are fully cross-validated before either file is
/// published. Optimizer writeback starts locked.
pub async fn initialize_files(
    config_path: &Path,
    policy_name: &str,
    preset_name: &str,
    strong_model: Option<&str>,
    economy_model: &str,
) -> Result<PolicyFileUpdate> {
    validate_name(policy_name)?;
    validate_name(preset_name).context("validating preset name")?;
    validate_tier_model(economy_model, "economy")?;
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config = bitrouter_sdk::config::parse(&raw).context("parsing bitrouter.yaml")?;
    let strong_model = strong_model
        .map(ToString::to_string)
        .or_else(|| {
            config
                .presets
                .get(preset_name)
                .and_then(|preset| preset.model.clone())
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "preset '@{preset_name}' has no model; pass --strong <model> to create it"
            )
        })?;
    validate_tier_model(&strong_model, "strong")?;
    if strong_model == economy_model {
        anyhow::bail!("strong and economy tiers must use different models");
    }

    let lock_path = resolve_path(&config, Some(config_path))
        .ok_or_else(|| anyhow::anyhow!("cannot resolve policy lock path"))?;
    if lock_path == config_path {
        anyhow::bail!("policy.path cannot point to bitrouter.yaml itself");
    }
    let (mut document, expected_digest) = if lock_path.is_file() {
        let loaded = load(&lock_path).await?;
        (loaded.document, Some(loaded.digest))
    } else {
        (PolicyLock::default(), None)
    };
    if document.policies.contains_key(policy_name) {
        anyhow::bail!(
            "policy '{policy_name}' already exists in {}",
            lock_path.display()
        );
    }

    let adequacy = AdequacyConfig {
        enabled: true,
        escalation_tier: Some("strong".into()),
        explore_enabled: true,
        explore_tier: Some("economy".into()),
        min_semantic_successes_for_lock: 1,
        ..AdequacyConfig::default()
    };
    document.policies.insert(
        policy_name.to_string(),
        PolicyDefinition {
            tiers: BTreeMap::from([
                ("economy".into(), economy_model.to_string()),
                ("strong".into(), strong_model.clone()),
            ]),
            default_tier: Some("strong".into()),
            tool_use_tier: Some("strong".into()),
            tool_safe_tiers: vec!["strong".into()],
            adequacy,
            ..PolicyDefinition::default()
        },
    );
    let preset_model = (!config.presets.contains_key(preset_name)).then_some(strong_model.as_str());
    let edited_config = edit_config_policy_with_model(
        &raw,
        preset_name,
        policy_name,
        preset_model,
        PolicyWriteback::Locked,
    )?;
    let candidate_config =
        bitrouter_sdk::config::parse(&edited_config).context("validating candidate config")?;
    validate_for_config(&candidate_config, &document)?;

    let digest = write_atomic(&lock_path, expected_digest.as_deref(), &document)?;
    write_text_atomic(config_path, &raw, &edited_config)?;
    Ok(PolicyFileUpdate {
        path: lock_path,
        digest,
        document,
        changes: vec![
            format!("created policy '{policy_name}'"),
            format!("bound preset '@{preset_name}'"),
        ],
    })
}

/// Read adequacy evidence and project it into a candidate policy lock. Dry-run
/// is the default. Applying is permitted only while `policy.writeback: evolve`.
pub async fn evolve_files(config_path: &Path, apply: bool) -> Result<PolicyFileUpdate> {
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config = bitrouter_sdk::config::parse(&raw).context("parsing bitrouter.yaml")?;
    if apply && config.policy.writeback == PolicyWriteback::Locked {
        anyhow::bail!(
            "policy writeback is locked; run `bitrouter policy unlock` before `policy evolve --apply`"
        );
    }
    let loaded = load_for_config(&config, Some(config_path))
        .await?
        .ok_or_else(|| anyhow::anyhow!("no policy lock is configured"))?;
    let database_url = readonly_database_url(&config.database.url, config_path)?;
    let db = crate::db::connect(&database_url)
        .await
        .map_err(anyhow::Error::from)?;
    let store = AdequacyStore::new(db);
    let exploration = store
        .load_exploration_all()
        .await
        .map_err(anyhow::Error::from)?;
    let semantic = store
        .load_semantic_success_counts()
        .await
        .map_err(anyhow::Error::from)?;
    let evolved = evolve_document(&loaded.document, &exploration, &semantic)?;
    let digest = semantic_digest(&evolved.document)?;
    if apply && !evolved.changes.is_empty() {
        write_atomic(&loaded.path, Some(&loaded.digest), &evolved.document)?;
    }
    let changes = evolved
        .changes
        .iter()
        .map(|change| {
            format!(
                "{}: {} -> {}",
                change.policy, change.request_key, change.tier
            )
        })
        .collect();
    Ok(PolicyFileUpdate {
        path: loaded.path,
        digest,
        document: evolved.document,
        changes,
    })
}

/// Explicit operator command for changing optimizer publication permission.
/// `locked` governs programmatic writes to the lock, not this main-config edit.
pub async fn set_writeback_file(config_path: &Path, writeback: PolicyWriteback) -> Result<()> {
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("reading {}", config_path.display()))?;
    let edited = edit_config_writeback(&raw, writeback)?;
    write_text_atomic(config_path, &raw, &edited)
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

fn validate_tier_model(model: &str, tier: &str) -> Result<()> {
    if model.trim().is_empty() || model.starts_with('@') {
        anyhow::bail!("{tier} model must be a non-empty model id, not a preset");
    }
    Ok(())
}

/// Publish a main-config edit only if the file still matches the caller's
/// snapshot. File permissions are retained across the atomic replacement.
pub fn write_text_atomic(path: &Path, expected: &str, updated: &str) -> Result<()> {
    let current = std::fs::read_to_string(path)
        .with_context(|| format!("reading current config {}", path.display()))?;
    if current != expected {
        anyhow::bail!(
            "config changed since it was loaded; refusing to overwrite {}",
            path.display()
        );
    }
    let permissions = std::fs::metadata(path)
        .with_context(|| format!("reading permissions for {}", path.display()))?
        .permissions();
    let tmp = sibling_temp_path(path);
    let result = (|| -> Result<()> {
        let mut file = std::fs::File::create(&tmp)
            .with_context(|| format!("creating config temp file {}", tmp.display()))?;
        std::fs::set_permissions(&tmp, permissions)
            .with_context(|| format!("preserving permissions on {}", tmp.display()))?;
        file.write_all(updated.as_bytes())
            .with_context(|| format!("writing config temp file {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing config temp file {}", tmp.display()))?;
        #[cfg(windows)]
        if path.exists() {
            std::fs::remove_file(path)
                .with_context(|| format!("replacing config {}", path.display()))?;
        }
        std::fs::rename(&tmp, path)
            .with_context(|| format!("publishing config {}", path.display()))?;
        sync_parent(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn sibling_temp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("policy-lock.yaml");
    path.with_file_name(format!(".{file_name}.tmp-{}", uuid::Uuid::new_v4()))
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)
            .with_context(|| format!("opening parent directory {}", parent.display()))?
            .sync_all()
            .with_context(|| format!("syncing parent directory {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<()> {
    Ok(())
}

/// Optimistic semantic-digest check followed by publication through a
/// same-directory temp file and atomic rename. A detected human or Git update
/// made after the optimizer loaded its snapshot is never overwritten.
pub fn write_atomic(
    path: &Path,
    expected_digest: Option<&str>,
    document: &PolicyLock,
) -> Result<String> {
    if let Some(expected) = expected_digest {
        let current = std::fs::read_to_string(path)
            .with_context(|| format!("reading current policy lock {}", path.display()))?;
        let parsed: PolicyLock = serde_saphyr::from_str(&current)
            .with_context(|| format!("parsing current policy lock {}", path.display()))?;
        let actual = semantic_digest(&parsed)?;
        if actual != expected {
            anyhow::bail!(
                "policy lock changed since it was loaded (expected {expected}, found {actual}); refusing to overwrite"
            );
        }
    }
    let rendered = deterministic_yaml(document)?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating policy directory {}", parent.display()))?;
    }
    let permissions = std::fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    let tmp = sibling_temp_path(path);
    let result = (|| -> Result<()> {
        let mut file = std::fs::File::create(&tmp)
            .with_context(|| format!("creating policy temp file {}", tmp.display()))?;
        if let Some(permissions) = permissions {
            std::fs::set_permissions(&tmp, permissions)
                .with_context(|| format!("preserving permissions on {}", tmp.display()))?;
        }
        file.write_all(rendered.as_bytes())
            .with_context(|| format!("writing policy temp file {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing policy temp file {}", tmp.display()))?;
        #[cfg(windows)]
        if path.exists() {
            std::fs::remove_file(path)
                .with_context(|| format!("replacing policy lock {}", path.display()))?;
        }
        std::fs::rename(&tmp, path)
            .with_context(|| format!("publishing policy lock {}", path.display()))?;
        sync_parent(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    semantic_digest(document)
}

#[derive(Default)]
struct PolicySnapshot {
    path: Option<PathBuf>,
    digest: Option<String>,
    routers: BTreeMap<String, Arc<PolicyTableRouter>>,
}

/// Fully built policy candidate that has not yet replaced the live snapshot.
pub(crate) struct PreparedPolicySnapshot(Arc<PolicySnapshot>);

/// Live last-known-good policy registry shared by the model selector and
/// daemon reloader. A request clones one snapshot before deciding, so reloads
/// never mix old and new policy state inside one request.
pub struct PolicyRuntime {
    snapshot: RwLock<Arc<PolicySnapshot>>,
    db: DatabaseConnection,
    pending: Arc<PendingAdequacyStore>,
    decision_recorder: Option<Arc<PolicyDecisionJsonlRecorder>>,
}

impl PolicyRuntime {
    pub(crate) async fn new(
        config: &Config,
        config_path: Option<&Path>,
        db: DatabaseConnection,
        pending: Arc<PendingAdequacyStore>,
        decision_recorder: Option<Arc<PolicyDecisionJsonlRecorder>>,
    ) -> Result<Arc<Self>> {
        let runtime = Arc::new(Self {
            snapshot: RwLock::new(Arc::new(PolicySnapshot::default())),
            db,
            pending,
            decision_recorder,
        });
        runtime.reload_for_config(config, config_path).await?;
        Ok(runtime)
    }

    pub async fn reload_for_config(
        &self,
        config: &Config,
        config_path: Option<&Path>,
    ) -> Result<()> {
        let prepared = self.prepare_for_config(config, config_path).await?;
        self.commit(prepared);
        Ok(())
    }

    pub(crate) async fn prepare_for_config(
        &self,
        config: &Config,
        config_path: Option<&Path>,
    ) -> Result<PreparedPolicySnapshot> {
        let loaded = load_for_config(config, config_path).await?;
        let mut routers = BTreeMap::new();
        if let Some(loaded) = &loaded {
            for (name, definition) in &loaded.document.policies {
                let table_config = definition.as_table_config();
                let table = PolicyTable::from_config(&table_config)
                    .ok_or_else(|| anyhow::anyhow!("policy '{name}' is inert"))?;
                let ledger = if table_config.adequacy.enabled {
                    Some(Arc::new(
                        AdequacyLedger::load(
                            &table_config.adequacy,
                            AdequacyStore::new(self.db.clone()),
                        )
                        .await?,
                    ))
                } else {
                    None
                };
                let mut router = PolicyTableRouter::new(table, ledger.clone())
                    .with_state_namespace(name.clone());
                if let Some(recorder) = &self.decision_recorder {
                    router = router.with_shared_decision_recorder(recorder.clone());
                }
                if ledger.is_some() {
                    router = router.with_pending_adequacy_store(self.pending.clone());
                }
                routers.insert(name.clone(), Arc::new(router));
            }
        }
        Ok(PreparedPolicySnapshot(Arc::new(PolicySnapshot {
            path: loaded.as_ref().map(|lock| lock.path.clone()),
            digest: loaded.as_ref().map(|lock| lock.digest.clone()),
            routers,
        })))
    }

    pub(crate) fn commit(&self, prepared: PreparedPolicySnapshot) {
        *self
            .snapshot
            .write()
            .unwrap_or_else(PoisonError::into_inner) = prepared.0;
    }

    pub fn status(&self, writeback: PolicyWriteback) -> PolicyRuntimeStatus {
        let snapshot = self
            .snapshot
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        PolicyRuntimeStatus {
            path: snapshot.path.clone(),
            digest: snapshot.digest.clone(),
            policies: snapshot.routers.keys().cloned().collect(),
            writeback,
        }
    }
}

impl ModelSelector for PolicyRuntime {
    fn select(&self, policy: &str, ctx: &mut PipelineContext) -> bitrouter_sdk::Result<()> {
        let snapshot = self
            .snapshot
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let router = snapshot.routers.get(policy).ok_or_else(|| {
            bitrouter_sdk::BitrouterError::bad_request(format!(
                "preset references unavailable policy '{policy}'"
            ))
        })?;
        let input_model = ctx.model().to_string();
        let selected = router.select_for_bound_policy(&input_model, ctx.prompt(), ctx.headers());
        if let Some(model) = selected {
            ctx.set_model(model);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PolicyRuntimeStatus {
    pub path: Option<PathBuf>,
    pub digest: Option<String>,
    pub policies: Vec<String>,
    pub writeback: PolicyWriteback,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition() -> PolicyDefinition {
        PolicyDefinition {
            tiers: BTreeMap::from([
                ("economy".into(), "vendor:economy".into()),
                ("strong".into(), "vendor:strong".into()),
            ]),
            routes: BTreeMap::from([("opening".into(), "strong".into())]),
            default_tier: Some("strong".into()),
            tool_use_tier: Some("strong".into()),
            tool_safe_tiers: vec!["strong".into()],
            ..Default::default()
        }
    }

    #[test]
    fn deterministic_round_trip_and_digest() {
        let lock = PolicyLock {
            lockfile_version: 1,
            policies: BTreeMap::from([("coding".into(), definition())]),
        };
        let first = deterministic_yaml(&lock).unwrap();
        let parsed: PolicyLock = serde_saphyr::from_str(&first).unwrap();
        let second = deterministic_yaml(&parsed).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            semantic_digest(&lock).unwrap(),
            semantic_digest(&parsed).unwrap()
        );
        assert!(!first.contains("generation"));
        assert!(!first.contains("parent"));
    }

    #[test]
    fn freezing_disables_exploration_but_preserves_adequacy() {
        let mut policy = definition();
        policy.adequacy.enabled = true;
        policy.adequacy.explore_enabled = true;
        policy.adequacy.explore_tier = Some("economy".into());
        let lock = PolicyLock {
            lockfile_version: 1,
            policies: BTreeMap::from([("coding".into(), policy)]),
        };

        let frozen = freeze_document(lock);

        assert!(frozen.policies["coding"].adequacy.enabled);
        assert!(!frozen.policies["coding"].adequacy.explore_enabled);
        assert_eq!(
            frozen.policies["coding"].adequacy.explore_tier.as_deref(),
            Some("economy")
        );
    }

    #[test]
    fn candidate_export_refuses_to_replace_the_active_lock() {
        let dir = tempfile::tempdir().unwrap();
        let active = dir.path().join("policy-lock.yaml");
        let lock = PolicyLock {
            lockfile_version: 1,
            policies: BTreeMap::from([("coding".into(), definition())]),
        };
        write_atomic(&active, None, &lock).unwrap();
        let before = std::fs::read(&active).unwrap();

        let error = export_candidate_file(&active, &active, &lock).unwrap_err();

        assert!(error.to_string().contains("active policy lock"));
        assert_eq!(std::fs::read(&active).unwrap(), before);
    }

    #[test]
    fn independent_candidate_exports_are_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let active = dir.path().join("policy-lock.yaml");
        let first = dir.path().join("candidate-a.yaml");
        let second = dir.path().join("candidate-b.yaml");
        let lock = PolicyLock {
            lockfile_version: 1,
            policies: BTreeMap::from([("coding".into(), definition())]),
        };
        write_atomic(&active, None, &lock).unwrap();

        let first_digest = export_candidate_file(&active, &first, &lock).unwrap();
        let second_digest = export_candidate_file(&active, &second, &lock).unwrap();

        assert_eq!(
            std::fs::read(&first).unwrap(),
            std::fs::read(&second).unwrap()
        );
        assert_eq!(first_digest, second_digest);
    }

    #[test]
    fn validation_rejects_empty_or_duplicate_tier_models() {
        let mut empty = definition();
        empty.tiers.insert("strong".into(), "   ".into());
        let error = validate_document(&PolicyLock {
            lockfile_version: 1,
            policies: BTreeMap::from([("coding".into(), empty)]),
        })
        .unwrap_err();
        assert!(error.to_string().contains("non-empty model id"));

        let mut duplicate = definition();
        duplicate
            .tiers
            .insert("economy".into(), "vendor:strong".into());
        let error = validate_document(&PolicyLock {
            lockfile_version: 1,
            policies: BTreeMap::from([("coding".into(), duplicate)]),
        })
        .unwrap_err();
        assert!(error.to_string().contains("same model"));
    }

    #[test]
    fn validation_rejects_a_bound_preset_without_a_base_model() {
        let mut config = Config::default();
        config.presets.insert(
            "coding".into(),
            bitrouter_sdk::config::PresetConfig {
                policy: Some("coding".into()),
                ..Default::default()
            },
        );
        let error = validate_for_config(
            &config,
            &PolicyLock {
                lockfile_version: 1,
                policies: BTreeMap::from([("coding".into(), definition())]),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("must define a base model"));
    }

    #[cfg(unix)]
    #[test]
    fn atomic_policy_update_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy-lock.yaml");
        let mut lock = PolicyLock {
            lockfile_version: 1,
            policies: BTreeMap::from([("coding".into(), definition())]),
        };
        let digest = write_atomic(&path, None, &lock).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        lock.policies
            .get_mut("coding")
            .unwrap()
            .routes
            .insert("midstream".into(), "strong".into());

        write_atomic(&path, Some(&digest), &lock).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn relative_path_uses_config_directory() {
        let mut config = Config::default();
        assert_eq!(
            resolve_path(&config, Some(Path::new("/tmp/team/bitrouter.yaml"))).unwrap(),
            Path::new("/tmp/team/policy-lock.yaml")
        );
        config.policy.path = Some(PathBuf::from("routing/policy.yaml"));
        assert_eq!(
            resolve_path(&config, Some(Path::new("/tmp/team/bitrouter.yaml"))).unwrap(),
            Path::new("/tmp/team/routing/policy.yaml")
        );
    }

    #[test]
    fn config_edits_preserve_comments_and_bind_the_preset() {
        let raw = r#"# team routing
providers: {}
presets:
  coding:
    # keep this operator note
    model: anthropic/claude-opus-4.8
"#;

        let edited =
            edit_config_policy(raw, "coding", "terminal-bench", PolicyWriteback::Locked).unwrap();

        assert!(edited.contains("# team routing"));
        assert!(edited.contains("# keep this operator note"));
        assert!(edited.contains("    policy: terminal-bench"));
        assert!(edited.contains("policy:\n  writeback: locked"));
        let parsed = bitrouter_sdk::config::parse(&edited).unwrap();
        assert_eq!(
            parsed.presets["coding"].policy.as_deref(),
            Some("terminal-bench")
        );
        assert_eq!(parsed.policy.writeback, PolicyWriteback::Locked);
    }

    #[test]
    fn config_edit_refuses_to_replace_a_different_binding() {
        let raw = r#"presets:
  coding:
    model: anthropic/claude-opus-4.8
    policy: production
"#;

        let error =
            edit_config_policy(raw, "coding", "experiment", PolicyWriteback::Locked).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("already binds policy 'production'")
        );
    }

    #[test]
    fn evolution_only_materializes_qualified_namespaced_locks() {
        use crate::adequacy::store::PersistedExplorationState;

        let mut policy = definition();
        policy.adequacy.enabled = true;
        policy.adequacy.explore_enabled = true;
        policy.adequacy.explore_tier = Some("economy".into());
        policy.adequacy.min_semantic_successes_for_lock = 2;
        let lock = PolicyLock {
            lockfile_version: 1,
            policies: BTreeMap::from([("coding".into(), policy), ("other".into(), definition())]),
        };
        let rows = vec![
            PersistedExplorationState {
                fingerprint: "coding\0codex|responses|tool_followup|-|-|exec_command".into(),
                observed: 8,
                adequate_trials: 4,
                locked: true,
            },
            PersistedExplorationState {
                fingerprint: "coding\0opening".into(),
                observed: 8,
                adequate_trials: 4,
                locked: true,
            },
            PersistedExplorationState {
                fingerprint: "other\0must-not-leak".into(),
                observed: 8,
                adequate_trials: 4,
                locked: true,
            },
        ];
        let semantic = BTreeMap::from([
            (
                "coding\0codex|responses|tool_followup|-|-|exec_command".into(),
                2,
            ),
            ("coding\0opening".into(), 1),
        ]);

        let evolved = evolve_document(&lock, &rows, &semantic).unwrap();

        assert_eq!(evolved.changes.len(), 1);
        assert_eq!(evolved.changes[0].policy, "coding");
        assert_eq!(evolved.changes[0].tier, "economy");
        assert_eq!(
            evolved.document.policies["coding"].routes["codex|responses|tool_followup|-|-|exec_command"],
            "economy"
        );
        assert_eq!(
            evolved.document.policies["coding"].routes["opening"],
            "strong"
        );
        assert!(
            !evolved.document.policies["coding"]
                .routes
                .contains_key("must-not-leak")
        );
    }

    #[test]
    fn evolution_never_removes_an_existing_operator_route() {
        let request_key = "codex|responses|tool_followup|-|-|exec_command";
        let mut policy = definition();
        policy.adequacy.enabled = true;
        policy.adequacy.explore_enabled = true;
        policy.adequacy.explore_tier = Some("economy".into());
        policy.routes.insert(request_key.into(), "economy".into());
        let lock = PolicyLock {
            lockfile_version: 1,
            policies: BTreeMap::from([("coding".into(), policy)]),
        };
        let rows = vec![PersistedExplorationState {
            fingerprint: format!("coding\0{request_key}"),
            observed: 9,
            adequate_trials: 0,
            locked: false,
        }];

        let evolved = evolve_document(&lock, &rows, &BTreeMap::new()).unwrap();

        assert!(evolved.changes.is_empty());
        assert_eq!(
            evolved.document.policies["coding"].routes[request_key],
            "economy"
        );
    }

    #[tokio::test]
    async fn initialize_writes_a_locked_policy_and_preserves_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("bitrouter.yaml");
        tokio::fs::write(
            &config_path,
            r#"# owned by the routing team
presets:
  coding:
    model: anthropic/claude-opus-4.8
"#,
        )
        .await
        .unwrap();

        let update = initialize_files(
            &config_path,
            "terminal-bench",
            "coding",
            None,
            "moonshotai/kimi-k2.7-code",
        )
        .await
        .unwrap();

        assert_eq!(update.path, dir.path().join("policy-lock.yaml"));
        let config_raw = tokio::fs::read_to_string(&config_path).await.unwrap();
        assert!(config_raw.contains("# owned by the routing team"));
        let config = bitrouter_sdk::config::parse(&config_raw).unwrap();
        assert_eq!(config.policy.writeback, PolicyWriteback::Locked);
        assert_eq!(
            config.presets["coding"].policy.as_deref(),
            Some("terminal-bench")
        );
        let loaded = load(&update.path).await.unwrap();
        let policy = &loaded.document.policies["terminal-bench"];
        assert_eq!(policy.tiers["strong"], "anthropic/claude-opus-4.8");
        assert_eq!(policy.tiers["economy"], "moonshotai/kimi-k2.7-code");
        assert_eq!(policy.default_tier.as_deref(), Some("strong"));
        assert_eq!(policy.adequacy.explore_tier.as_deref(), Some("economy"));
    }

    #[tokio::test]
    async fn locked_config_refuses_evolution_apply() {
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

        let error = evolve_files(&config_path, true).await.unwrap_err();

        assert!(error.to_string().contains("writeback is locked"));
        assert!(!dir.path().join("bitrouter.db").exists());
    }

    #[tokio::test]
    async fn locked_config_can_export_without_mutating_active_files() {
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
        let frozen = freeze_document(update.document);
        export_candidate_file(&update.path, &candidate_path, &frozen).unwrap();

        assert_eq!(tokio::fs::read(&config_path).await.unwrap(), before_config);
        assert_eq!(
            tokio::fs::read(&initialized.path).await.unwrap(),
            before_lock
        );
        let candidate = load(&candidate_path).await.unwrap();
        assert!(
            candidate.document.policies["terminal-bench"]
                .adequacy
                .enabled
        );
        assert!(
            !candidate.document.policies["terminal-bench"]
                .adequacy
                .explore_enabled
        );
    }

    #[tokio::test]
    async fn reload_swaps_valid_policy_and_keeps_last_known_good_on_error() {
        use bitrouter_sdk::caller::CallerContext;
        use bitrouter_sdk::language_model::{
            GenerationParams, Message, PipelineRequest, Prompt, Role,
        };

        fn context(model: &str) -> PipelineContext {
            let prompt = Prompt {
                model: model.into(),
                system: None,
                system_provider_metadata: Default::default(),
                messages: vec![Message::text(Role::User, "solve this")],
                tools: Vec::new(),
                params: GenerationParams::default(),
                response_format: None,
                tool_choice: None,
                stream: false,
            };
            PipelineContext::new(PipelineRequest::new(model, CallerContext::local(), prompt))
        }

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("bitrouter.yaml");
        tokio::fs::write(
            &config_path,
            r#"presets:
  coding:
    model: vendor:strong
    policy: coding
"#,
        )
        .await
        .unwrap();
        let config = bitrouter_sdk::config::load(&config_path).await.unwrap();
        let lock_path = dir.path().join("policy-lock.yaml");
        let mut reloadable = definition();
        reloadable.key_strategy = PolicyKeyStrategy::LegacyFingerprint;
        let mut lock = PolicyLock {
            lockfile_version: 1,
            policies: BTreeMap::from([("coding".into(), reloadable)]),
        };
        write_atomic(&lock_path, None, &lock).unwrap();
        let db = crate::db::connect("sqlite::memory:").await.unwrap();
        crate::db::run_migrations(&db).await.unwrap();
        let runtime = PolicyRuntime::new(
            &config,
            Some(&config_path),
            db,
            Arc::new(PendingAdequacyStore::default()),
            None,
        )
        .await
        .unwrap();

        let mut initial = context("vendor:strong");
        runtime.select("coding", &mut initial).unwrap();
        assert_eq!(initial.model(), "vendor:strong");

        lock.policies
            .get_mut("coding")
            .unwrap()
            .routes
            .insert("opening".into(), "economy".into());
        write_atomic(&lock_path, None, &lock).unwrap();
        runtime
            .reload_for_config(&config, Some(&config_path))
            .await
            .unwrap();
        let mut reloaded = context("vendor:strong");
        runtime.select("coding", &mut reloaded).unwrap();
        assert_eq!(reloaded.model(), "vendor:economy");

        tokio::fs::write(&lock_path, "lockfileVersion: invalid\n")
            .await
            .unwrap();
        assert!(
            runtime
                .reload_for_config(&config, Some(&config_path))
                .await
                .is_err()
        );
        let mut last_known_good = context("vendor:strong");
        runtime.select("coding", &mut last_known_good).unwrap();
        assert_eq!(last_known_good.model(), "vendor:economy");
    }

    #[tokio::test]
    async fn database_evolution_apply_adds_without_removing_existing_routes() {
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
            "coding",
            "coding",
            None,
            "moonshotai/kimi-k2.7-code",
        )
        .await
        .unwrap();
        set_writeback_file(&config_path, PolicyWriteback::Evolve)
            .await
            .unwrap();
        let db_path = dir.path().join("bitrouter.db");
        let db = crate::db::connect(&format!("sqlite://{}", db_path.display()))
            .await
            .unwrap();
        crate::db::run_migrations(&db).await.unwrap();
        let store = AdequacyStore::new(db);
        let request_key = "codex|responses|tool_followup|-|-|exec_command";
        let ledger_key = format!("coding\0{request_key}");
        store
            .upsert_exploration(&ledger_key, 4, 3, true)
            .await
            .unwrap();
        store
            .record_semantic_success(&ledger_key, "terminal-bench/task-a")
            .await
            .unwrap();

        let added = evolve_files(&config_path, true).await.unwrap();

        assert_eq!(added.changes.len(), 1);
        assert_eq!(
            load(&added.path).await.unwrap().document.policies["coding"].routes[request_key],
            "economy"
        );

        store
            .upsert_exploration(&ledger_key, 5, 0, false)
            .await
            .unwrap();
        store.clear_semantic_successes(&ledger_key).await.unwrap();
        let unchanged = evolve_files(&config_path, true).await.unwrap();

        assert!(unchanged.changes.is_empty());
        assert_eq!(
            load(&unchanged.path).await.unwrap().document.policies["coding"].routes[request_key],
            "economy"
        );
    }
}
