//! File-backed, preset-bound adaptive routing policies.
//!
//! `policy-lock.yaml` is the current effective policy artifact. The evidence
//! ledger can compile a candidate, but only explicit publication replaces this
//! file. Serving never reads learned database rows for semantic selection.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use anyhow::{Context, Result};
use bitrouter_sdk::config::{
    AdequacyConfig, Config, PolicyKeyStrategy, PolicyRuntimeMode, PolicyTableConfig,
    validate_policy_table_config,
};
use bitrouter_sdk::language_model::{ModelSelector, PipelineContext};
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::adequacy::store::AdequacyStore;
use crate::eval::settlement::PendingEvalDecisionStore;
use crate::policy_table_router::{PolicyTable, PolicyTableRouter};
use crate::workflow_state::decision::PolicyDecisionJsonlRecorder;

pub const DEFAULT_POLICY_LOCK_FILENAME: &str = "policy-lock.yaml";
pub const LEGACY_POLICY_LOCKFILE_VERSION: u32 = 1;
pub const POLICY_LOCKFILE_VERSION: u32 = 2;
pub const POLICY_COMPILER_ID: &str = "bitrouter-policy-compiler";
pub const POLICY_COMPILER_VERSION: u32 = 1;
const EMPTY_SHA256: &str =
    "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// The complete deterministic policy artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyLock {
    /// File-format version only.
    #[serde(rename = "lockfileVersion")]
    pub lockfile_version: u32,
    /// Reproducible compiler inputs and artifact lineage. Required for v2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<PolicyArtifact>,
    /// Named policies referenced by `presets.<name>.policy`.
    #[serde(default)]
    pub policies: BTreeMap<String, PolicyDefinition>,
    /// Decision-relevant provenance for explicit routes, nested by policy and
    /// canonical route key. Required for every v2 route.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub certificates: BTreeMap<String, BTreeMap<String, PolicyCertificate>>,
}

impl Default for PolicyLock {
    fn default() -> Self {
        Self {
            lockfile_version: POLICY_LOCKFILE_VERSION,
            artifact: Some(PolicyArtifact::empty()),
            policies: BTreeMap::new(),
            certificates: BTreeMap::new(),
        }
    }
}

impl PolicyLock {
    pub fn is_v2(&self) -> bool {
        self.lockfile_version == POLICY_LOCKFILE_VERSION
    }

    pub fn certificate(&self, policy: &str, request_key: &str) -> Option<&PolicyCertificate> {
        self.certificates
            .get(policy)
            .and_then(|entries| entries.get(request_key))
    }
}

/// Reproducible identity and evidence lineage for one compiled lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyArtifact {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_digest: Option<String>,
    pub evidence_root: String,
    /// Content-addressed admitted eval snapshot used by this compilation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_snapshot_root: Option<String>,
    pub source_snapshot_time_unix_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration: Option<LegacyMigration>,
    pub compiler: CompilerIdentity,
}

impl PolicyArtifact {
    fn empty() -> Self {
        Self {
            parent_digest: None,
            evidence_root: EMPTY_SHA256.to_string(),
            eval_snapshot_root: None,
            source_snapshot_time_unix_ms: 0,
            migration: None,
            compiler: CompilerIdentity {
                id: POLICY_COMPILER_ID.to_string(),
                version: POLICY_COMPILER_VERSION,
                config_digest: EMPTY_SHA256.to_string(),
            },
        }
    }
}

/// Digest of the sealed pre-v2 learner tables projected into this artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyMigration {
    pub legacy_adequacy_digest: String,
}

