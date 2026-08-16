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
    AdequacyConfig, Config, PolicyKeyStrategy, PolicyModelTarget, PolicyRuntimeMode,
    PolicyTableConfig, TrajectoryConfig, validate_policy_table_config,
};
#[cfg(test)]
use bitrouter_sdk::language_model::types::ReasoningEffort;
use bitrouter_sdk::language_model::{ModelSelector, PipelineContext, RouteHook, RoutingTarget};
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::adequacy::store::AdequacyStore;
use crate::continuation::{ContinuationAdjustment, ContinuationRequestPlan};
use crate::eval::settlement::{EvalInvocation, PendingEvalDecisionStore};
use crate::policy_table_router::{PolicyTable, PolicyTableRouter};
use crate::trajectory::correlation::{TrajectoryRuntime, stable_id};
use crate::trajectory::guard::{ProgressGuardPolicy, RouteIntentClauseDisposition};
use crate::trajectory::store::GuardedRouteInput;
use crate::workflow_state::decision::PolicyDecisionJsonlRecorder;
use crate::workflow_state::ir::RouteProjection;
use crate::workflow_state::predictive::{
    PredictiveRouteProjection, PredictorContract, compiled_predictor_contract,
    compiled_scorecard_digest,
};

pub const DEFAULT_POLICY_LOCK_FILENAME: &str = "policy-lock.yaml";
pub const LEGACY_POLICY_LOCKFILE_VERSION: u32 = 1;
pub const EVIDENCE_POLICY_LOCKFILE_VERSION: u32 = 2;
pub const POLICY_LOCKFILE_VERSION: u32 = 3;
pub const POLICY_COMPILER_ID: &str = "bitrouter-policy-compiler";
pub const POLICY_COMPILER_VERSION: u32 = 1;
pub(crate) const OPTIMIZATION_EXPERIMENT_COMPILER_ID: &str =
    "bitrouter-optimization-private-experiment";
const EMPTY_SHA256: &str =
    "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// The complete deterministic policy artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyLock {
    /// File-format version only.
    #[serde(rename = "lockfileVersion")]
    pub lockfile_version: u32,
    /// Reproducible compiler inputs and artifact lineage. Required for v2+.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<PolicyArtifact>,
    /// Named policies referenced by `presets.<name>.policy`.
    #[serde(default)]
    pub policies: BTreeMap<String, PolicyDefinition>,
    /// Decision-relevant provenance for explicit routes, nested by policy and
    /// canonical route key. Required for every v2+ route.
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
    pub fn is_compiled(&self) -> bool {
        matches!(
            self.lockfile_version,
            EVIDENCE_POLICY_LOCKFILE_VERSION | POLICY_LOCKFILE_VERSION
        )
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
    #[serde(default, skip_serializing_if = "LegacyEvidenceSource::is_database")]
    pub source: LegacyEvidenceSource,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyEvidenceSource {
    #[default]
    DatabaseAtSnapshot,
    SealedEmpty,
}

impl LegacyEvidenceSource {
    fn is_database(&self) -> bool {
        *self == Self::DatabaseAtSnapshot
    }
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
    pub tiers: BTreeMap<String, PolicyModelTarget>,
    /// Workflow-state/fingerprint key to tier. `fingerprints` is accepted as a
    /// migration alias, while deterministic output always uses `routes`.
    #[serde(alias = "fingerprints")]
    pub routes: BTreeMap<String, String>,
    pub default_tier: Option<String>,
    pub tool_use_tier: Option<String>,
    pub tool_safe_tiers: Vec<String>,
    /// Optional signed, named-policy-only trajectory guard. Legacy global
    /// `policy_table:` config has no corresponding field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_guard: Option<ProgressGuardPolicy>,
    /// Exact deterministic predictor admitted by this signed policy. Required
    /// whenever a route uses the predictive `agent_route/v1` namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predictor: Option<PredictorContract>,
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
            progress_guard: None,
            predictor: None,
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
        EVIDENCE_POLICY_LOCKFILE_VERSION | POLICY_LOCKFILE_VERSION => {
            validate_compiled_metadata(document)?
        }
        version => {
            anyhow::bail!(
                "unsupported policy lockfileVersion {version}; expected {LEGACY_POLICY_LOCKFILE_VERSION}, {EVIDENCE_POLICY_LOCKFILE_VERSION}, or {POLICY_LOCKFILE_VERSION}"
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
            if model.effort().is_some() && document.lockfile_version < POLICY_LOCKFILE_VERSION {
                anyhow::bail!(
                    "policy '{name}' tier '{tier}' uses a compound model/effort target that requires policy lock v{POLICY_LOCKFILE_VERSION}"
                );
            }
            if model.model().trim().is_empty() {
                anyhow::bail!("policy '{name}' tier '{tier}' must use a non-empty model id");
            }
            if model.model().starts_with('@')
                || bitrouter_sdk::config::presets::is_reserved(model.model())
            {
                anyhow::bail!(
                    "policy '{name}' tier target '{}' cannot reference another preset",
                    model.model()
                );
            }
            if let Some(previous) = model_tiers.insert(model, tier) {
                anyhow::bail!(
                    "policy '{name}' tiers '{previous}' and '{tier}' use the same model/effort target '{}'",
                    model.model()
                );
            }
        }
        if let Some(guard) = &policy.progress_guard {
            if document.lockfile_version < EVIDENCE_POLICY_LOCKFILE_VERSION {
                anyhow::bail!(
                    "policy '{name}' progress_guard requires policy lock v{EVIDENCE_POLICY_LOCKFILE_VERSION}+"
                );
            }
            validate_progress_guard(name, policy, guard)?;
        }
        validate_predictive_route_keys(name, policy)?;
        validate_predictor_contract(name, policy, document.lockfile_version)?;
    }
    if document.is_compiled() {
        validate_v2_certificates(document)?;
    }
    Ok(())
}

fn validate_predictor_contract(
    policy_name: &str,
    policy: &PolicyDefinition,
    lockfile_version: u32,
) -> Result<()> {
    let uses_predictive_routes = policy
        .routes
        .keys()
        .any(|key| key.starts_with("agent_route/"));
    if uses_predictive_routes && lockfile_version < EVIDENCE_POLICY_LOCKFILE_VERSION {
        anyhow::bail!(
            "policy '{policy_name}' predictive agent_route routes require policy lock v{EVIDENCE_POLICY_LOCKFILE_VERSION}+ provenance metadata"
        );
    }
    if !uses_predictive_routes && policy.predictor.is_none() {
        return Ok(());
    }
    let expected = compiled_predictor_contract();
    let Some(actual) = policy.predictor.as_ref() else {
        anyhow::bail!(
            "policy '{policy_name}' uses predictive agent_route routes but is missing its signed predictor contract (expected {})",
            compiled_scorecard_digest()
        );
    };
    if actual != &expected {
        anyhow::bail!(
            "policy '{policy_name}' predictor contract does not match this BitRouter binary (expected {})",
            compiled_scorecard_digest()
        );
    }
    Ok(())
}

fn validate_predictive_route_keys(policy_name: &str, policy: &PolicyDefinition) -> Result<()> {
    for route_key in policy.routes.keys() {
        if PredictiveRouteProjection::parse_key(route_key).is_none() {
            anyhow::bail!(
                "policy '{policy_name}' route '{route_key}' is not a canonical agent_route/v1|<task-family>|<role>|<risk> key"
            );
        }
    }
    Ok(())
}

fn validate_progress_guard(
    policy_name: &str,
    policy: &PolicyDefinition,
    guard: &ProgressGuardPolicy,
) -> Result<()> {
    if guard.escalation_tier.trim().is_empty() || !policy.tiers.contains_key(&guard.escalation_tier)
    {
        anyhow::bail!(
            "policy '{policy_name}' progress_guard escalation_tier must reference a defined tier"
        )
    }
    if guard.protected_tiers.is_empty() || !guard.protected_tiers.contains(&guard.escalation_tier) {
        anyhow::bail!(
            "policy '{policy_name}' progress_guard protected_tiers must be non-empty and contain escalation_tier"
        )
    }
    if let Some(unknown) = guard
        .protected_tiers
        .iter()
        .find(|tier| !policy.tiers.contains_key(*tier))
    {
        anyhow::bail!(
            "policy '{policy_name}' progress_guard protected tier '{unknown}' is not defined"
        )
    }
    if let Some(tool_use_tier) = policy.tool_use_tier.as_deref()
        && !guard.protected_tiers.contains(tool_use_tier)
    {
        anyhow::bail!(
            "policy '{policy_name}' progress_guard protected_tiers must contain tool_use_tier '{tool_use_tier}'"
        )
    }
    if guard.hold_for_requests == 0 || guard.hold_for_requests > u32::MAX as u64 {
        anyhow::bail!(
            "policy '{policy_name}' progress_guard hold_for_requests must be in 1..=u32::MAX"
        )
    }
    let thresholds = [
        guard.max_consecutive_unprotected,
        guard.max_same_projection_unprotected,
        guard.max_recovery_count,
        guard.max_episode_requests,
        guard.max_episode_elapsed_ms,
        guard.max_episode_cost_micro_usd,
    ];
    if thresholds.into_iter().flatten().any(|value| value == 0) {
        anyhow::bail!("policy '{policy_name}' progress_guard thresholds must be positive")
    }
    Ok(())
}

fn validate_compiled_metadata(document: &PolicyLock) -> Result<()> {
    let artifact = document
        .artifact
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("compiled policy lock requires artifact metadata"))?;
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
    if !config.trajectory.enabled
        && document
            .policies
            .values()
            .any(|policy| policy.progress_guard.is_some())
    {
        anyhow::bail!("signed policy progress_guard requires trajectory.enabled: true")
    }
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

/// Ensure a non-empty legacy database has been sealed into the active compiled lock
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PolicyGuardDiff {
    pub policy: String,
    pub field: String,
    pub active_value: Option<String>,
    pub candidate_value: Option<String>,
}

pub fn diff_progress_guards(active: &PolicyLock, candidate: &PolicyLock) -> Vec<PolicyGuardDiff> {
    let mut policies = active.policies.keys().cloned().collect::<BTreeSet<_>>();
    policies.extend(candidate.policies.keys().cloned());
    let mut differences = Vec::new();
    for policy_name in policies {
        let active_fields = guard_fields(
            active
                .policies
                .get(&policy_name)
                .and_then(|policy| policy.progress_guard.as_ref()),
        );
        let candidate_fields = guard_fields(
            candidate
                .policies
                .get(&policy_name)
                .and_then(|policy| policy.progress_guard.as_ref()),
        );
        let mut fields = active_fields.keys().cloned().collect::<BTreeSet<_>>();
        fields.extend(candidate_fields.keys().cloned());
        for field in fields {
            let active_value = active_fields.get(&field).cloned();
            let candidate_value = candidate_fields.get(&field).cloned();
            if active_value != candidate_value {
                differences.push(PolicyGuardDiff {
                    policy: policy_name.clone(),
                    field,
                    active_value,
                    candidate_value,
                });
            }
        }
    }
    differences
}

pub fn diff_explanations(active: &PolicyLock, candidate: &PolicyLock) -> Vec<String> {
    let mut explanations = diff_documents(active, candidate)
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
        .collect::<Vec<_>>();
    let mut policies = active.policies.keys().cloned().collect::<BTreeSet<_>>();
    policies.extend(candidate.policies.keys().cloned());
    for policy_name in policies {
        let active_tiers = active
            .policies
            .get(&policy_name)
            .map(|policy| &policy.tiers);
        let candidate_tiers = candidate
            .policies
            .get(&policy_name)
            .map(|policy| &policy.tiers);
        let mut tiers = active_tiers
            .into_iter()
            .flat_map(|items| items.keys().cloned())
            .collect::<BTreeSet<_>>();
        tiers.extend(
            candidate_tiers
                .into_iter()
                .flat_map(|items| items.keys().cloned()),
        );
        for tier in tiers {
            let active_target = active_tiers.and_then(|items| items.get(&tier));
            let candidate_target = candidate_tiers.and_then(|items| items.get(&tier));
            if active_target != candidate_target {
                explanations.push(format!(
                    "{policy_name}: tier {tier} {} -> {}",
                    active_target.map_or_else(|| "unset".into(), ToString::to_string),
                    candidate_target.map_or_else(|| "unset".into(), ToString::to_string),
                ));
            }
        }
    }
    explanations.extend(
        diff_progress_guards(active, candidate)
            .into_iter()
            .map(|difference| {
                format!(
                    "{}: {} {} -> {}",
                    difference.policy,
                    difference.field,
                    difference.active_value.as_deref().unwrap_or("unset"),
                    difference.candidate_value.as_deref().unwrap_or("unset")
                )
            }),
    );
    explanations
}

fn guard_fields(guard: Option<&ProgressGuardPolicy>) -> BTreeMap<String, String> {
    let Some(guard) = guard else {
        return BTreeMap::new();
    };
    let optional = |value: Option<u64>| value.map(|value| value.to_string());
    let mut fields = BTreeMap::from([
        (
            "progress_guard.escalation_tier".to_string(),
            guard.escalation_tier.clone(),
        ),
        (
            "progress_guard.protected_tiers".to_string(),
            guard
                .protected_tiers
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(","),
        ),
        (
            "progress_guard.hold_for_requests".to_string(),
            guard.hold_for_requests.to_string(),
        ),
        (
            "progress_guard.incomplete_history".to_string(),
            match guard.incomplete_history {
                crate::trajectory::guard::IncompleteHistoryAction::Observe => "observe",
                crate::trajectory::guard::IncompleteHistoryAction::Escalate => "escalate",
            }
            .to_string(),
        ),
    ]);
    for (field, value) in [
        (
            "progress_guard.max_consecutive_unprotected",
            optional(guard.max_consecutive_unprotected),
        ),
        (
            "progress_guard.max_same_projection_unprotected",
            optional(guard.max_same_projection_unprotected),
        ),
        (
            "progress_guard.max_recovery_count",
            optional(guard.max_recovery_count),
        ),
        (
            "progress_guard.max_episode_requests",
            optional(guard.max_episode_requests),
        ),
        (
            "progress_guard.max_episode_elapsed_ms",
            optional(guard.max_episode_elapsed_ms),
        ),
        (
            "progress_guard.max_episode_cost_micro_usd",
            optional(guard.max_episode_cost_micro_usd),
        ),
    ] {
        if let Some(value) = value {
            fields.insert(field.to_string(), value);
        }
    }
    fields
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
    let _publication_lock = acquire_publication_lock(active_path)?;
    publish_candidate_unlocked(active_path, expected_digest, candidate, history_dir)
}

/// Publish while the caller holds [`acquire_publication_lock`] for `active_path`.
pub fn publish_candidate_unlocked(
    active_path: &Path,
    expected_digest: &str,
    candidate: &PolicyLock,
    history_dir: &Path,
) -> Result<PromotionRecord> {
    let candidate_bytes = deterministic_yaml(candidate)?.into_bytes();
    publish_bytes_unlocked(
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
    let _publication_lock = acquire_publication_lock(active_path)?;
    rollback_to_digest_unlocked(active_path, expected_digest, target_digest, history_dir)
}

/// Restore history while the caller holds [`acquire_publication_lock`] for `active_path`.
pub fn rollback_to_digest_unlocked(
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
    publish_bytes_unlocked(
        active_path,
        expected_digest,
        &target_bytes,
        &target,
        history_dir,
        "rollback",
    )
}

fn publish_bytes_unlocked(
    active_path: &Path,
    expected_digest: &str,
    target_bytes: &[u8],
    target: &PolicyLock,
    history_dir: &Path,
    action: &str,
) -> Result<PromotionRecord> {
    if target
        .artifact
        .as_ref()
        .is_some_and(|artifact| artifact.compiler.id == OPTIMIZATION_EXPERIMENT_COMPILER_ID)
    {
        anyhow::bail!("private optimization experiment policy locks cannot be published");
    }
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
    write_bytes_atomic_unlocked(active_path, target_bytes)?;
    let record = PromotionRecord {
        action: action.to_string(),
        parent_digest,
        child_digest,
        recorded_at: chrono::Utc::now().to_rfc3339(),
    };
    append_promotion_record(history_dir, &record)?;
    Ok(record)
}

/// Load and verify one immutable policy history snapshot.
pub fn load_history_snapshot(history_dir: &Path, digest: &str) -> Result<PolicyLock> {
    validate_sha256_digest(digest, "policy history digest")?;
    let path = history_snapshot_path(history_dir, digest)?;
    let bytes = std::fs::read(&path)
        .with_context(|| format!("reading policy history {}", path.display()))?;
    let raw = std::str::from_utf8(&bytes).context("policy history is not UTF-8")?;
    let target: PolicyLock = serde_saphyr::from_str(raw)
        .with_context(|| format!("parsing policy history {}", path.display()))?;
    validate_document(&target)?;
    let actual = semantic_digest(&target)?;
    if actual != digest {
        anyhow::bail!(
            "policy history digest mismatch for {} (expected {digest}, found {actual})",
            path.display()
        );
    }
    Ok(target)
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

/// Ensure provider-qualified policy tiers remain routable after setup by
/// adding empty registry-backed provider stubs without reserializing the
/// operator's config. Existing provider bodies and comments are untouched.
pub fn edit_config_provider_stubs(raw: &str, providers: &[String]) -> Result<String> {
    let parsed = bitrouter_sdk::config::parse_with(raw, |_| {
        Some("bitrouter-provider-stub-validation".into())
    })
    .context("parsing bitrouter.yaml")?;
    let mut missing = providers
        .iter()
        .filter(|provider| !parsed.providers.contains_key(provider.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>();
    if missing.is_empty() {
        return Ok(raw.to_string());
    }
    if missing.iter().any(|provider| {
        provider.is_empty()
            || provider.len() > 128
            || !provider
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    }) {
        anyhow::bail!("provider ids must use ASCII letters, digits, '.', '_' or '-'");
    }

    let mut lines = source_lines(raw);
    if let Some((start, end)) = block_range(&lines, "providers", 0) {
        let inline_empty = lines[start]
            .split_once(':')
            .is_some_and(|(_, value)| matches!(value.trim(), "{}" | "{ }"));
        if inline_empty {
            lines[start] = "providers:".into();
        } else {
            require_block_header(&lines[start], "providers")?;
        }
        let insert_at = if inline_empty { start + 1 } else { end };
        for (offset, provider) in std::mem::take(&mut missing).into_iter().enumerate() {
            lines.insert(insert_at + offset, format!("  {provider}: {{}}"));
        }
    } else {
        if !lines.is_empty() && !lines.last().is_some_and(|line| line.is_empty()) {
            lines.push(String::new());
        }
        lines.push("providers:".into());
        for provider in missing {
            lines.push(format!("  {provider}: {{}}"));
        }
    }
    let edited = render_source_lines(lines, raw.ends_with('\n'));
    let checked = bitrouter_sdk::config::parse_with(&edited, |_| {
        Some("bitrouter-provider-stub-validation".into())
    })
    .context("validating provider stubs in bitrouter.yaml")?;
    if providers
        .iter()
        .any(|provider| !checked.providers.contains_key(provider))
    {
        anyhow::bail!("edited config did not retain every optimization provider");
    }
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
    initialize_files_with_efforts(
        config_path,
        policy_name,
        preset_name,
        strong_model,
        None,
        economy_model,
        None,
    )
    .await
}

/// Create one named policy whose tiers may identify the same model at distinct
/// exact effort levels. Scalar/model-only callers retain the historical form.
pub async fn initialize_files_with_efforts(
    config_path: &Path,
    policy_name: &str,
    preset_name: &str,
    strong_model: Option<&str>,
    strong_effort: Option<bitrouter_sdk::language_model::types::ReasoningEffort>,
    economy_model: &str,
    economy_effort: Option<bitrouter_sdk::language_model::types::ReasoningEffort>,
) -> Result<PolicyFileUpdate> {
    let _config_lock = acquire_publication_lock(config_path)?;
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config = bitrouter_sdk::config::parse(&raw).context("parsing bitrouter.yaml")?;
    let lock_path = resolve_path(&config, Some(config_path))
        .ok_or_else(|| anyhow::anyhow!("cannot resolve policy lock path"))?;
    let _policy_lock = acquire_publication_lock(&lock_path)?;
    initialize_files_unlocked(
        config_path,
        policy_name,
        preset_name,
        strong_model,
        strong_effort,
        economy_model,
        economy_effort,
    )
    .await
}

/// Initialize while the caller holds both config and policy publication locks.
pub async fn initialize_files_unlocked(
    config_path: &Path,
    policy_name: &str,
    preset_name: &str,
    strong_model: Option<&str>,
    strong_effort: Option<bitrouter_sdk::language_model::types::ReasoningEffort>,
    economy_model: &str,
    economy_effort: Option<bitrouter_sdk::language_model::types::ReasoningEffort>,
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
    if strong_model == economy_model && strong_effort == economy_effort {
        anyhow::bail!("strong and economy tiers must use different model/effort targets");
    }

    let mut capability_config = config.clone();
    crate::merge_registry_into(&mut capability_config).await;
    bitrouter_providers::apply_builtin_defaults(&mut capability_config);
    let mut tool_safe_tiers = vec!["strong".to_string()];
    if route_supports_capability(
        &capability_config,
        economy_model,
        bitrouter_sdk::language_model::types::Capability::Tools,
    ) {
        tool_safe_tiers.push("economy".to_string());
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
    let original_document = document.clone();
    document.policies.insert(
        policy_name.to_string(),
        PolicyDefinition {
            tiers: BTreeMap::from([
                (
                    "economy".into(),
                    economy_effort.map_or_else(
                        || PolicyModelTarget::from(economy_model),
                        |effort| PolicyModelTarget::ModelEffort {
                            model: economy_model.to_owned(),
                            effort,
                        },
                    ),
                ),
                (
                    "strong".into(),
                    strong_effort.map_or_else(
                        || PolicyModelTarget::from(strong_model.as_str()),
                        |effort| PolicyModelTarget::ModelEffort {
                            model: strong_model.clone(),
                            effort,
                        },
                    ),
                ),
            ]),
            default_tier: Some("strong".into()),
            tool_use_tier: Some("strong".into()),
            tool_safe_tiers,
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

    let digest = write_atomic_unlocked(&lock_path, expected_digest.as_deref(), &document)?;
    if let Err(config_error) = write_text_atomic_unlocked(config_path, &raw, &edited_config) {
        let policy_recovery = if expected_digest.is_some() {
            write_atomic_unlocked(&lock_path, Some(&digest), &original_document).map(|_| ())
        } else {
            match load(&lock_path).await {
                Ok(current) if current.digest == digest => std::fs::remove_file(&lock_path)
                    .with_context(|| format!("removing {}", lock_path.display())),
                Ok(_) => Err(anyhow::anyhow!(
                    "created policy changed before initialization recovery"
                )),
                Err(error) => Err(error.context("loading created policy for recovery")),
            }
        };
        return match policy_recovery {
            Ok(()) => Err(config_error.context("restored policy after config update failed")),
            Err(recovery) => Err(config_error.context(format!(
                "config update failed and policy recovery also failed: {recovery:#}"
            ))),
        };
    }
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

fn route_supports_capability(
    config: &bitrouter_sdk::config::Config,
    route: &str,
    capability: bitrouter_sdk::language_model::types::Capability,
) -> bool {
    let Some((provider_id, model_id)) = route.split_once(':') else {
        return false;
    };
    config.providers.get(provider_id).is_some_and(|provider| {
        provider.active
            && (provider.model_supports_capability(model_id, capability)
                || embedded_catalog_supports_capability(provider_id, model_id, capability))
    })
}

fn embedded_catalog_supports_capability(
    provider_id: &str,
    model_id: &str,
    capability: bitrouter_sdk::language_model::types::Capability,
) -> bool {
    let Ok(catalog) = serde_json::from_str::<serde_json::Value>(include_str!(
        "../../../dist/registry/models.json"
    )) else {
        return false;
    };
    catalog
        .get("data")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|models| {
            models.iter().any(|model| {
                let canonical_match =
                    model.get("id").and_then(serde_json::Value::as_str) == Some(model_id);
                model
                    .get("providers")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|providers| {
                        providers.iter().any(|provider| {
                            provider.get("provider").and_then(serde_json::Value::as_str)
                                == Some(provider_id)
                                && (canonical_match
                                    || provider
                                        .get("provider_model_id")
                                        .and_then(serde_json::Value::as_str)
                                        == Some(model_id))
                                && provider
                                    .get("capabilities")
                                    .and_then(serde_json::Value::as_array)
                                    .is_some_and(|capabilities| {
                                        capabilities
                                            .iter()
                                            .any(|item| item.as_str() == Some(capability.as_str()))
                                    })
                        })
                    })
            })
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
/// compiles from the frozen adequacy snapshot alone.
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
        path: loaded.path,
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
    let history_dir = default_history_dir(&active.path);
    let record = if lock_held {
        publish_candidate_unlocked(
            &active.path,
            parent_digest,
            &candidate.document,
            &history_dir,
        )?
    } else {
        publish_candidate(
            &active.path,
            parent_digest,
            &candidate.document,
            &history_dir,
        )?
    };
    Ok(PolicyFileUpdate {
        path: active.path,
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
    verify_document_evidence_with_config(config_path, &config, &loaded.document, loaded.digest)
        .await
}

/// Reconstruct evidence for a reviewed candidate before it becomes active.
pub async fn verify_document_evidence(
    config_path: &Path,
    document: &PolicyLock,
) -> Result<EvidenceVerification> {
    validate_for_config(
        &bitrouter_sdk::config::parse(
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
    let config = bitrouter_sdk::config::parse(&raw).context("parsing bitrouter.yaml")?;
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
    let legacy = match artifact
        .migration
        .as_ref()
        .map(|migration| migration.source)
    {
        Some(LegacyEvidenceSource::SealedEmpty) => crate::policy_compile::LegacyAdequacySnapshot {
            snapshot_time_unix_ms: artifact.source_snapshot_time_unix_ms,
            pins: Vec::new(),
            exploration: Vec::new(),
            semantic_successes: Vec::new(),
            reliability_events: Vec::new(),
        },
        _ => {
            crate::policy_compile::LegacyAdequacySnapshot::load(
                &AdequacyStore::new(db.clone()),
                artifact.source_snapshot_time_unix_ms,
            )
            .await?
        }
    };
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
        policy_digest,
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
    if model.trim().is_empty()
        || model.starts_with('@')
        || bitrouter_sdk::config::presets::is_reserved(model)
    {
        anyhow::bail!("{tier} model must be a non-empty model id, not a preset");
    }
    Ok(())
}

/// Publish a main-config edit only if the file still matches the caller's
/// snapshot. File permissions are retained across the atomic replacement.
pub fn write_text_atomic(path: &Path, expected: &str, updated: &str) -> Result<()> {
    let _publication_lock = acquire_publication_lock(path)?;
    write_text_atomic_unlocked(path, expected, updated)
}

/// Replace text while the caller holds the target publication lock.
pub fn write_text_atomic_unlocked(path: &Path, expected: &str, updated: &str) -> Result<()> {
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
        replace_file_atomic(&tmp, path)
            .with_context(|| format!("publishing config {}", path.display()))?;
        sync_parent(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

pub fn write_bytes_atomic_unlocked(path: &Path, updated: &[u8]) -> Result<()> {
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
        replace_file_atomic(&tmp, path)
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

/// Durably create a file without ever exposing partial contents or replacing
/// an existing owner-controlled file.
pub fn write_new_file_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating directory {}", parent.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary file in {}", parent.display()))?;
    temporary
        .write_all(contents)
        .with_context(|| format!("writing temporary file for {}", path.display()))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("syncing temporary file for {}", path.display()))?;
    temporary.persist_noclobber(path).map_err(|error| {
        if path.exists() {
            anyhow::anyhow!("{} already exists; refusing to overwrite", path.display())
        } else {
            anyhow::Error::new(error.error)
                .context(format!("publishing new file {}", path.display()))
        }
    })?;
    sync_parent(path)
}

pub fn acquire_publication_lock(path: &Path) -> Result<std::fs::File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating policy directory {}", parent.display()))?;
    }
    let canonical = canonical_lock_target(path)?;
    let file_name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("policy-lock.yaml");
    let lock_path = canonical.with_file_name(format!(".{file_name}.bitrouter.lock"));
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

pub fn try_acquire_publication_lock(path: &Path) -> Result<std::fs::File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating policy directory {}", parent.display()))?;
    }
    let canonical = canonical_lock_target(path)?;
    let file_name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("policy-lock.yaml");
    let lock_path = canonical.with_file_name(format!(".{file_name}.bitrouter.lock"));
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening publication lock {}", lock_path.display()))?;
    lock.try_lock().with_context(|| {
        format!(
            "another operation is already running for {}",
            canonical.display()
        )
    })?;
    Ok(lock)
}

fn canonical_lock_target(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return std::fs::canonicalize(path)
            .with_context(|| format!("canonicalizing lock target {}", path.display()));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let parent = std::fs::canonicalize(parent)
        .with_context(|| format!("canonicalizing lock parent {}", parent.display()))?;
    Ok(parent.join(path.file_name().unwrap_or_default()))
}

#[cfg(not(windows))]
fn replace_file_atomic(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file_atomic(source: &Path, destination: &Path) -> std::io::Result<()> {
    atomicwrites::replace_atomic(source, destination)
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
    write_atomic_unlocked(path, expected_digest, document)
}

/// Replace a policy document while the caller holds its publication lock.
pub fn write_atomic_unlocked(
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
        replace_file_atomic(&tmp, path)
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
    continuation_config: bitrouter_sdk::config::ContinuationConfig,
    trajectory_config: TrajectoryConfig,
    trajectory: Option<Arc<TrajectoryRuntime>>,
}

#[derive(Debug)]
struct PredictiveSingleTargetDispatch;

/// Prevent a predictive named-policy decision from inheriting the SDK's
/// generic cross-account fallback chain. Without an authoritative proof that a
/// failed provider accepted no generation and incurred no charge, a semantic
/// request may be dispatched to exactly one physical target.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PredictiveSingleTargetRouteHook;

#[async_trait::async_trait]
impl RouteHook for PredictiveSingleTargetRouteHook {
    async fn resolve(
        &self,
        chain: &mut Vec<RoutingTarget>,
        ctx: &mut PipelineContext,
    ) -> bitrouter_sdk::Result<()> {
        if ctx.extension::<PredictiveSingleTargetDispatch>().is_some() {
            chain.truncate(1);
        }
        Ok(())
    }
}

impl PolicyRuntime {
    pub(crate) async fn new(
        config: &Config,
        config_path: Option<&Path>,
        db: DatabaseConnection,
        decision_recorder: Option<Arc<PolicyDecisionJsonlRecorder>>,
        eval_decisions: PendingEvalDecisionStore,
        trajectory: Option<Arc<TrajectoryRuntime>>,
    ) -> Result<Arc<Self>> {
        let runtime = Arc::new(Self {
            snapshot: RwLock::new(Arc::new(PolicySnapshot::default())),
            db,
            decision_recorder,
            eval_decisions,
            continuation_config: config.continuation.clone(),
            trajectory_config: config.trajectory.clone(),
            trajectory,
        });
        runtime.reload_for_config(config, config_path).await?;
        Ok(runtime)
    }

    #[cfg(test)]
    pub(crate) fn trajectory_is_active(&self) -> bool {
        self.trajectory.is_some()
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
        if config.continuation != self.continuation_config {
            anyhow::bail!(
                "changing any continuation setting requires a daemon restart; live continuation state was not changed"
            )
        }
        if config.trajectory != self.trajectory_config {
            anyhow::bail!(
                "changing any trajectory setting requires a daemon restart; live policy state was not changed"
            )
        }
        if self.trajectory_config.enabled != self.trajectory.is_some() {
            anyhow::bail!(
                "changing trajectory.enabled requires a daemon restart; live policy state was not changed"
            )
        }
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
                let mut router = PolicyTableRouter::new(table)
                    .with_state_namespace(name.clone())
                    .with_progress_guard(definition.progress_guard.clone());
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

#[async_trait::async_trait]
impl ModelSelector for PolicyRuntime {
    async fn select_variant(
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
        let invocation = ctx
            .extension::<EvalInvocation>()
            .unwrap_or_else(|| Arc::new(EvalInvocation::new(ctx.caller().user_id())));
        if !ctx.has_event::<EvalInvocation>() {
            ctx.emit(invocation.as_ref().clone());
        }
        ctx.insert_extension(invocation.clone());
        let guard = router.progress_guard();
        if let (Some(trajectory), Some(guard)) = (&self.trajectory, guard) {
            if ctx.caller().is_anonymous() {
                return Err(bitrouter_sdk::BitrouterError::Unauthorized(
                    "trajectory correlation requires an authenticated caller".into(),
                ));
            }
            let inbound_protocol = ctx.inbound_protocol().ok_or_else(|| {
                bitrouter_sdk::BitrouterError::bad_request(
                    "trajectory correlation requires an inbound protocol",
                )
            })?;
            let captured_at = trajectory_request_started_at(ctx).map_err(|error| {
                bitrouter_sdk::BitrouterError::internal(format!(
                    "trajectory request timestamp failed: {error}"
                ))
            })?;
            let input_model = ctx.model().to_string();
            let input_effort = ctx.prompt().params.reasoning_effort;
            let mut decision = router.candidate_for_guarded_policy(ctx.prompt(), ctx.headers());
            let projection = RouteProjection::parse_key(&decision.observed_route_projection)
                .ok_or_else(|| {
                    bitrouter_sdk::BitrouterError::internal(
                        "named policy produced an invalid observed route projection",
                    )
                })?;
            let policy_digest = snapshot.digest.as_deref().ok_or_else(|| {
                bitrouter_sdk::BitrouterError::internal(
                    "guarded named policy has no active lock digest",
                )
            })?;
            let policy_name = router.eval_policy_name().ok_or_else(|| {
                bitrouter_sdk::BitrouterError::internal(
                    "guarded named policy has no persisted evaluation identity",
                )
            })?;
            let request_key = decision.request_key.clone();
            let baseline_tier = router.eval_baseline_tier(&decision);
            let baseline_effort = baseline_tier
                .as_deref()
                .and_then(|tier| router.effort_of_tier(tier))
                .or(input_effort);
            let owner = ctx.caller().user_id();
            let trajectory_request_id = trajectory
                .request_identity(owner, ctx.request_id())
                .map_err(|error| {
                    bitrouter_sdk::BitrouterError::internal(format!(
                        "trajectory request identity failed: {error}"
                    ))
                })?;
            let guarded_input = GuardedRouteInput {
                route_event_id: stable_id("route-intent", owner, &trajectory_request_id),
                guard_event_id: stable_id("guard-activation", owner, &trajectory_request_id),
                policy_name: policy_name.to_owned(),
                route_projection: decision.route_projection.clone(),
                request_key,
                baseline_tier,
                baseline_effort,
                tier_efforts: router.effective_tier_efforts(input_effort),
                preset: Some(policy_name.to_owned()),
                projection,
                candidate_tier: decision.static_tier.clone(),
                policy_digest: policy_digest.to_owned(),
                policy: guard.clone(),
                carries_tools: !ctx.prompt().tools.is_empty(),
                tool_use_tier: router.tool_use_tier(),
                tool_safe_tiers: router.tool_safe_tiers(),
            };
            let (correlated, guarded) = trajectory
                .begin_guarded_request(
                    ctx.caller().user_id(),
                    ctx.request_id(),
                    inbound_protocol,
                    ctx.prompt(),
                    &captured_at,
                    guarded_input,
                )
                .await
                .map_err(|error| {
                    if error
                        .downcast_ref::<crate::trajectory::correlation::InvalidCorrelationEvidence>(
                        )
                        .is_some()
                    {
                        bitrouter_sdk::BitrouterError::bad_request(error.to_string())
                    } else {
                        bitrouter_sdk::BitrouterError::internal(format!(
                            "trajectory correlation failed: {error}"
                        ))
                    }
                })?;
            let guard_applied = guarded.intent.clauses.iter().any(|clause| {
                clause.clause_id.starts_with("progress_guard.")
                    && clause.disposition == RouteIntentClauseDisposition::Applied
            });
            router.apply_guarded_route(
                &mut decision,
                guarded.intent.selected_tier.as_deref(),
                guard_applied,
                guarded.tool_floor_applied,
            );
            decision.trajectory_episode_id = Some(guarded.snapshot.episode_id.clone());
            decision.trajectory_sequence = Some(guarded.route_sequence);
            decision.trajectory_completeness = Some(guarded.causal_completeness);
            decision.trajectory_health_digest =
                Some(guarded.intent.trajectory_snapshot_digest.clone());
            decision.progress_candidate_tier = guarded.intent.candidate_tier.clone();
            decision.progress_clause_ids = guarded
                .intent
                .clauses
                .iter()
                .map(|clause| clause.clause_id.clone())
                .collect();
            if let Some(plan) = ctx.extension::<ContinuationRequestPlan>()
                && let Some(adjustment) = plan.adjustment.as_ref()
            {
                router.apply_continuation_adjustment(&mut decision, adjustment)?;
            }
            let route_projection = decision.route_projection.clone();
            let selected = router.record_bound_policy_decision(
                &correlated.request_id,
                invocation.as_ref(),
                input_model,
                input_effort,
                decision,
                ctx.headers(),
            );
            if let Some(target) = selected {
                ctx.set_model(target.model());
                if let Some(effort) = target.effort() {
                    ctx.set_policy_reasoning_effort(effort);
                }
                if let Some(effort) = pinned_continuation_effort(ctx) {
                    ctx.set_policy_reasoning_effort_override(effort);
                }
                mark_predictive_single_target(&route_projection, ctx);
            }
            ctx.insert_extension(Arc::new(correlated));
            return Ok(());
        }
        let input_model = ctx.model().to_string();
        let input_effort = ctx.prompt().params.reasoning_effort;
        let mut decision = router.decision_for_bound_policy(ctx.prompt(), ctx.headers());
        if let Some(plan) = ctx.extension::<ContinuationRequestPlan>()
            && let Some(adjustment) = plan.adjustment.as_ref()
        {
            router.apply_continuation_adjustment(&mut decision, adjustment)?;
        }
        let route_projection = decision.route_projection.clone();
        let selected = router.record_bound_policy_decision(
            ctx.request_id(),
            invocation.as_ref(),
            input_model,
            input_effort,
            decision,
            ctx.headers(),
        );
        if let Some(target) = selected {
            ctx.set_model(target.model());
            if let Some(effort) = target.effort() {
                ctx.set_policy_reasoning_effort(effort);
            }
            if let Some(effort) = pinned_continuation_effort(ctx) {
                ctx.set_policy_reasoning_effort_override(effort);
            }
            mark_predictive_single_target(&route_projection, ctx);
        }
        Ok(())
    }
}

fn pinned_continuation_effort(
    ctx: &PipelineContext,
) -> Option<Option<bitrouter_sdk::language_model::types::ReasoningEffort>> {
    ctx.extension::<ContinuationRequestPlan>().and_then(|plan| {
        plan.adjustment
            .as_ref()
            .and_then(ContinuationAdjustment::pinned_effort_override)
    })
}

fn mark_predictive_single_target(route_projection: &str, ctx: &mut PipelineContext) {
    if PredictiveRouteProjection::parse_key(route_projection).is_some() {
        ctx.insert_extension(Arc::new(PredictiveSingleTargetDispatch));
    }
}

fn trajectory_request_started_at(ctx: &PipelineContext) -> Result<String> {
    trajectory_request_started_at_elapsed(chrono::Utc::now(), ctx.request_duration_ms())
}

fn trajectory_request_started_at_elapsed(
    observed_at: chrono::DateTime<chrono::Utc>,
    elapsed_ms: u64,
) -> Result<String> {
    let elapsed_ms =
        i64::try_from(elapsed_ms).context("pipeline request duration exceeds timestamp range")?;
    let started_at = observed_at
        .checked_sub_signed(chrono::TimeDelta::milliseconds(elapsed_ms))
        .ok_or_else(|| anyhow::anyhow!("pipeline request start exceeds timestamp range"))?;
    Ok(started_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
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

    #[test]
    fn trajectory_request_start_uses_pipeline_elapsed_time() -> anyhow::Result<()> {
        let observed_at = chrono::DateTime::parse_from_rfc3339("2026-08-06T10:00:00.050Z")?
            .with_timezone(&chrono::Utc);

        let started_at = trajectory_request_started_at_elapsed(observed_at, 25)?;

        assert_eq!(started_at, "2026-08-06T10:00:00.025Z");
        Ok(())
    }

    fn definition() -> PolicyDefinition {
        PolicyDefinition {
            tiers: BTreeMap::from([
                ("economy".into(), PolicyModelTarget::from("vendor:economy")),
                ("strong".into(), PolicyModelTarget::from("vendor:strong")),
            ]),
            routes: BTreeMap::new(),
            default_tier: Some("strong".into()),
            tool_use_tier: Some("strong".into()),
            tool_safe_tiers: vec!["strong".into()],
            ..Default::default()
        }
    }

    fn progress_guard() -> ProgressGuardPolicy {
        ProgressGuardPolicy {
            escalation_tier: "strong".into(),
            protected_tiers: BTreeSet::from(["strong".into()]),
            max_consecutive_unprotected: Some(3),
            max_same_projection_unprotected: Some(4),
            max_recovery_count: Some(1),
            max_episode_requests: Some(8),
            max_episode_elapsed_ms: None,
            max_episode_cost_micro_usd: Some(50_000),
            hold_for_requests: 2,
            incomplete_history: crate::trajectory::guard::IncompleteHistoryAction::Observe,
        }
    }

    const TEST_DIGEST: &str =
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn certificate(selected_tier: &str) -> PolicyCertificate {
        PolicyCertificate {
            owner: RouteOwner::Compiler,
            selected_tier: selected_tier.to_string(),
            baseline_tier: Some("strong".to_string()),
            source: CertificateSource::LegacyAdequacyV1,
            eligible_episodes: 1,
            independent_tasks: 1,
            quality: None,
            economics: None,
            latency: None,
            critical_violations: 0,
            verdict: PromotionVerdict::Promote,
            evaluator_config_digest: None,
            compiler_config_digest: TEST_DIGEST.to_string(),
            evidence_digest: TEST_DIGEST.to_string(),
            legacy: None,
        }
    }

    fn task_aware_lock(routes: BTreeMap<String, String>) -> PolicyLock {
        let mut policy = definition();
        policy.routes = routes.clone();
        policy.predictor = Some(compiled_predictor_contract());
        let certificates = routes
            .iter()
            .map(|(key, tier)| (key.clone(), certificate(tier)))
            .collect();
        PolicyLock {
            lockfile_version: EVIDENCE_POLICY_LOCKFILE_VERSION,
            artifact: Some(PolicyArtifact::empty()),
            policies: BTreeMap::from([("coding".to_string(), policy)]),
            certificates: BTreeMap::from([("coding".to_string(), certificates)]),
        }
    }

    #[test]
    fn unified_v1_predictive_routes_require_a_predictor() {
        let mut lock = task_aware_lock(BTreeMap::from([(
            "agent_route/v1|code:review|verify|normal".to_string(),
            "economy".to_string(),
        )]));
        lock.policies
            .get_mut("coding")
            .expect("test policy")
            .predictor = None;

        let error = validate_document(&lock)
            .expect_err("predictive routes without a predictor contract must be rejected");

        assert!(error.to_string().contains("predictor"));
    }

    #[test]
    fn unified_v1_predictor_contract_rejects_malformed_task_family_route_keys() {
        let lock = task_aware_lock(BTreeMap::from([(
            "agent_route/v1|code:not_a_family|verify|normal".to_string(),
            "economy".to_string(),
        )]));

        let error = validate_document(&lock)
            .expect_err("malformed task-family route keys must not be admitted");

        assert!(format!("{error:#}").contains("canonical"));
    }

    #[test]
    fn unified_v1_predictor_contract_accepts_baseline_and_task_routes() -> anyhow::Result<()> {
        let lock = task_aware_lock(BTreeMap::from([
            (
                "agent_route/v1|unknown|verify|normal".to_string(),
                "economy".to_string(),
            ),
            (
                "agent_route/v1|code:review|verify|normal".to_string(),
                "strong".to_string(),
            ),
        ]));

        validate_document(&lock)
    }

    #[test]
    fn prior_predictor_contract_is_rejected_for_unified_v1_routes() -> anyhow::Result<()> {
        let legacy = PredictorContract {
            algorithm: "deterministic_scorecard".into(),
            version: 1,
            config_digest:
                "sha256:7483fb5fa02c0141f568b82287234895c666fef426789e32783bdd3a00cea3ec".into(),
            confidence_kind: "heuristic_margin".into(),
            calibration_digest: None,
        };
        let mut v1_only = task_aware_lock(BTreeMap::from([(
            "agent_route/v1|unknown|verify|normal".to_string(),
            "economy".to_string(),
        )]));
        v1_only
            .policies
            .get_mut("coding")
            .ok_or_else(|| anyhow::anyhow!("test policy missing"))?
            .predictor = Some(legacy.clone());
        assert!(validate_document(&v1_only).is_err());

        for changed in [
            PredictorContract {
                algorithm: "different_scorecard".into(),
                ..legacy.clone()
            },
            PredictorContract {
                config_digest: TEST_DIGEST.into(),
                ..legacy.clone()
            },
            PredictorContract {
                version: 2,
                ..legacy.clone()
            },
            PredictorContract {
                confidence_kind: "calibrated".into(),
                ..legacy.clone()
            },
            PredictorContract {
                calibration_digest: Some(TEST_DIGEST.into()),
                ..legacy.clone()
            },
        ] {
            let mut changed_lock = v1_only.clone();
            changed_lock
                .policies
                .get_mut("coding")
                .ok_or_else(|| anyhow::anyhow!("test policy missing"))?
                .predictor = Some(changed);
            assert!(validate_document(&changed_lock).is_err());
        }
        Ok(())
    }

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
      agent_route/v1|unknown|implement|normal: economy
    default_tier: strong
    predictor:
      algorithm: deterministic_scorecard
      version: 1
      config_digest: "sha256:7039bc16f3ac2e306d7855a193aee8bb4cd4395a92a58a09768d60d628f70f37"
      confidence_kind: heuristic_margin
certificates:
  coding:
    agent_route/v1|unknown|implement|normal:
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

    /// A tier may not target the reserved namespace. `bitrouter/auto` resolves
    /// back through the very policy whose tier names it, so accepting it would
    /// sign a self-referential lock. The `@`-only guard used to let it through
    /// because the public spelling carries neither an `@` nor a colon.
    #[test]
    fn tier_target_cannot_reference_the_reserved_namespace() {
        let mut policy = definition();
        policy
            .tiers
            .insert("strong".into(), "bitrouter/auto".into());
        let lock = PolicyLock {
            lockfile_version: 1,
            artifact: None,
            policies: BTreeMap::from([("auto".into(), policy)]),
            certificates: BTreeMap::new(),
        };

        let error = validate_document(&lock).unwrap_err().to_string();
        assert!(
            error.contains("cannot reference another preset"),
            "unexpected error: {error}"
        );
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
    fn progress_guard_is_optional_signed_and_deterministic() -> anyhow::Result<()> {
        let old = PolicyLock {
            lockfile_version: 1,
            artifact: None,
            policies: BTreeMap::from([("coding".into(), definition())]),
            certificates: BTreeMap::new(),
        };
        let old_bytes = deterministic_yaml(&old)?;
        assert!(!old_bytes.contains("progress_guard"));
        let old_round_trip: PolicyLock = serde_saphyr::from_str(&old_bytes)?;
        assert_eq!(deterministic_yaml(&old_round_trip)?, old_bytes);

        let mut guarded = PolicyLock::default();
        let mut policy = definition();
        policy.routes.clear();
        policy.progress_guard = Some(crate::trajectory::guard::ProgressGuardPolicy {
            escalation_tier: "strong".into(),
            protected_tiers: BTreeSet::from(["strong".into()]),
            max_consecutive_unprotected: Some(3),
            max_same_projection_unprotected: None,
            max_recovery_count: Some(1),
            max_episode_requests: None,
            max_episode_elapsed_ms: None,
            max_episode_cost_micro_usd: Some(5_000),
            hold_for_requests: 2,
            incomplete_history: crate::trajectory::guard::IncompleteHistoryAction::Escalate,
        });
        guarded.policies.insert("coding".into(), policy);
        let guarded_bytes = deterministic_yaml(&guarded)?;
        let parsed: PolicyLock = serde_saphyr::from_str(&guarded_bytes)?;
        assert_eq!(deterministic_yaml(&parsed)?, guarded_bytes);
        assert_ne!(
            semantic_digest(&guarded)?,
            semantic_digest(&PolicyLock::default())?
        );
        assert!(guarded_bytes.contains("progress_guard:"));
        Ok(())
    }

    #[test]
    fn progress_guard_validation_is_named_v2_and_non_downgrading() -> anyhow::Result<()> {
        let guarded_lock = |guard: ProgressGuardPolicy, mutate: fn(&mut PolicyDefinition)| {
            let mut lock = PolicyLock::default();
            let mut policy = definition();
            policy.routes.clear();
            policy.progress_guard = Some(guard);
            mutate(&mut policy);
            lock.policies.insert("coding".into(), policy);
            lock
        };

        let mut v1 = guarded_lock(progress_guard(), |_| {});
        v1.lockfile_version = LEGACY_POLICY_LOCKFILE_VERSION;
        v1.artifact = None;
        assert!(validate_document(&v1).is_err());

        let mut unknown_escalation = progress_guard();
        unknown_escalation.escalation_tier = "missing".into();
        assert!(validate_document(&guarded_lock(unknown_escalation, |_| {})).is_err());

        let mut empty_protected = progress_guard();
        empty_protected.protected_tiers.clear();
        assert!(validate_document(&guarded_lock(empty_protected, |_| {})).is_err());

        let mut zero_threshold = progress_guard();
        zero_threshold.max_episode_requests = Some(0);
        assert!(validate_document(&guarded_lock(zero_threshold, |_| {})).is_err());

        let mut zero_hold = progress_guard();
        zero_hold.hold_for_requests = 0;
        assert!(validate_document(&guarded_lock(zero_hold, |_| {})).is_err());

        let unprotected_floor = guarded_lock(progress_guard(), |policy| {
            policy.tiers.insert("floor".into(), "vendor:floor".into());
            policy.tool_use_tier = Some("floor".into());
            policy.tool_safe_tiers = vec!["floor".into()];
        });
        assert!(validate_document(&unprotected_floor).is_err());

        let legacy_global = bitrouter_sdk::config::parse(
            "policy_table:\n  progress_guard:\n    escalation_tier: strong\n",
        );
        assert!(legacy_global.is_err());
        Ok(())
    }

    #[test]
    fn progress_guard_requires_explicit_trajectory_activation() -> anyhow::Result<()> {
        let mut policy = definition();
        policy.routes.clear();
        policy.progress_guard = Some(progress_guard());
        let mut lock = PolicyLock::default();
        lock.policies.insert("coding".into(), policy);

        let disabled = Config::default();
        let error = validate_for_config(&disabled, &lock).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("progress_guard requires trajectory.enabled: true"),
            "got: {error}"
        );

        let mut enabled = Config::default();
        enabled.trajectory.enabled = true;
        validate_for_config(&enabled, &lock)?;
        Ok(())
    }

    #[test]
    fn guard_diff_reports_every_changed_field() {
        let mut active = PolicyLock::default();
        let mut policy = definition();
        policy.routes.clear();
        active.policies.insert("coding".into(), policy.clone());
        policy.progress_guard = Some(progress_guard());
        let mut candidate = PolicyLock::default();
        candidate.policies.insert("coding".into(), policy);

        let differences = diff_progress_guards(&active, &candidate);
        let fields = differences
            .iter()
            .map(|difference| difference.field.as_str())
            .collect::<BTreeSet<_>>();
        for field in [
            "progress_guard.escalation_tier",
            "progress_guard.protected_tiers",
            "progress_guard.max_consecutive_unprotected",
            "progress_guard.max_same_projection_unprotected",
            "progress_guard.max_recovery_count",
            "progress_guard.max_episode_requests",
            "progress_guard.max_episode_cost_micro_usd",
            "progress_guard.hold_for_requests",
            "progress_guard.incomplete_history",
        ] {
            assert!(fields.contains(field), "missing {field}");
        }
    }

    #[test]
    fn effort_only_tier_diff_is_visible() {
        let mut active = PolicyLock::default();
        let mut active_policy = definition();
        active_policy.routes.clear();
        active_policy.tiers.insert(
            "strong".into(),
            PolicyModelTarget::ModelEffort {
                model: "vendor:same".into(),
                effort: ReasoningEffort::High,
            },
        );
        active
            .policies
            .insert("coding".into(), active_policy.clone());

        active_policy.tiers.insert(
            "strong".into(),
            PolicyModelTarget::ModelEffort {
                model: "vendor:same".into(),
                effort: ReasoningEffort::Low,
            },
        );
        let mut candidate = PolicyLock::default();
        candidate.policies.insert("coding".into(), active_policy);

        assert_eq!(
            diff_explanations(&active, &candidate),
            ["coding: tier strong vendor:same@high -> vendor:same@low"]
        );
    }

    #[tokio::test]
    async fn publish_reload_and_rollback_preserve_guard_bytes() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let active_path = dir.path().join("policy-lock.yaml");
        let history = dir.path().join("history");
        let mut active = PolicyLock::default();
        let mut policy = definition();
        policy.routes.clear();
        active.policies.insert("coding".into(), policy.clone());
        write_atomic(&active_path, None, &active)?;
        let active_bytes = std::fs::read(&active_path)?;

        policy.progress_guard = Some(progress_guard());
        let mut candidate = active.clone();
        candidate.policies.insert("coding".into(), policy);
        let promoted = publish_candidate(
            &active_path,
            &semantic_digest(&active)?,
            &candidate,
            &history,
        )?;
        let loaded = load(&active_path).await?;
        assert_eq!(loaded.document, candidate);
        rollback_to_digest(
            &active_path,
            &promoted.child_digest,
            &promoted.parent_digest,
            &history,
        )?;
        assert_eq!(std::fs::read(&active_path)?, active_bytes);
        Ok(())
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
        let ledger_key = "coding\0agent_route/v1|unknown|unknown|normal";
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
                proposed_progress_guards: None,
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
            None,
        )
        .await?;
        let mut frozen = context();
        runtime.select_variant("coding", None, &mut frozen).await?;
        assert_eq!(frozen.model(), "vendor:economy");

        let empty_target_db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&empty_target_db).await?;
        let empty_target = PolicyRuntime::new(
            &config,
            Some(&config_path),
            empty_target_db,
            None,
            PendingEvalDecisionStore::default(),
            None,
        )
        .await?;
        let mut copied = context();
        empty_target
            .select_variant("coding", None, &mut copied)
            .await?;
        assert_eq!(copied.model(), frozen.model());

        config.policy.mode = PolicyRuntimeMode::Adaptive;
        runtime
            .reload_for_config(&config, Some(&config_path))
            .await?;
        let mut adaptive = context();
        runtime
            .select_variant("coding", None, &mut adaptive)
            .await?;
        assert_eq!(adaptive.model(), "vendor:economy");

        store.upsert_pin(ledger_key, 1_700_000_200).await?;
        store.upsert_exploration(ledger_key, 99, 0, false).await?;
        let mut after_database_mutation = context();
        runtime
            .select_variant("coding", None, &mut after_database_mutation)
            .await?;
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
            None,
        )
        .await?;

        let mut exact = context();
        runtime
            .select_variant("auto", Some("cost"), &mut exact)
            .await?;
        assert_eq!(exact.model(), "vendor:economy");
        let mut fallback = context();
        runtime
            .select_variant("auto", Some("latency"), &mut fallback)
            .await?;
        assert_eq!(fallback.model(), "vendor:strong");
        Ok(())
    }

    #[tokio::test]
    async fn policy_runtime_trajectory_is_optional_and_metadata_is_noncausal() -> anyhow::Result<()>
    {
        use bitrouter_sdk::caller::CallerContext;
        use bitrouter_sdk::language_model::{
            ApiProtocol, GenerationParams, Message, PipelineRequest, Prompt, Role,
        };
        use http::HeaderValue;
        use sea_orm::ConnectionTrait;

        use crate::trajectory::canonical::{Canonicalizer, CorrelationKey};
        use crate::trajectory::correlation::{CorrelatedRequest, TrajectoryRuntime};
        use crate::trajectory::store::TrajectoryStore;

        fn context_with_previous_response_id(
            headers: http::HeaderMap,
            request_id: &str,
            previous_response_id: Option<serde_json::Value>,
        ) -> PipelineContext {
            let is_responses = previous_response_id.is_some();
            let mut prompt = Prompt {
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
            if let Some(previous_response_id) = previous_response_id {
                prompt
                    .params
                    .extra
                    .insert("previous_response_id".into(), previous_response_id);
            }
            let mut request = PipelineRequest::new(
                "vendor:strong",
                CallerContext::new("key-a", "owner-a"),
                prompt,
            );
            request.request_id = request_id.into();
            request.headers = headers;
            request.inbound_protocol = Some(if is_responses {
                ApiProtocol::Responses
            } else {
                ApiProtocol::ChatCompletions
            });
            PipelineContext::new(request)
        }

        fn context(headers: http::HeaderMap) -> PipelineContext {
            context_with_previous_response_id(headers, "request-stable", None)
        }

        fn trajectory(db: sea_orm::DatabaseConnection) -> anyhow::Result<Arc<TrajectoryRuntime>> {
            Ok(Arc::new(TrajectoryRuntime::new(
                TrajectoryStore::new(db),
                Canonicalizer::new(CorrelationKey::from_bytes([31; 32])?),
            )))
        }

        let old_dir = tempfile::tempdir()?;
        let old_config_path = old_dir.path().join("bitrouter.yaml");
        tokio::fs::write(
            &old_config_path,
            "trajectory:\n  enabled: true\npresets:\n  coding:\n    model: vendor:strong\n    policy: coding\n",
        )
        .await?;
        let old_lock = PolicyLock {
            lockfile_version: 1,
            artifact: None,
            policies: BTreeMap::from([("coding".into(), definition())]),
            certificates: BTreeMap::new(),
        };
        write_atomic(&old_dir.path().join("policy-lock.yaml"), None, &old_lock)?;
        let old_config = bitrouter_sdk::config::load(&old_config_path).await?;
        let old_db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&old_db).await?;
        let old_runtime = PolicyRuntime::new(
            &old_config,
            Some(&old_config_path),
            old_db.clone(),
            None,
            PendingEvalDecisionStore::default(),
            Some(trajectory(old_db.clone())?),
        )
        .await?;
        let mut old_context =
            context_with_previous_response_id(http::HeaderMap::new(), "request-old-no-guard", None);
        old_runtime
            .select_variant("coding", None, &mut old_context)
            .await?;
        assert_eq!(old_context.model(), "vendor:strong");
        assert!(old_context.extension::<CorrelatedRequest>().is_none());
        assert!(
            TrajectoryStore::new(old_db)
                .request("owner-a", "request-old-no-guard")
                .await?
                .is_none()
        );

        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("bitrouter.yaml");
        tokio::fs::write(
            &config_path,
            "trajectory:\n  enabled: true\npresets:\n  coding:\n    model: vendor:strong\n    policy: coding\n",
        )
        .await?;
        let mut guarded_definition = definition();
        guarded_definition.routes.clear();
        guarded_definition.progress_guard = Some(ProgressGuardPolicy {
            escalation_tier: "strong".into(),
            protected_tiers: BTreeSet::from(["strong".into()]),
            max_consecutive_unprotected: Some(3),
            max_same_projection_unprotected: Some(3),
            max_recovery_count: Some(2),
            max_episode_requests: Some(10),
            max_episode_elapsed_ms: None,
            max_episode_cost_micro_usd: None,
            hold_for_requests: 2,
            incomplete_history: crate::trajectory::guard::IncompleteHistoryAction::Observe,
        });
        let mut lock = PolicyLock::default();
        lock.policies.insert("coding".into(), guarded_definition);
        write_atomic(&dir.path().join("policy-lock.yaml"), None, &lock)?;
        let config = bitrouter_sdk::config::load(&config_path).await?;

        let disabled_db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&disabled_db).await?;
        let disabled = PolicyRuntime::new(
            &config,
            Some(&config_path),
            disabled_db.clone(),
            None,
            PendingEvalDecisionStore::default(),
            None,
        )
        .await;
        let Err(disabled_error) = disabled else {
            anyhow::bail!("enabled guarded policy must reject missing trajectory runtime")
        };
        assert!(
            disabled_error
                .to_string()
                .contains("requires a daemon restart")
        );
        assert!(
            TrajectoryStore::new(disabled_db)
                .request("owner-a", "request-stable")
                .await?
                .is_none()
        );

        let baseline_db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&baseline_db).await?;
        let decision_path = dir.path().join("guarded-decisions.jsonl");
        let baseline = PolicyRuntime::new(
            &config,
            Some(&config_path),
            baseline_db.clone(),
            Some(Arc::new(PolicyDecisionJsonlRecorder::new(
                decision_path.clone(),
            )?)),
            PendingEvalDecisionStore::default(),
            Some(trajectory(baseline_db.clone())?),
        )
        .await?;
        let mut preauth_context = context(http::HeaderMap::new());
        preauth_context.set_caller(CallerContext::anonymous());
        let preauth_error = baseline
            .select_variant("coding", None, &mut preauth_context)
            .await
            .expect_err("trajectory runtime must not run before authentication");
        assert_eq!(preauth_error.status(), http::StatusCode::UNAUTHORIZED);
        assert!(
            TrajectoryStore::new(baseline_db.clone())
                .request("anonymous", "request-stable")
                .await?
                .is_none()
        );
        let mut baseline_context = context(http::HeaderMap::new());
        baseline
            .select_variant("coding", None, &mut baseline_context)
            .await?;
        let baseline_correlation = baseline_context
            .extension::<CorrelatedRequest>()
            .ok_or_else(|| anyhow::anyhow!("missing baseline correlation"))?;
        let records =
            crate::workflow_state::decision::PolicyDecisionRecord::load_jsonl(&decision_path)?;
        let guarded_record = records
            .last()
            .ok_or_else(|| anyhow::anyhow!("missing guarded decision record"))?;
        let guarded_events = TrajectoryStore::new(baseline_db.clone())
            .events_for_episode("owner-a", &baseline_correlation.episode_id)
            .await?;
        assert_eq!(guarded_events.len(), 2);
        assert_eq!(
            guarded_events
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![
                crate::trajectory::types::TrajectoryEventKind::RequestStarted,
                crate::trajectory::types::TrajectoryEventKind::RouteIntentRecorded,
            ],
            "the atomic route intent must exist when policy selection returns"
        );
        assert_eq!(
            guarded_record.trajectory_completeness.as_deref(),
            Some("complete")
        );
        assert!(guarded_record.trajectory_episode_id.is_some());
        assert_eq!(
            guarded_record.trajectory_sequence,
            guarded_events.last().map(|event| event.sequence)
        );
        assert!(guarded_record.trajectory_health_digest.is_some());
        assert_eq!(guarded_record.candidate_tier.as_deref(), Some("strong"));
        assert_eq!(guarded_record.progress_clause_ids.len(), 9);
        assert_eq!(
            guarded_record.request_id.as_deref(),
            Some(baseline_correlation.request_id.as_str())
        );
        assert!(!baseline_correlation.request_id.contains("request-stable"));

        let metadata_db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&metadata_db).await?;
        let metadata = PolicyRuntime::new(
            &config,
            Some(&config_path),
            metadata_db.clone(),
            None,
            PendingEvalDecisionStore::default(),
            Some(trajectory(metadata_db.clone())?),
        )
        .await?;
        let mut headers = http::HeaderMap::new();
        for (name, value) in [
            ("x-bitrouter-benchmark-id", "benchmark-other"),
            ("x-bitrouter-trial-id", "trial-other"),
            ("x-bitrouter-workflow", "workflow-other"),
            ("x-bitrouter-agent-role", "role-other"),
            ("x-superpowers-workflow", "superpowers-other"),
        ] {
            headers.insert(
                http::HeaderName::from_bytes(name.as_bytes())?,
                HeaderValue::from_str(value)?,
            );
        }
        let mut metadata_context = context(headers);
        metadata
            .select_variant("coding", None, &mut metadata_context)
            .await?;
        let metadata_correlation = metadata_context
            .extension::<CorrelatedRequest>()
            .ok_or_else(|| anyhow::anyhow!("missing metadata correlation"))?;

        assert_eq!(baseline_context.model(), metadata_context.model());
        assert_eq!(
            baseline_correlation.episode_id,
            metadata_correlation.episode_id
        );
        assert_eq!(baseline_correlation.evidence, metadata_correlation.evidence);
        assert!(
            TrajectoryStore::new(metadata_db)
                .request("owner-a", &metadata_correlation.request_id)
                .await?
                .is_some()
        );

        for (index, malformed) in [
            serde_json::json!(42),
            serde_json::Value::Null,
            serde_json::json!(" \n"),
        ]
        .into_iter()
        .enumerate()
        {
            let request_id = format!("request-malformed-{index}");
            let mut malformed_context = context_with_previous_response_id(
                http::HeaderMap::new(),
                &request_id,
                Some(malformed),
            );
            let error = baseline
                .select_variant("coding", None, &mut malformed_context)
                .await
                .expect_err("malformed previous_response_id must fail");
            assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
            assert!(
                TrajectoryStore::new(baseline_db.clone())
                    .request("owner-a", &request_id)
                    .await?
                    .is_none()
            );
        }

        baseline_db
            .execute(sea_orm::Statement::from_string(
                sea_orm::DatabaseBackend::Sqlite,
                "UPDATE trajectory_episodes SET next_sequence = 99".to_owned(),
            ))
            .await?;
        let mut corrupt_context = context_with_previous_response_id(
            http::HeaderMap::new(),
            "request-corrupt-storage",
            Some(serde_json::json!("request-stable")),
        );
        let storage_error = baseline
            .select_variant("coding", None, &mut corrupt_context)
            .await
            .expect_err("storage corruption must remain an internal error");
        assert_eq!(
            storage_error.status(),
            http::StatusCode::INTERNAL_SERVER_ERROR
        );
        Ok(())
    }

    #[tokio::test]
    async fn trajectory_config_change_requires_restart_and_preserves_lkg() -> anyhow::Result<()> {
        use crate::trajectory::canonical::{Canonicalizer, CorrelationKey};
        use crate::trajectory::correlation::TrajectoryRuntime;
        use crate::trajectory::store::TrajectoryStore;

        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let disabled_config = Config::default();
        let disabled = PolicyRuntime::new(
            &disabled_config,
            None,
            db.clone(),
            None,
            PendingEvalDecisionStore::default(),
            None,
        )
        .await?;
        let mut continuation_retention_change = disabled_config.clone();
        continuation_retention_change.continuation.retention_days = 31;
        let Err(continuation_error) = disabled
            .prepare_for_config(&continuation_retention_change, None)
            .await
        else {
            anyhow::bail!("changing continuation retention must require restart")
        };
        assert!(
            continuation_error
                .to_string()
                .contains("continuation setting requires a daemon restart")
        );

        let mut continuation_batch_change = disabled_config.clone();
        continuation_batch_change.continuation.prune_batch_size = 999;
        let Err(continuation_error) = disabled
            .prepare_for_config(&continuation_batch_change, None)
            .await
        else {
            anyhow::bail!("changing continuation prune batch must require restart")
        };
        assert!(
            continuation_error
                .to_string()
                .contains("continuation setting requires a daemon restart")
        );

        let mut enabled_config = Config::default();
        enabled_config.trajectory.enabled = true;
        let Err(enable_error) = disabled.prepare_for_config(&enabled_config, None).await else {
            anyhow::bail!("hot enabling trajectory must require restart")
        };
        assert!(
            enable_error
                .to_string()
                .contains("requires a daemon restart")
        );
        assert!(!disabled.trajectory_is_active());

        let mut disabled_retention_change = disabled_config.clone();
        disabled_retention_change.trajectory.retention_days = 31;
        let Err(retention_error) = disabled
            .prepare_for_config(&disabled_retention_change, None)
            .await
        else {
            anyhow::bail!("changing disabled trajectory retention must require restart")
        };
        assert!(
            retention_error
                .to_string()
                .contains("requires a daemon restart")
        );

        let mut disabled_batch_change = disabled_config.clone();
        disabled_batch_change.trajectory.outbox_batch_size = 101;
        let Err(batch_error) = disabled
            .prepare_for_config(&disabled_batch_change, None)
            .await
        else {
            anyhow::bail!("changing disabled trajectory batch size must require restart")
        };
        assert!(
            batch_error
                .to_string()
                .contains("requires a daemon restart")
        );

        let trajectory = Arc::new(TrajectoryRuntime::new(
            TrajectoryStore::new(db.clone()),
            Canonicalizer::new(CorrelationKey::from_bytes([41; 32])?),
        ));
        let enabled = PolicyRuntime::new(
            &enabled_config,
            None,
            db,
            None,
            PendingEvalDecisionStore::default(),
            Some(trajectory),
        )
        .await?;
        let Err(disable_error) = enabled.prepare_for_config(&disabled_config, None).await else {
            anyhow::bail!("hot disabling trajectory must require restart")
        };
        assert!(
            disable_error
                .to_string()
                .contains("requires a daemon restart")
        );
        assert!(enabled.trajectory_is_active());

        let mut enabled_retention_change = enabled_config.clone();
        enabled_retention_change.trajectory.retention_days = 31;
        let Err(retention_error) = enabled
            .prepare_for_config(&enabled_retention_change, None)
            .await
        else {
            anyhow::bail!("changing active trajectory retention must require restart")
        };
        assert!(
            retention_error
                .to_string()
                .contains("requires a daemon restart")
        );

        let mut enabled_batch_change = enabled_config;
        enabled_batch_change.trajectory.outbox_batch_size = 101;
        let Err(batch_error) = enabled
            .prepare_for_config(&enabled_batch_change, None)
            .await
        else {
            anyhow::bail!("changing active trajectory batch size must require restart")
        };
        assert!(
            batch_error
                .to_string()
                .contains("requires a daemon restart")
        );
        Ok(())
    }

    #[test]
    fn legacy_workflow_state_strategy_is_rejected() {
        let error = serde_saphyr::from_str::<PolicyLock>(
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
        .expect_err("retired workflow_state strategy must be rejected");
        assert!(error.to_string().contains("no longer supported"));
    }

    #[test]
    fn auto_router_template_lock_is_bound_and_canonical() -> anyhow::Result<()> {
        use bitrouter_sdk::language_model::{
            GenerationParams, Message, Prompt, Role,
            types::{Content, ProviderMetadata, ToolResultOutput},
        };

        let template_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("templates/auto-router");
        let config_raw = std::fs::read_to_string(template_dir.join("bitrouter.yaml"))?;
        let lock_raw = std::fs::read_to_string(template_dir.join("policy-lock.yaml"))?;
        let config = bitrouter_sdk::config::parse(&config_raw)?;
        let lock: PolicyLock = serde_saphyr::from_str(&lock_raw)?;

        assert_eq!(config.policy.mode, PolicyRuntimeMode::Frozen);
        assert!(!config_raw.contains("writeback:"));
        assert!(!lock_raw.contains("enabled:"));
        assert!(!lock_raw.contains("explore_enabled:"));
        let policy = &lock.policies["auto"];
        assert_eq!(policy.key_strategy, PolicyKeyStrategy::AgentTrace);
        assert_eq!(
            policy.tiers.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from(["balanced".into(), "economy".into(), "strong".into()])
        );
        assert_eq!(
            policy.tiers["balanced"].model(),
            "bitrouter:moonshotai/kimi-k3"
        );
        let predictive_routes = policy
            .routes
            .iter()
            .filter(|(key, _)| key.starts_with("agent_route/v1|"))
            .map(|(key, tier)| (key.clone(), tier.clone()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(predictive_routes.len(), 18);
        assert_eq!(
            predictive_routes,
            BTreeMap::from([
                (
                    "agent_route/v1|agent:web_research|mechanical|normal".into(),
                    "balanced".into(),
                ),
                (
                    "agent_route/v1|code:debugging|implement|guarded".into(),
                    "strong".into(),
                ),
                (
                    "agent_route/v1|code:review|verify|normal".into(),
                    "strong".into(),
                ),
                (
                    "agent_route/v1|unknown|finalize|context".into(),
                    "balanced".into(),
                ),
                (
                    "agent_route/v1|unknown|finalize|guarded".into(),
                    "strong".into(),
                ),
                (
                    "agent_route/v1|unknown|finalize|normal".into(),
                    "balanced".into(),
                ),
                (
                    "agent_route/v1|unknown|implement|context".into(),
                    "balanced".into(),
                ),
                (
                    "agent_route/v1|unknown|implement|guarded".into(),
                    "balanced".into(),
                ),
                (
                    "agent_route/v1|unknown|implement|normal".into(),
                    "balanced".into(),
                ),
                (
                    "agent_route/v1|unknown|mechanical|context".into(),
                    "balanced".into()
                ),
                (
                    "agent_route/v1|unknown|mechanical|guarded".into(),
                    "balanced".into()
                ),
                (
                    "agent_route/v1|unknown|mechanical|normal".into(),
                    "economy".into(),
                ),
                (
                    "agent_route/v1|unknown|orchestrate|context".into(),
                    "strong".into(),
                ),
                (
                    "agent_route/v1|unknown|orchestrate|guarded".into(),
                    "strong".into(),
                ),
                (
                    "agent_route/v1|unknown|orchestrate|normal".into(),
                    "strong".into(),
                ),
                (
                    "agent_route/v1|unknown|verify|context".into(),
                    "balanced".into(),
                ),
                (
                    "agent_route/v1|unknown|verify|guarded".into(),
                    "strong".into(),
                ),
                (
                    "agent_route/v1|unknown|verify|normal".into(),
                    "economy".into(),
                ),
            ])
        );
        let retired_prefix = format!("agent_route/{}|", ["v", "2"].concat());
        assert!(
            policy
                .routes
                .keys()
                .all(|key| !key.starts_with(&retired_prefix))
        );
        let router =
            PolicyTableRouter::from_config(&policy.as_table_config(PolicyRuntimeMode::Frozen))
                .ok_or_else(|| anyhow::anyhow!("auto template is missing policy tiers"))?;
        let fallback_prompt = Prompt {
            model: "incoming".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![
                Message::text(
                    Role::User,
                    "Implement a new module and refactor the parser API.",
                ),
                Message {
                    role: Role::Assistant,
                    content: vec![Content::ToolCall {
                        id: "call_read_file".into(),
                        name: "read_file".into(),
                        arguments: "{}".into(),
                        provider_executed: false,
                        dynamic: false,
                        provider_metadata: ProviderMetadata::new(),
                    }],
                },
                Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult {
                        call_id: "call_read_file".into(),
                        tool_name: None,
                        output: ToolResultOutput::Text {
                            value: "parser source".into(),
                        },
                        dynamic: false,
                        provider_metadata: ProviderMetadata::new(),
                    }],
                },
            ],
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        let fallback = router.decision_for(&fallback_prompt, &http::HeaderMap::new());
        assert_eq!(
            fallback.route_projection,
            "agent_route/v1|code:generation|implement|normal"
        );
        assert_eq!(
            fallback.request_key,
            "agent_route/v1|unknown|implement|normal"
        );
        assert_eq!(fallback.selected_tier.as_deref(), Some("balanced"));
        assert_eq!(policy.default_tier.as_deref(), Some("balanced"));
        assert_eq!(policy.tool_use_tier.as_deref(), Some("strong"));
        assert_eq!(policy.tool_safe_tiers, ["strong", "balanced", "economy"]);
        assert_eq!(
            policy.predictor.as_ref(),
            Some(&compiled_predictor_contract())
        );
        let guard = policy
            .progress_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("auto template is missing its progress guard"))?;
        assert_eq!(guard.escalation_tier, "strong");
        assert_eq!(guard.protected_tiers, BTreeSet::from(["strong".into()]));

        let rendered = deterministic_yaml(&lock)?;
        assert!(rendered.contains("key_strategy: agent_trace"));
        assert!(!rendered.contains("key_strategy: workflow_state"));
        validate_for_config(&config, &lock)?;
        Ok(())
    }

    #[test]
    fn predictive_routes_require_the_exact_compiled_predictor_contract() -> anyhow::Result<()> {
        let template_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("templates/auto-router");
        let lock_raw = std::fs::read_to_string(template_dir.join("policy-lock.yaml"))?;
        let lock: PolicyLock = serde_saphyr::from_str(&lock_raw)?;

        let mut missing = lock.clone();
        if let Some(policy) = missing.policies.get_mut("auto") {
            policy.predictor = None;
        }
        let missing_error = validate_document(&missing)
            .expect_err("predictive routes without a predictor contract must be rejected");
        assert!(missing_error.to_string().contains("predictor"));

        let mut mismatched = lock;
        if let Some(policy) = mismatched.policies.get_mut("auto")
            && let Some(predictor) = &mut policy.predictor
        {
            predictor.config_digest = EMPTY_SHA256.to_owned();
        }
        let mismatch_error = validate_document(&mismatched)
            .expect_err("a stale predictor contract must be rejected");
        assert!(
            mismatch_error
                .to_string()
                .contains(compiled_scorecard_digest())
        );
        Ok(())
    }

    #[test]
    fn predictive_routes_require_compiled_artifact_and_certificates() -> anyhow::Result<()> {
        let template_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("templates/auto-router");
        let lock_raw = std::fs::read_to_string(template_dir.join("policy-lock.yaml"))?;
        let mut lock: PolicyLock = serde_saphyr::from_str(&lock_raw)?;
        lock.lockfile_version = LEGACY_POLICY_LOCKFILE_VERSION;
        lock.artifact = None;
        lock.certificates.clear();
        if let Some(policy) = lock.policies.get_mut("auto") {
            policy.progress_guard = None;
        }

        let error = validate_document(&lock)
            .expect_err("predictive routes must not bypass compiled provenance metadata");
        assert!(
            error.to_string().contains("require policy lock v2"),
            "unexpected validation error: {error:#}"
        );
        Ok(())
    }

    #[test]
    fn auto_router_template_routes_have_deterministic_compiler_certificates() -> anyhow::Result<()>
    {
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
        let policy = &lock.policies["auto"];
        assert_eq!(certificates.len(), 18);
        assert_eq!(
            certificates.keys().collect::<Vec<_>>(),
            policy.routes.keys().collect::<Vec<_>>()
        );
        assert!(policy.routes.keys().all(|request_key| {
            PredictiveRouteProjection::parse_key(request_key).is_some()
                && certificates.contains_key(request_key)
        }));
        let compiler_digest = &lock
            .artifact
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("auto template is missing artifact metadata"))?
            .compiler
            .config_digest;
        let compiler = &lock
            .artifact
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("auto template is missing artifact metadata"))?
            .compiler;
        let expected_compiler_digest = canonical_template_digest(&(
            "auto-router-predictive-template-v1",
            compiler.id.as_str(),
            compiler.version,
            "auto",
            policy,
        ))?;
        assert_eq!(compiler_digest, &expected_compiler_digest);
        let mut route_evidence = BTreeMap::new();
        let mut configured_route_evidence = BTreeMap::new();
        for (request_key, certificate) in certificates {
            assert_eq!(certificate.owner, RouteOwner::Compiler);
            assert_eq!(certificate.source, CertificateSource::Mixed);
            assert_eq!(certificate.verdict, PromotionVerdict::Experiment);
            assert_eq!(certificate.selected_tier, policy.routes[request_key]);
            let projection =
                PredictiveRouteProjection::parse_key(request_key).ok_or_else(|| {
                    anyhow::anyhow!("template route '{request_key}' is not canonical")
                })?;
            if projection.task_family != crate::workflow_state::predictive::TaskFamily::Unknown {
                let baseline_key = projection.unknown_baseline().key();
                let baseline_tier = policy.routes.get(&baseline_key).ok_or_else(|| {
                    anyhow::anyhow!(
                        "template route '{request_key}' has no unknown-family baseline '{baseline_key}'"
                    )
                })?;
                assert_eq!(
                    certificate.baseline_tier.as_deref(),
                    Some(baseline_tier.as_str()),
                    "template route '{request_key}' must certify its unknown-family baseline tier"
                );
            }
            assert_eq!(&certificate.compiler_config_digest, compiler_digest);
            let expected_evidence_digest = canonical_template_digest(&(
                "auto-router-predictive-route-v1",
                "auto",
                request_key,
                certificate.selected_tier.as_str(),
            ))?;
            configured_route_evidence.insert(request_key, certificate.evidence_digest.clone());
            route_evidence.insert(request_key, expected_evidence_digest);
        }
        assert_eq!(configured_route_evidence, route_evidence);
        let evidence_root = &lock
            .artifact
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("auto template is missing artifact metadata"))?
            .evidence_root;
        let expected_evidence_root = canonical_template_digest(&(
            "auto-router-predictive-evidence-v1",
            "auto",
            route_evidence,
        ))?;
        assert_eq!(evidence_root, &expected_evidence_root);
        let rendered = deterministic_yaml(&lock)?;
        let reparsed: PolicyLock = serde_saphyr::from_str(&rendered)?;
        assert_eq!(deterministic_yaml(&reparsed)?, rendered);
        Ok(())
    }

    fn canonical_template_digest<T: Serialize>(value: &T) -> anyhow::Result<String> {
        let canonical = serde_json::to_vec(value)?;
        Ok(format!("sha256:{}", hex::encode(Sha256::digest(canonical))))
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
    fn validation_treats_model_and_effort_as_the_target_identity() -> anyhow::Result<()> {
        let mut distinct = definition();
        distinct.routes.clear();
        distinct.tiers = BTreeMap::from([
            (
                "economy".into(),
                PolicyModelTarget::ModelEffort {
                    model: "vendor:same".into(),
                    effort: ReasoningEffort::Low,
                },
            ),
            (
                "strong".into(),
                PolicyModelTarget::ModelEffort {
                    model: "vendor:same".into(),
                    effort: ReasoningEffort::High,
                },
            ),
        ]);
        let mut lock = PolicyLock::default();
        lock.policies.insert("coding".into(), distinct.clone());
        validate_document(&lock)?;

        let mut mislabeled_v2 = lock.clone();
        mislabeled_v2.lockfile_version = EVIDENCE_POLICY_LOCKFILE_VERSION;
        let version_error = validate_document(&mislabeled_v2)
            .err()
            .ok_or_else(|| anyhow::anyhow!("v2 compound target was accepted"))?;
        assert!(
            version_error
                .to_string()
                .contains("requires policy lock v3")
        );

        distinct.tiers.insert(
            "strong".into(),
            PolicyModelTarget::ModelEffort {
                model: "vendor:same".into(),
                effort: ReasoningEffort::Low,
            },
        );
        lock.policies.insert("coding".into(), distinct);
        let error = validate_document(&lock)
            .err()
            .ok_or_else(|| anyhow::anyhow!("duplicate compound target was accepted"))?;
        assert!(error.to_string().contains("same model/effort target"));
        Ok(())
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
        lock.policies.get_mut("coding").unwrap().default_tier = Some("economy".into());

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
    fn config_provider_stubs_preserve_existing_provider_bodies() -> anyhow::Result<()> {
        let raw = r#"# operator-owned config
providers:
  openai:
    api_key: ${OPENAI_API_KEY}
inherit_defaults: true
"#;
        let edited = edit_config_provider_stubs(
            raw,
            &["openai-codex".into(), "bitrouter".into(), "openai".into()],
        )?;
        let parsed = bitrouter_sdk::config::parse_with(&edited, |_| Some("test".into()))?;

        assert!(edited.contains("# operator-owned config"));
        assert!(edited.contains("    api_key: ${OPENAI_API_KEY}"));
        assert!(parsed.providers.contains_key("openai-codex"));
        assert!(parsed.providers.contains_key("bitrouter"));
        assert!(parsed.providers.contains_key("openai"));
        Ok(())
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
        assert_eq!(policy.tiers["strong"].model(), "anthropic/claude-opus-4.8");
        assert_eq!(policy.tiers["economy"].model(), "moonshotai/kimi-k2.7-code");
        assert_eq!(policy.default_tier.as_deref(), Some("strong"));
        assert_eq!(policy.adequacy.explore_tier.as_deref(), Some("economy"));
    }

    #[tokio::test]
    async fn initialize_marks_only_declared_tool_capable_tiers_safe() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("bitrouter.yaml");
        tokio::fs::write(
            &config_path,
            r#"inherit_defaults: false
providers:
  strong-provider:
    api_base: https://strong.example/v1
    api_key: strong
    models:
      - id: strong-model
        capabilities: [reasoning, tools]
  economy-provider:
    api_base: https://economy.example/v1
    api_key: economy
    models:
      - id: economy-model
        capabilities: [tools]
presets:
  auto:
    model: strong-provider:strong-model
"#,
        )
        .await?;

        let update = initialize_files(
            &config_path,
            "auto",
            "auto",
            None,
            "economy-provider:economy-model",
        )
        .await?;
        let loaded = load(&update.path).await?;

        assert_eq!(
            loaded.document.policies["auto"].tool_safe_tiers,
            ["strong", "economy"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn initialize_accepts_same_model_at_distinct_efforts() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("bitrouter.yaml");
        tokio::fs::write(
            &config_path,
            r#"inherit_defaults: false
providers:
  model-provider:
    api_base: https://model.example/v1
    api_key: test
    models:
      - id: one-model
        capabilities: [reasoning, tools]
        reasoning_effort:
          levels: [low, high]
          default: high
presets:
  auto:
    model: model-provider:one-model
"#,
        )
        .await?;

        let update = initialize_files_with_efforts(
            &config_path,
            "auto",
            "auto",
            Some("model-provider:one-model"),
            Some(ReasoningEffort::High),
            "model-provider:one-model",
            Some(ReasoningEffort::Low),
        )
        .await?;
        let loaded = load(&update.path).await?;
        let policy = &loaded.document.policies["auto"];
        assert_eq!(policy.tiers["strong"].model(), "model-provider:one-model");
        assert_eq!(policy.tiers["strong"].effort(), Some(ReasoningEffort::High));
        assert_eq!(policy.tiers["economy"].model(), "model-provider:one-model");
        assert_eq!(policy.tiers["economy"].effort(), Some(ReasoningEffort::Low));
        Ok(())
    }

    #[tokio::test]
    async fn initialize_uses_embedded_cloud_capabilities_for_tool_safety() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("bitrouter.yaml");
        tokio::fs::write(
            &config_path,
            r#"registry:
  enabled: false
providers:
  openai-codex:
    api_base: https://chatgpt.example/backend-api/codex
    models:
      - id: gpt-5.6-sol
        capabilities: [reasoning, tools]
  bitrouter:
    api_base: https://api.bitrouter.example/v1
presets:
  auto:
    model: openai-codex:gpt-5.6-sol
"#,
        )
        .await?;

        let update = initialize_files(
            &config_path,
            "auto",
            "auto",
            None,
            "bitrouter:deepseek/deepseek-v4-flash-0731",
        )
        .await?;
        let loaded = load(&update.path).await?;

        assert_eq!(
            loaded.document.policies["auto"].tool_safe_tiers,
            ["strong", "economy"]
        );
        Ok(())
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
        let route_key = "agent_route/v1|unknown|unknown|normal";
        let mut lock = task_aware_lock(BTreeMap::from([(
            route_key.to_string(),
            "strong".to_string(),
        )]));
        write_atomic(&lock_path, None, &lock).unwrap();
        let db = crate::db::connect("sqlite::memory:").await.unwrap();
        crate::db::run_migrations(&db).await.unwrap();
        let runtime = PolicyRuntime::new(
            &config,
            Some(&config_path),
            db,
            None,
            PendingEvalDecisionStore::default(),
            None,
        )
        .await
        .unwrap();

        let mut initial = context("vendor:strong");
        runtime
            .select_variant("coding", None, &mut initial)
            .await
            .unwrap();
        assert_eq!(initial.model(), "vendor:strong");

        lock.policies
            .get_mut("coding")
            .unwrap()
            .routes
            .insert(route_key.into(), "economy".into());
        lock.certificates
            .get_mut("coding")
            .and_then(|certificates| certificates.get_mut(route_key))
            .expect("test route certificate")
            .selected_tier = "economy".into();
        write_atomic(&lock_path, None, &lock).unwrap();
        runtime
            .reload_for_config(&config, Some(&config_path))
            .await
            .unwrap();
        let mut reloaded = context("vendor:strong");
        runtime
            .select_variant("coding", None, &mut reloaded)
            .await
            .unwrap();
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
        runtime
            .select_variant("coding", None, &mut last_known_good)
            .await
            .unwrap();
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
        let request_key = "agent_route/v1|unknown|implement|normal";
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