/// Compiler implementation and deterministic configuration identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompilerIdentity {
    pub id: String,
    pub version: u32,
    pub config_digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteOwner {
    Operator,
    Compiler,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CertificateSource {
    Operator,
    LegacyAdequacyV1,
    TaskNative,
    Human,
    Enterprise,
    Agentic,
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromotionVerdict {
    Retain,
    Promote,
    Demote,
    Experiment,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualitySummary {
    pub baseline_pass_rate_ppm: i64,
    pub candidate_pass_rate_ppm: i64,
    pub delta_ppm: i64,
    pub lower_bound_ppm: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EconomicsSummary {
    pub normalized_cost_delta_ppm: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LatencySummary {
    pub normalized_latency_delta_ppm: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyAdequacySummary {
    pub observed: u32,
    pub adequate_trials: u32,
    pub semantic_successes: u32,
    pub pinned: bool,
}

/// Auditable decision summary for one explicit route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyCertificate {
    pub owner: RouteOwner,
    pub selected_tier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_tier: Option<String>,
    pub source: CertificateSource,
    #[serde(default)]
    pub eligible_episodes: u32,
    #[serde(default)]
    pub independent_tasks: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<QualitySummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub economics: Option<EconomicsSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency: Option<LatencySummary>,
    #[serde(default)]
    pub critical_violations: u32,
    pub verdict: PromotionVerdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluator_config_digest: Option<String>,
    pub compiler_config_digest: String,
    pub evidence_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy: Option<LegacyAdequacySummary>,
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
            key_strategy: PolicyKeyStrategy::AgentTrace,
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
    pub fn as_table_config(&self, mode: PolicyRuntimeMode) -> PolicyTableConfig {
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
            adequacy: mode.apply_to_adequacy(&self.adequacy),
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
    match document.lockfile_version {
        LEGACY_POLICY_LOCKFILE_VERSION => {
            if document.artifact.is_some() || !document.certificates.is_empty() {
                anyhow::bail!("policy lock v1 cannot contain v2 artifact or certificates");
            }
        }
        POLICY_LOCKFILE_VERSION => validate_v2_metadata(document)?,
        version => {
            anyhow::bail!(
                "unsupported policy lockfileVersion {version}; expected {LEGACY_POLICY_LOCKFILE_VERSION} or {POLICY_LOCKFILE_VERSION}"
            );
        }
    }
    for (name, policy) in &document.policies {
        validate_name(name)?;
        let config = policy.as_table_config(PolicyRuntimeMode::Frozen);
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
    if document.lockfile_version == POLICY_LOCKFILE_VERSION {
        validate_v2_certificates(document)?;
    }
    Ok(())
}

fn validate_v2_metadata(document: &PolicyLock) -> Result<()> {
    let artifact = document
        .artifact
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("policy lock v2 requires artifact metadata"))?;
    if let Some(parent) = artifact.parent_digest.as_deref() {
        validate_sha256_digest(parent, "artifact.parent_digest")?;
    }
    validate_sha256_digest(&artifact.evidence_root, "artifact.evidence_root")?;
    if let Some(root) = artifact.eval_snapshot_root.as_deref() {
        validate_sha256_digest(root, "artifact.eval_snapshot_root")?;
    }
    if artifact.source_snapshot_time_unix_ms < 0 {
        anyhow::bail!("artifact.source_snapshot_time_unix_ms cannot be negative");
    }
    if artifact.compiler.id.trim().is_empty() {
        anyhow::bail!("artifact.compiler.id cannot be empty");
    }
    if artifact.compiler.version == 0 {
        anyhow::bail!("artifact.compiler.version must be positive");
    }
    validate_sha256_digest(
        &artifact.compiler.config_digest,
        "artifact.compiler.config_digest",
    )?;
    if let Some(migration) = &artifact.migration {
        validate_sha256_digest(
            &migration.legacy_adequacy_digest,
            "artifact.migration.legacy_adequacy_digest",
        )?;
    }
    Ok(())
}

fn validate_v2_certificates(document: &PolicyLock) -> Result<()> {
    for policy_name in document.certificates.keys() {
        if !document.policies.contains_key(policy_name) {
            anyhow::bail!("certificates reference missing policy '{policy_name}'");
        }
    }
    for (policy_name, policy) in &document.policies {
        let certificates = document.certificates.get(policy_name);
        for (request_key, selected_tier) in &policy.routes {
            let certificate = certificates
                .and_then(|entries| entries.get(request_key))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "policy '{policy_name}' route '{request_key}' requires a v2 certificate"
                    )
                })?;
            if certificate.selected_tier != *selected_tier {
                anyhow::bail!(
                    "policy '{policy_name}' route '{request_key}' selects tier '{selected_tier}' but its certificate selected tier '{}'",
                    certificate.selected_tier
                );
            }
        }
        if let Some(certificates) = certificates {
            for (request_key, certificate) in certificates {
                if !policy.routes.contains_key(request_key) {
                    anyhow::bail!(
                        "policy '{policy_name}' certificate '{request_key}' has no explicit route"
                    );
                }
                if !policy.tiers.contains_key(&certificate.selected_tier) {
                    anyhow::bail!(
                        "policy '{policy_name}' certificate '{request_key}' references unknown selected tier '{}'",
                        certificate.selected_tier
                    );
                }
                if let Some(baseline) = certificate.baseline_tier.as_deref()
                    && !policy.tiers.contains_key(baseline)
                {
                    anyhow::bail!(
                        "policy '{policy_name}' certificate '{request_key}' references unknown baseline tier '{baseline}'"
                    );
                }
                validate_sha256_digest(
                    &certificate.compiler_config_digest,
                    "certificate.compiler_config_digest",
                )?;
                validate_sha256_digest(
                    &certificate.evidence_digest,
                    "certificate.evidence_digest",
                )?;
                if let Some(digest) = certificate.evaluator_config_digest.as_deref() {
                    validate_sha256_digest(digest, "certificate.evaluator_config_digest")?;
                }
                match (certificate.owner, certificate.source) {
                    (RouteOwner::Operator, CertificateSource::Operator)
                    | (RouteOwner::Compiler, CertificateSource::LegacyAdequacyV1)
                    | (RouteOwner::Compiler, CertificateSource::TaskNative)
                    | (RouteOwner::Compiler, CertificateSource::Human)
                    | (RouteOwner::Compiler, CertificateSource::Enterprise)
                    | (RouteOwner::Compiler, CertificateSource::Agentic)
                    | (RouteOwner::Compiler, CertificateSource::Mixed) => {}
                    _ => {
                        anyhow::bail!(
                            "policy '{policy_name}' certificate '{request_key}' has inconsistent owner and source"
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

fn validate_sha256_digest(value: &str, field: &str) -> Result<()> {
    let Some(hex_digest) = value.strip_prefix("sha256:") else {
        anyhow::bail!("{field} must be a sha256 digest");
    };
    if hex_digest.len() != 64
        || !hex_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        anyhow::bail!("{field} must contain 64 lowercase hexadecimal digits");
    }
    Ok(())
}

pub fn validate_for_config(config: &Config, document: &PolicyLock) -> Result<()> {
    validate_document(document)?;
    for (name, policy) in &document.policies {
        validate_policy_table_config(&policy.as_table_config(config.policy.mode))
            .map_err(anyhow::Error::from)
            .with_context(|| {
                format!(
                    "validating policy '{name}' for {:?} mode",
                    config.policy.mode
                )
            })?;
    }
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

/// Ensure a non-empty legacy database has been sealed into the active v2 lock
/// before an adaptive process accepts publication-capable traffic.
pub fn verify_legacy_migration(
    mode: PolicyRuntimeMode,
    document: &PolicyLock,
    snapshot: &crate::policy_compile::LegacyAdequacySnapshot,
) -> Result<()> {
    if mode == PolicyRuntimeMode::Frozen || snapshot.is_empty() {
        return Ok(());
    }
    let actual = snapshot.semantic_digest()?;
    let migrated = document
        .artifact
        .as_ref()
        .and_then(|artifact| artifact.migration.as_ref())
        .map(|migration| migration.legacy_adequacy_digest.as_str());
    if migrated != Some(actual.as_str()) {
        anyhow::bail!(
            "adaptive policy startup found unsealed legacy learned state; run `bitrouter policy compile --output policy-candidate.yaml` and publish the candidate"
        );
    }
    Ok(())
}

pub fn validate_name(name: &str) -> Result<()> {
    let mut segments = name.split(':');
    let base = segments.next().unwrap_or_default();
    let variant = segments.next();
    if segments.next().is_some()
        || !valid_policy_name_segment(base)
        || variant.is_some_and(|segment| !valid_policy_name_segment(segment))
    {
        anyhow::bail!(
            "invalid policy name '{name}' (use base or base:variant; each segment accepts letters, digits, '.', '_' or '-', without a leading '.')"
        );
    }
    Ok(())
}

fn valid_policy_name_segment(segment: &str) -> bool {
    !segment.is_empty()
        && !segment.starts_with('.')
        && !segment.chars().any(|character| {
            character.is_whitespace()
                || !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.'))
        })
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PolicyRouteDiff {
    pub policy: String,
    pub request_key: String,
    pub active_tier: Option<String>,
    pub candidate_tier: Option<String>,
}

pub fn diff_documents(active: &PolicyLock, candidate: &PolicyLock) -> Vec<PolicyRouteDiff> {
    let mut policies = active.policies.keys().cloned().collect::<BTreeSet<_>>();
    policies.extend(candidate.policies.keys().cloned());
    let mut differences = Vec::new();
    for policy_name in policies {
        let active_routes = active
            .policies
            .get(&policy_name)
            .map(|policy| &policy.routes);
        let candidate_routes = candidate
            .policies
            .get(&policy_name)
            .map(|policy| &policy.routes);
        let mut request_keys = active_routes
            .into_iter()
            .flat_map(|routes| routes.keys().cloned())
            .collect::<BTreeSet<_>>();
        request_keys.extend(
            candidate_routes
                .into_iter()
                .flat_map(|routes| routes.keys().cloned()),
        );
        for request_key in request_keys {
            let active_tier = active_routes
                .and_then(|routes| routes.get(&request_key))
                .cloned();
            let candidate_tier = candidate_routes
                .and_then(|routes| routes.get(&request_key))
                .cloned();
            if active_tier != candidate_tier {
                differences.push(PolicyRouteDiff {
                    policy: policy_name.clone(),
                    request_key,
                    active_tier,
                    candidate_tier,
                });
            }
        }
    }
    differences
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotionRecord {
    pub action: String,
    pub parent_digest: String,
    pub child_digest: String,
    pub recorded_at: String,
}

/// Publish a validated candidate while retaining exact parent and child bytes.
pub fn publish_candidate(
    active_path: &Path,
    expected_digest: &str,
    candidate: &PolicyLock,
    history_dir: &Path,
) -> Result<PromotionRecord> {
    let candidate_bytes = deterministic_yaml(candidate)?.into_bytes();
    publish_bytes(
        active_path,
        expected_digest,
        &candidate_bytes,
        candidate,
        history_dir,
        "promote",
    )
}

/// Restore the exact bytes previously archived for a semantic digest.
pub fn rollback_to_digest(
    active_path: &Path,
    expected_digest: &str,
    target_digest: &str,
    history_dir: &Path,
) -> Result<PromotionRecord> {
    validate_sha256_digest(target_digest, "rollback target digest")?;
    let target_path = history_snapshot_path(history_dir, target_digest)?;
    let target_bytes = std::fs::read(&target_path)
        .with_context(|| format!("reading policy history {}", target_path.display()))?;
    let target_raw = std::str::from_utf8(&target_bytes).context("policy history is not UTF-8")?;
    let target: PolicyLock = serde_saphyr::from_str(target_raw)
        .with_context(|| format!("parsing policy history {}", target_path.display()))?;
    validate_document(&target)?;
    let actual = semantic_digest(&target)?;
    if actual != target_digest {
        anyhow::bail!(
            "policy history digest mismatch for {} (expected {target_digest}, found {actual})",
            target_path.display()
        );
    }
    publish_bytes(
        active_path,
        expected_digest,
        &target_bytes,
        &target,
        history_dir,
        "rollback",
    )
}

fn publish_bytes(
    active_path: &Path,
    expected_digest: &str,
    target_bytes: &[u8],
    target: &PolicyLock,
    history_dir: &Path,
    action: &str,
) -> Result<PromotionRecord> {
    let _publication_lock = acquire_publication_lock(active_path)?;
    let parent_bytes = std::fs::read(active_path)
        .with_context(|| format!("reading active policy lock {}", active_path.display()))?;
    let parent_raw =
        std::str::from_utf8(&parent_bytes).context("active policy lock is not UTF-8")?;
    let parent: PolicyLock = serde_saphyr::from_str(parent_raw)
        .with_context(|| format!("parsing active policy lock {}", active_path.display()))?;
    validate_document(&parent)?;
    let parent_digest = semantic_digest(&parent)?;
    if parent_digest != expected_digest {
        anyhow::bail!(
            "policy lock changed since it was loaded (expected {expected_digest}, found {parent_digest}); refusing to overwrite"
        );
    }
    let child_digest = semantic_digest(target)?;
    archive_policy_bytes(history_dir, &parent_digest, &parent_bytes)?;
    archive_policy_bytes(history_dir, &child_digest, target_bytes)?;
    write_bytes_atomic(active_path, target_bytes)?;
    let record = PromotionRecord {
        action: action.to_string(),
        parent_digest,
        child_digest,
        recorded_at: chrono::Utc::now().to_rfc3339(),
    };
    append_promotion_record(history_dir, &record)?;
    Ok(record)
}

fn history_snapshot_path(history_dir: &Path, digest: &str) -> Result<PathBuf> {
    let hex_digest = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow::anyhow!("history digest must use sha256"))?;
    Ok(history_dir.join(format!("{hex_digest}.yaml")))
}

fn archive_policy_bytes(history_dir: &Path, digest: &str, bytes: &[u8]) -> Result<()> {
    std::fs::create_dir_all(history_dir)
        .with_context(|| format!("creating policy history {}", history_dir.display()))?;
    let path = history_snapshot_path(history_dir, digest)?;
    if path.exists() {
        let existing = std::fs::read(&path)
            .with_context(|| format!("reading policy history {}", path.display()))?;
        if existing != bytes {
            anyhow::bail!(
                "policy history {} already exists with different bytes",
                path.display()
            );
        }
        return Ok(());
    }
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    let mut file = options
        .open(&path)
        .with_context(|| format!("creating policy history {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("writing policy history {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing policy history {}", path.display()))?;
    sync_parent(&path)
}

fn append_promotion_record(history_dir: &Path, record: &PromotionRecord) -> Result<()> {
    std::fs::create_dir_all(history_dir)
        .with_context(|| format!("creating policy history {}", history_dir.display()))?;
    let path = history_dir.join("promotions.jsonl");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening policy history log {}", path.display()))?;
    serde_json::to_writer(&mut file, record).context("serializing promotion record")?;
    file.write_all(b"\n")
        .with_context(|| format!("writing policy history log {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing policy history log {}", path.display()))?;
    sync_parent(&path)
}

fn resolved_file_location(path: &Path) -> Result<PathBuf> {
    path.file_name()
        .ok_or_else(|| anyhow::anyhow!("policy output must name a file: {}", path.display()))?;
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolving current directory")?
            .join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                resolved.push(component.as_os_str());
            }
            std::path::Component::Normal(_) => {
                resolved.push(component.as_os_str());
                if resolved.exists() {
                    resolved = std::fs::canonicalize(&resolved).with_context(|| {
                        format!("resolving policy path component {}", resolved.display())
                    })?;
                }
            }
        }
    }
    Ok(resolved)
}

/// Bind an existing preset to a routing policy and set the process mode
/// without reserializing the rest of `bitrouter.yaml`.
/// This keeps comments and operator formatting intact.
pub fn edit_config_policy(
    raw: &str,
    preset: &str,
    policy: &str,
    mode: PolicyRuntimeMode,
) -> Result<String> {
    edit_config_policy_with_model(raw, preset, policy, None, mode)
}

/// Change only the policy runtime mode in `bitrouter.yaml`.
pub fn edit_config_mode(raw: &str, mode: PolicyRuntimeMode) -> Result<String> {
    bitrouter_sdk::config::parse(raw).context("parsing bitrouter.yaml")?;
    let mut lines = source_lines(raw);
    set_policy_mode(&mut lines, mode)?;
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
    mode: PolicyRuntimeMode,
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
    set_policy_mode(&mut lines, mode)?;
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

fn set_policy_mode(lines: &mut Vec<String>, mode: PolicyRuntimeMode) -> Result<()> {
    let value = match mode {
        PolicyRuntimeMode::Frozen => "frozen",
        PolicyRuntimeMode::Adaptive => "adaptive",
    };
    if let Some((start, end)) = block_range(lines, "policy", 0) {
        require_block_header(&lines[start], "policy")?;
        if let Some(index) = child_key(lines, start + 1, end, "mode", 2)
            .or_else(|| child_key(lines, start + 1, end, "writeback", 2))
        {
            lines[index] = format!("  mode: {value}");
        } else {
            lines.insert(end, format!("  mode: {value}"));
        }
    } else {
        if !lines.is_empty() && !lines.last().is_some_and(|line| line.is_empty()) {
            lines.push(String::new());
        }
        lines.push("policy:".into());
        lines.push(format!("  mode: {value}"));
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
    pub conflicts: Vec<String>,
}

/// Create one named adaptive policy and bind it to a preset. The candidate
/// main config and lock are fully cross-validated before either file is
/// published. The BitRouter process starts in frozen mode.
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
        escalation_tier: Some("strong".into()),
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
        PolicyRuntimeMode::Frozen,
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
        conflicts: Vec::new(),
    })
}

/// Compile a candidate from a caller-frozen database snapshot time.
pub async fn compile_files(
    config_path: &Path,
    snapshot_time_unix_ms: i64,
) -> Result<PolicyFileUpdate> {
    compile_files_with_eval(config_path, snapshot_time_unix_ms, None).await
}

/// Compile against an explicit immutable eval snapshot. Omitting the root
/// preserves the legacy-only compatibility path.
pub async fn compile_files_with_eval(
    config_path: &Path,
    snapshot_time_unix_ms: i64,
    eval_evidence_root: Option<&str>,
) -> Result<PolicyFileUpdate> {
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config = bitrouter_sdk::config::parse(&raw).context("parsing bitrouter.yaml")?;
    let loaded = load_for_config(&config, Some(config_path))
        .await?
        .ok_or_else(|| anyhow::anyhow!("no policy lock is configured"))?;
    let database_url = readonly_database_url(&config.database.url, config_path)?;
    let db = crate::db::connect(&database_url)
        .await
        .map_err(anyhow::Error::from)?;
    let store = AdequacyStore::new(db.clone());
    let legacy =
        crate::policy_compile::LegacyAdequacySnapshot::load(&store, snapshot_time_unix_ms).await?;
    let eval = match eval_evidence_root {
        Some(root) => Some(
            crate::eval::compiler::EvalEvidenceSnapshot::load(
                &crate::eval::store::EvalStore::new(db),
                root,
            )
            .await?,
        ),
        None => None,
    };
    let compiled = crate::policy_compile::compile_candidate(crate::policy_compile::CompileInput {
        current: &loaded.document,
        parent_digest: Some(&loaded.digest),
        legacy: &legacy,
        eval: eval.as_ref(),
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
        path: loaded.path,
        digest,
        document: compiled.document,
        changes,
        conflicts,
    })
}

/// Validate and publish one exact precompiled v2 candidate. The candidate's
/// parent digest is the compare-and-swap token, so a stale compiler can never
/// overwrite a newer active lock.
pub async fn publish_candidate_file(
    config_path: &Path,
    candidate_path: &Path,
) -> Result<PolicyFileUpdate> {
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config = bitrouter_sdk::config::parse(&raw).context("parsing bitrouter.yaml")?;
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
        .ok_or_else(|| anyhow::anyhow!("policy publish requires a compiled v2 candidate"))?;
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
    let differences = diff_documents(&active.document, &candidate.document);
    let history_dir = default_history_dir(&active.path);
    let record = publish_candidate(
        &active.path,
        parent_digest,
        &candidate.document,
        &history_dir,
    )?;
    Ok(PolicyFileUpdate {
        path: active.path,
        digest: record.child_digest,
        document: candidate.document,
        changes: differences
            .into_iter()
            .map(|difference| {
                format!(
                    "{}: {} {} -> {}",
                    difference.policy,
                    difference.request_key,
                    difference.active_tier.as_deref().unwrap_or("default"),
                    difference.candidate_tier.as_deref().unwrap_or("default")
                )
            })
            .collect(),
        conflicts: Vec::new(),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct EvidenceVerification {
    pub policy_digest: String,
    pub evidence_root: String,
    pub legacy_evidence_root: String,
    pub eval_snapshot_root: Option<String>,
    pub eval_results: usize,
}

/// Reconstruct the artifact-level evidence root from the local append-only
/// ledger and content-addressed eval snapshot. This never changes the lock.
pub async fn verify_evidence_files(config_path: &Path) -> Result<EvidenceVerification> {
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config = bitrouter_sdk::config::parse(&raw).context("parsing bitrouter.yaml")?;
    let loaded = load_for_config(&config, Some(config_path))
        .await?
        .ok_or_else(|| anyhow::anyhow!("no policy lock is configured"))?;
    let artifact = loaded
        .document
        .artifact
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("policy lock v1 has no verifiable evidence artifact"))?;
    let database_url = readonly_database_url(&config.database.url, config_path)?;
    let db = crate::db::connect(&database_url)
        .await
        .map_err(anyhow::Error::from)?;
    let legacy = crate::policy_compile::LegacyAdequacySnapshot::load(
        &AdequacyStore::new(db.clone()),
        artifact.source_snapshot_time_unix_ms,
    )
    .await?;
    let legacy_evidence_root = legacy.semantic_digest()?;
    if artifact
        .migration
        .as_ref()
        .is_some_and(|migration| migration.legacy_adequacy_digest != legacy_evidence_root)
    {
        anyhow::bail!("local legacy evidence no longer matches the compiled migration digest");
    }
    let eval = match artifact.eval_snapshot_root.as_deref() {
        Some(root) => Some(
            crate::eval::compiler::EvalEvidenceSnapshot::load(
                &crate::eval::store::EvalStore::new(db),
                root,
            )
            .await?,
        ),
        None => None,
    };
    let reconstructed = match &eval {
        Some(eval) => crate::eval::types::canonical_digest(&(
            "policy-evidence-v2",
            legacy_evidence_root.as_str(),
            eval.evidence_root.as_str(),
        ))?,
        None => legacy_evidence_root.clone(),
    };
    if reconstructed != artifact.evidence_root {
        anyhow::bail!("policy artifact evidence_root does not match local evidence");
    }
    Ok(EvidenceVerification {
        policy_digest: loaded.digest,
        evidence_root: reconstructed,
        legacy_evidence_root,
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
    let config = bitrouter_sdk::config::parse(&raw).context("parsing bitrouter.yaml")?;
    if apply && config.policy.mode == PolicyRuntimeMode::Frozen {
        anyhow::bail!(
            "policy runtime mode is frozen; set `policy.mode: adaptive` before `policy evolve --apply`"
        );
    }
    let update = compile_files(config_path, chrono::Utc::now().timestamp_millis()).await?;
    if apply {
        if !update.conflicts.is_empty() {
            anyhow::bail!(
                "compiled policy has unresolved conflicts; inspect `bitrouter policy compile` before applying"
            );
        }
        let loaded = load_for_config(&config, Some(config_path))
            .await?
            .ok_or_else(|| anyhow::anyhow!("no policy lock is configured"))?;
        if loaded.digest != update.digest {
            let history_dir = default_history_dir(&loaded.path);
            publish_candidate(&loaded.path, &loaded.digest, &update.document, &history_dir)?;
        }
    }
    Ok(update)
}

pub fn default_history_dir(active_path: &Path) -> PathBuf {
    active_path
        .parent()
        .map(|parent| parent.join(".bitrouter-policy-history"))
        .unwrap_or_else(|| PathBuf::from(".bitrouter-policy-history"))
}

/// Explicit helper for changing process-owned policy mode in the main config.
pub async fn set_mode_file(config_path: &Path, mode: PolicyRuntimeMode) -> Result<()> {
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("reading {}", config_path.display()))?;
    let edited = edit_config_mode(&raw, mode)?;
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
    let _publication_lock = acquire_publication_lock(path)?;
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

fn write_bytes_atomic(path: &Path, updated: &[u8]) -> Result<()> {
    let permissions = std::fs::metadata(path)
        .with_context(|| format!("reading permissions for {}", path.display()))?
        .permissions();
    let tmp = sibling_temp_path(path);
    let result = (|| -> Result<()> {
        let mut file = std::fs::File::create(&tmp)
            .with_context(|| format!("creating policy temp file {}", tmp.display()))?;
        std::fs::set_permissions(&tmp, permissions)
            .with_context(|| format!("preserving permissions on {}", tmp.display()))?;
        file.write_all(updated)
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
        sync_parent(path)
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

fn acquire_publication_lock(path: &Path) -> Result<std::fs::File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating policy directory {}", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("policy-lock.yaml");
    let lock_path = path.with_file_name(format!(".{file_name}.bitrouter.lock"));
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening publication lock {}", lock_path.display()))?;
    lock.lock()
        .with_context(|| format!("acquiring publication lock {}", lock_path.display()))?;
    Ok(lock)
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
    let _publication_lock = acquire_publication_lock(path)?;
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
    decision_recorder: Option<Arc<PolicyDecisionJsonlRecorder>>,
    eval_decisions: PendingEvalDecisionStore,
}

impl PolicyRuntime {
    pub(crate) async fn new(
        config: &Config,
        config_path: Option<&Path>,
        db: DatabaseConnection,
        decision_recorder: Option<Arc<PolicyDecisionJsonlRecorder>>,
        eval_decisions: PendingEvalDecisionStore,
    ) -> Result<Arc<Self>> {
        let runtime = Arc::new(Self {
            snapshot: RwLock::new(Arc::new(PolicySnapshot::default())),
            db,
            decision_recorder,
            eval_decisions,
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
            let snapshot = crate::policy_compile::LegacyAdequacySnapshot::load(
                &AdequacyStore::new(self.db.clone()),
                chrono::Utc::now().timestamp_millis(),
            )
            .await?;
            verify_legacy_migration(config.policy.mode, &loaded.document, &snapshot)?;
            for (name, definition) in &loaded.document.policies {
                let table_config = definition.as_table_config(config.policy.mode);
                let table = PolicyTable::from_config(&table_config)
                    .ok_or_else(|| anyhow::anyhow!("policy '{name}' is inert"))?;
                let route_baselines = loaded
                    .document
                    .certificates
                    .get(name)
                    .into_iter()
                    .flat_map(|certificates| certificates.iter())
                    .filter_map(|(request_key, certificate)| {
                        certificate
                            .baseline_tier
                            .as_ref()
                            .map(|baseline| (request_key.clone(), baseline.clone()))
                    })
                    .collect();
                let mut router = PolicyTableRouter::new(table).with_state_namespace(name.clone());
                router = router.with_eval_observer(
                    self.eval_decisions.clone(),
                    name.clone(),
                    loaded.digest.clone(),
                    route_baselines,
                    definition.default_tier.clone(),
                );
                if let Some(recorder) = &self.decision_recorder {
                    router = router.with_shared_decision_recorder(recorder.clone());
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

    pub fn status(&self, mode: PolicyRuntimeMode) -> PolicyRuntimeStatus {
        let snapshot = self
            .snapshot
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        PolicyRuntimeStatus {
            path: snapshot.path.clone(),
            digest: snapshot.digest.clone(),
            policies: snapshot.routers.keys().cloned().collect(),
            mode,
        }
    }
}

impl ModelSelector for PolicyRuntime {
    fn select(&self, policy: &str, ctx: &mut PipelineContext) -> bitrouter_sdk::Result<()> {
        self.select_variant(policy, None, ctx)
    }

    fn select_variant(
        &self,
        policy: &str,
        variant: Option<&str>,
        ctx: &mut PipelineContext,
    ) -> bitrouter_sdk::Result<()> {
        let snapshot = self
            .snapshot
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let variant_policy = variant.map(|variant| format!("{policy}:{variant}"));
        let router = variant_policy
            .as_deref()
            .and_then(|name| snapshot.routers.get(name))
            .or_else(|| snapshot.routers.get(policy))
            .ok_or_else(|| {
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
    pub mode: PolicyRuntimeMode,
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

    const TEST_DIGEST: &str =
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn v1_remains_readable_but_v2_requires_an_artifact() -> anyhow::Result<()> {
        let v1: PolicyLock = serde_saphyr::from_str("lockfileVersion: 1\npolicies: {}\n")?;
        validate_document(&v1)?;

        let v2_without_artifact: PolicyLock =
            serde_saphyr::from_str("lockfileVersion: 2\npolicies: {}\n")?;
        let result = validate_document(&v2_without_artifact);

        assert!(result.is_err());
        assert!(
            result
                .err()
                .map(|error| error.to_string())
                .is_some_and(|message| message.contains("artifact"))
        );
        Ok(())
    }

    #[test]
    fn v2_certificate_must_match_its_route_and_selected_tier() -> anyhow::Result<()> {
        let raw = format!(
            r#"lockfileVersion: 2
artifact:
  parent_digest: null
  evidence_root: "{TEST_DIGEST}"
  source_snapshot_time_unix_ms: 1785369600000
  compiler:
    id: bitrouter-policy-compiler
    version: 1
    config_digest: "{TEST_DIGEST}"
policies:
  coding:
    tiers:
      economy: vendor:economy
      strong: vendor:strong
    routes:
      agent_trace/v1|edit|normal: economy
    default_tier: strong
certificates:
  coding:
    agent_trace/v1|edit|normal:
      owner: compiler
      selected_tier: strong
      baseline_tier: strong
      source: legacy_adequacy_v1
      eligible_episodes: 1
      independent_tasks: 1
      critical_violations: 0
      verdict: promote
      compiler_config_digest: "{TEST_DIGEST}"
      evidence_digest: "{TEST_DIGEST}"
"#
        );
        let lock: PolicyLock = serde_saphyr::from_str(&raw)?;
        let result = validate_document(&lock);

        assert!(result.is_err());
        assert!(
            result
                .err()
                .map(|error| error.to_string())
                .is_some_and(|message| message.contains("selected tier 'strong'"))
        );
        Ok(())
    }

    #[test]
    fn adaptive_v1_with_nonempty_legacy_state_fails_closed() {
        let snapshot = crate::policy_compile::LegacyAdequacySnapshot {
            snapshot_time_unix_ms: 1_700_000_000_000,
            pins: vec![crate::adequacy::store::LegacyPin {
                fingerprint: "auto\0agent_trace/v1|edit|normal".into(),
                pinned_at_unix: 1_700_000_000,
            }],
            exploration: Vec::new(),
            semantic_successes: Vec::new(),
            reliability_events: Vec::new(),
        };
        let lock = PolicyLock {
            lockfile_version: 1,
            artifact: None,
            policies: BTreeMap::new(),
            certificates: BTreeMap::new(),
        };

        let result = verify_legacy_migration(PolicyRuntimeMode::Adaptive, &lock, &snapshot);

        assert!(result.is_err());
        assert!(
            result
                .err()
                .is_some_and(|error| { error.to_string().contains("bitrouter policy compile") })
        );
    }

    #[test]
    fn copied_v2_lock_serves_with_an_empty_target_database() {
        let snapshot = crate::policy_compile::LegacyAdequacySnapshot {
            snapshot_time_unix_ms: 1_700_000_000_000,
            pins: Vec::new(),
            exploration: Vec::new(),
            semantic_successes: Vec::new(),
            reliability_events: Vec::new(),
        };

        assert!(
            verify_legacy_migration(
                PolicyRuntimeMode::Adaptive,
                &PolicyLock::default(),
                &snapshot
            )
            .is_ok()
        );
    }

    #[test]
    fn deterministic_round_trip_and_digest() {
        let lock = PolicyLock {
            lockfile_version: 1,
            artifact: None,
            policies: BTreeMap::from([("coding".into(), definition())]),
            certificates: BTreeMap::new(),
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
    fn process_mode_never_activates_request_time_learning() {
        let mut policy = definition();
        policy.adequacy.enabled = false;
        policy.adequacy.explore_enabled = false;
        policy.adequacy.explore_tier = Some("economy".into());

        let frozen = policy.as_table_config(PolicyRuntimeMode::Frozen);
        assert!(!frozen.adequacy.enabled);
        assert!(!frozen.adequacy.explore_enabled);

        let adaptive = policy.as_table_config(PolicyRuntimeMode::Adaptive);
        assert!(!adaptive.adequacy.enabled);
        assert!(!adaptive.adequacy.explore_enabled);
        assert_eq!(adaptive.adequacy.explore_tier.as_deref(), Some("economy"));
    }

    #[tokio::test]
    async fn frozen_and_adaptive_modes_route_identically_from_the_lock() -> anyhow::Result<()> {
        use bitrouter_sdk::caller::CallerContext;
        use bitrouter_sdk::language_model::{
            GenerationParams, Message, PipelineRequest, Prompt, Role,
        };

        fn context() -> PipelineContext {
            let prompt = Prompt {
                model: "vendor:strong".into(),
                system: None,
                system_provider_metadata: Default::default(),
                messages: vec![Message::text(Role::User, "solve this")],
                tools: Vec::new(),
                params: GenerationParams::default(),
                response_format: None,
                tool_choice: None,
                stream: false,
            };
            PipelineContext::new(PipelineRequest::new(
                "vendor:strong",
                CallerContext::local(),
                prompt,
            ))
        }

        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("bitrouter.yaml");
        tokio::fs::write(
            &config_path,
            r#"policy:
  mode: frozen
presets:
  coding:
    model: vendor:strong
    policy: coding
"#,
        )
        .await?;
        let mut policy = definition();
        policy.adequacy.escalation_tier = Some("strong".into());
        policy.adequacy.explore_tier = Some("economy".into());
        policy.adequacy.explore_opening = true;
        policy.adequacy.min_semantic_successes_for_lock = 1;
        let lock = PolicyLock {
            lockfile_version: 1,
            artifact: None,
            policies: BTreeMap::from([("coding".into(), policy)]),
            certificates: BTreeMap::new(),
        };
        write_atomic(&dir.path().join("policy-lock.yaml"), None, &lock)?;

        let mut config = bitrouter_sdk::config::load(&config_path).await?;
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = AdequacyStore::new(db.clone());
        let ledger_key = "coding\0agent_trace/v1|opening|normal";
        store.upsert_exploration(ledger_key, 3, 3, true).await?;
        store
            .record_semantic_success(ledger_key, "terminal-bench/task-a")
            .await?;
        let snapshot =
            crate::policy_compile::LegacyAdequacySnapshot::load(&store, 1_700_000_100_000).await?;
        let compiled =
            crate::policy_compile::compile_candidate(crate::policy_compile::CompileInput {
                current: &lock,
                parent_digest: None,
                legacy: &snapshot,
                eval: None,
            })?;
        write_atomic(
            &dir.path().join("policy-lock.yaml"),
            Some(&semantic_digest(&lock)?),
            &compiled.document,
        )?;

        let runtime = PolicyRuntime::new(
            &config,
            Some(&config_path),
            db,
            None,
            PendingEvalDecisionStore::default(),
        )
        .await?;
        let mut frozen = context();
        runtime.select("coding", &mut frozen)?;
        assert_eq!(frozen.model(), "vendor:economy");

        let empty_target_db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&empty_target_db).await?;
        let empty_target = PolicyRuntime::new(
            &config,
            Some(&config_path),
            empty_target_db,
            None,
            PendingEvalDecisionStore::default(),
        )
        .await?;
        let mut copied = context();
        empty_target.select("coding", &mut copied)?;
        assert_eq!(copied.model(), frozen.model());

        config.policy.mode = PolicyRuntimeMode::Adaptive;
        runtime
            .reload_for_config(&config, Some(&config_path))
            .await?;
        let mut adaptive = context();
        runtime.select("coding", &mut adaptive)?;
        assert_eq!(adaptive.model(), "vendor:economy");

        store.upsert_pin(ledger_key, 1_700_000_200).await?;
        store.upsert_exploration(ledger_key, 99, 0, false).await?;
        let mut after_database_mutation = context();
        runtime.select("coding", &mut after_database_mutation)?;
        assert_eq!(after_database_mutation.model(), adaptive.model());
        Ok(())
    }

    #[tokio::test]
    async fn policy_runtime_prefers_exact_variant_then_falls_back() -> anyhow::Result<()> {
        use bitrouter_sdk::caller::CallerContext;
        use bitrouter_sdk::language_model::{
            GenerationParams, Message, PipelineRequest, Prompt, Role,
        };

        fn context() -> PipelineContext {
            PipelineContext::new(PipelineRequest::new(
                "vendor:strong",
                CallerContext::local(),
                Prompt {
                    model: "vendor:strong".into(),
                    system: None,
                    system_provider_metadata: Default::default(),
                    messages: vec![Message::text(Role::User, "solve this")],
                    tools: Vec::new(),
                    params: GenerationParams::default(),
                    response_format: None,
                    tool_choice: None,
                    stream: false,
                },
            ))
        }

        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("bitrouter.yaml");
        tokio::fs::write(
            &config_path,
            "presets:\n  auto:\n    model: vendor:strong\n    policy: auto\n",
        )
        .await?;
        let mut cost = definition();
        cost.routes.insert("opening".into(), "economy".into());
        cost.default_tier = Some("economy".into());
        let lock = PolicyLock {
            lockfile_version: 1,
            artifact: None,
            policies: BTreeMap::from([("auto".into(), definition()), ("auto:cost".into(), cost)]),
            certificates: BTreeMap::new(),
        };
        write_atomic(&dir.path().join("policy-lock.yaml"), None, &lock)?;
        let config = bitrouter_sdk::config::load(&config_path).await?;
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let runtime = PolicyRuntime::new(
            &config,
            Some(&config_path),
            db,
            None,
            PendingEvalDecisionStore::default(),
        )
        .await?;

        let mut exact = context();
        runtime.select_variant("auto", Some("cost"), &mut exact)?;
        assert_eq!(exact.model(), "vendor:economy");
        let mut fallback = context();
        runtime.select_variant("auto", Some("latency"), &mut fallback)?;
        assert_eq!(fallback.model(), "vendor:strong");
        Ok(())
    }

    #[test]
    fn legacy_workflow_state_locks_deserialize_as_agent_trace_canonically() {
        let lock: PolicyLock = serde_saphyr::from_str(
            r#"
lockfileVersion: 1
policies:
  coding:
    key_strategy: workflow_state
    tiers:
      strong: vendor:strong
    routes:
      agent_trace/v1|edit|normal: strong
"#,
        )
        .unwrap();
        assert_eq!(
            lock.policies["coding"].key_strategy,
            PolicyKeyStrategy::AgentTrace
        );

        let rendered = deterministic_yaml(&lock).unwrap();
        assert!(rendered.contains("key_strategy: agent_trace"));
        assert!(!rendered.contains("key_strategy: workflow_state"));
    }

    #[test]
    fn auto_router_template_lock_is_bound_and_canonical() {
        let template_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("templates/auto-router");
        let config_raw = std::fs::read_to_string(template_dir.join("bitrouter.yaml")).unwrap();
        let lock_raw = std::fs::read_to_string(template_dir.join("policy-lock.yaml")).unwrap();
        let config = bitrouter_sdk::config::parse(&config_raw).unwrap();
        let lock: PolicyLock = serde_saphyr::from_str(&lock_raw).unwrap();

        validate_for_config(&config, &lock).unwrap();
        assert_eq!(config.policy.mode, PolicyRuntimeMode::Frozen);
        assert!(!config_raw.contains("writeback:"));
        assert!(!lock_raw.contains("enabled:"));
        assert!(!lock_raw.contains("explore_enabled:"));
        let policy = &lock.policies["auto"];
        assert_eq!(policy.key_strategy, PolicyKeyStrategy::AgentTrace);
        assert_eq!(policy.tiers["balanced"], "bitrouter:moonshotai/kimi-k3");
        assert_eq!(policy.routes["agent_trace/v2|edit|normal"], "economy");
        assert_eq!(policy.routes["agent_trace/v2|test|normal"], "economy");
        assert_eq!(
            policy.routes["agent_trace/v2|tool_followup|normal"],
            "economy"
        );
        for key in [
            "agent_trace/v2|review|normal",
            "agent_trace/v2|review|context",
            "agent_trace/v2|edit|context",
            "agent_trace/v2|test|context",
            "agent_trace/v2|tool_followup|context",
        ] {
            assert_eq!(policy.routes[key], "balanced", "{key}");
        }
        assert_eq!(policy.default_tier.as_deref(), Some("strong"));
        assert_eq!(policy.tool_use_tier.as_deref(), Some("strong"));
        assert_eq!(policy.tool_safe_tiers, ["strong", "balanced", "economy"]);

        let rendered = deterministic_yaml(&lock).unwrap();
        assert!(rendered.contains("key_strategy: agent_trace"));
        assert!(!rendered.contains("key_strategy: workflow_state"));
    }

    #[test]
    fn auto_router_template_experiments_are_compiler_owned() -> anyhow::Result<()> {
        let template_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("templates/auto-router");
        let lock_raw = std::fs::read_to_string(template_dir.join("policy-lock.yaml"))?;
        let lock: PolicyLock = serde_saphyr::from_str(&lock_raw)?;

        validate_document(&lock)?;
        let certificates = lock
            .certificates
            .get("auto")
            .ok_or_else(|| anyhow::anyhow!("auto template is missing route certificates"))?;
        assert_eq!(certificates.len(), 8);
        for certificate in certificates.values() {
            assert_eq!(certificate.owner, RouteOwner::Compiler);
            assert_eq!(certificate.source, CertificateSource::Mixed);
            assert_eq!(certificate.verdict, PromotionVerdict::Experiment);
        }
        Ok(())
    }

    #[test]
    fn candidate_export_refuses_to_replace_the_active_lock() {
        let dir = tempfile::tempdir().unwrap();
        let active = dir.path().join("policy-lock.yaml");
        let lock = PolicyLock {
            lockfile_version: 1,
            artifact: None,
            policies: BTreeMap::from([("coding".into(), definition())]),
            certificates: BTreeMap::new(),
        };
        write_atomic(&active, None, &lock).unwrap();
        let before = std::fs::read(&active).unwrap();

        let error = export_candidate_file(&active, &active, &lock).unwrap_err();

        assert!(error.to_string().contains("active policy lock"));
        assert_eq!(std::fs::read(&active).unwrap(), before);
    }

    #[test]
    fn candidate_export_refuses_nonexistent_parent_alias_of_active_lock() {
        let dir = tempfile::tempdir().unwrap();
        let active = dir.path().join("policy-lock.yaml");
        let candidate = dir.path().join("new").join("..").join("policy-lock.yaml");
        let lock = PolicyLock {
            lockfile_version: 1,
            artifact: None,
            policies: BTreeMap::from([("coding".into(), definition())]),
            certificates: BTreeMap::new(),
        };
        write_atomic(&active, None, &lock).unwrap();
        let before = std::fs::read(&active).unwrap();

        let error = export_candidate_file(&active, &candidate, &lock).unwrap_err();

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
            artifact: None,
            policies: BTreeMap::from([("coding".into(), definition())]),
            certificates: BTreeMap::new(),
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
    fn publish_preserves_exact_parent_bytes_for_rollback() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let active = dir.path().join("policy-lock.yaml");
        let history = dir.path().join("history");
        let original = "# operator comment\nlockfileVersion: 1\npolicies: {}\n";
        std::fs::write(&active, original)?;
        let current: PolicyLock = serde_saphyr::from_str(original)?;
        let mut candidate = current.clone();
        candidate.policies.insert("coding".into(), definition());

        let record = publish_candidate(&active, &semantic_digest(&current)?, &candidate, &history)?;
        rollback_to_digest(
            &active,
            &record.child_digest,
            &record.parent_digest,
            &history,
        )?;

        assert_eq!(std::fs::read(&active)?, original.as_bytes());
        Ok(())
    }

    #[test]
    fn validation_rejects_empty_or_duplicate_tier_models() {
        let mut empty = definition();
        empty.tiers.insert("strong".into(), "   ".into());
        let error = validate_document(&PolicyLock {
            lockfile_version: 1,
            artifact: None,
            policies: BTreeMap::from([("coding".into(), empty)]),
            certificates: BTreeMap::new(),
        })
        .unwrap_err();
        assert!(error.to_string().contains("non-empty model id"));

        let mut duplicate = definition();
        duplicate
            .tiers
            .insert("economy".into(), "vendor:strong".into());
        let error = validate_document(&PolicyLock {
            lockfile_version: 1,
            artifact: None,
            policies: BTreeMap::from([("coding".into(), duplicate)]),
            certificates: BTreeMap::new(),
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
                artifact: None,
                policies: BTreeMap::from([("coding".into(), definition())]),
                certificates: BTreeMap::new(),
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
            artifact: None,
            policies: BTreeMap::from([("coding".into(), definition())]),
            certificates: BTreeMap::new(),
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
            edit_config_policy(raw, "coding", "terminal-bench", PolicyRuntimeMode::Frozen).unwrap();

        assert!(edited.contains("# team routing"));
        assert!(edited.contains("# keep this operator note"));
        assert!(edited.contains("    policy: terminal-bench"));
        assert!(edited.contains("policy:\n  mode: frozen"));
        let parsed = bitrouter_sdk::config::parse(&edited).unwrap();
        assert_eq!(
            parsed.presets["coding"].policy.as_deref(),
            Some("terminal-bench")
        );
        assert_eq!(parsed.policy.mode, PolicyRuntimeMode::Frozen);
    }

    #[test]
    fn config_edit_refuses_to_replace_a_different_binding() {
        let raw = r#"presets:
  coding:
    model: anthropic/claude-opus-4.8
    policy: production
"#;

        let error =
            edit_config_policy(raw, "coding", "experiment", PolicyRuntimeMode::Frozen).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("already binds policy 'production'")
        );
    }

    #[tokio::test]
    async fn initialize_writes_a_frozen_policy_and_preserves_the_config() {
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
        assert_eq!(config.policy.mode, PolicyRuntimeMode::Frozen);
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
        reloadable.key_strategy = PolicyKeyStrategy::AgentTrace;
        let mut lock = PolicyLock {
            lockfile_version: 1,
            artifact: None,
            policies: BTreeMap::from([("coding".into(), reloadable)]),
            certificates: BTreeMap::new(),
        };
        write_atomic(&lock_path, None, &lock).unwrap();
        let db = crate::db::connect("sqlite::memory:").await.unwrap();
        crate::db::run_migrations(&db).await.unwrap();
        let runtime = PolicyRuntime::new(
            &config,
            Some(&config_path),
            db,
            None,
            PendingEvalDecisionStore::default(),
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
            .insert("agent_trace/v1|opening|normal".into(), "economy".into());
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
        set_mode_file(&config_path, PolicyRuntimeMode::Adaptive)
            .await
            .unwrap();
        let db_path = dir.path().join("bitrouter.db");
        let db = crate::db::connect(&format!("sqlite://{}", db_path.display()))
            .await
            .unwrap();
        crate::db::run_migrations(&db).await.unwrap();
        let store = AdequacyStore::new(db);
        let request_key = "agent_trace/v1|tool_followup|normal";
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
