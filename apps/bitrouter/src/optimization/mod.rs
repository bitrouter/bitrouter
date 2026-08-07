use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

pub mod evaluator;
pub mod orchestrator;
pub mod runner;
pub mod setup;

pub const OPTIMIZATION_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_INTENT_FILENAME: &str = "bitrouter.optimize.yaml";
pub const DEFAULT_LOCK_FILENAME: &str = "bitrouter.optimize.lock.yaml";
pub const DEFAULT_CONTRACT_FILENAME: &str = "bitrouter.eval.md";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptimizationPreference {
    QualityFirst,
    Balanced,
    SavingsFirst,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluatorRoute {
    Cloud,
    Direct,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCommand {
    pub command: Vec<String>,
    pub timeout_secs: u64,
}

impl WorkflowCommand {
    pub fn validate(&self) -> Result<()> {
        if self.command.is_empty()
            || self
                .command
                .iter()
                .any(|part| part.trim().is_empty() || part.chars().any(char::is_control))
        {
            anyhow::bail!("workflow command must contain non-empty bounded arguments");
        }
        if self.command.len() > 256 || self.command.iter().any(|part| part.len() > 4096) {
            anyhow::bail!("workflow command exceeds the bounded argv contract");
        }
        if self.timeout_secs == 0 {
            anyhow::bail!("workflow timeout must be positive");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomQualityGate {
    pub minimum_pass_rate_ppm: u32,
    pub maximum_quality_loss_ppm: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedEvaluator {
    pub agent: String,
    pub model: String,
    pub route: EvaluatorRoute,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptimizationIntent {
    pub version: u32,
    pub workflow: WorkflowCommand,
    pub contract: PathBuf,
    pub source_config: PathBuf,
    pub policy: String,
    pub preset: String,
    pub strong: String,
    pub economy: String,
    pub preference: OptimizationPreference,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_quality: Option<CustomQualityGate>,
    pub evaluator: ResolvedEvaluator,
}

impl OptimizationIntent {
    pub fn validate(&self) -> Result<()> {
        if self.version != OPTIMIZATION_SCHEMA_VERSION {
            anyhow::bail!("unsupported optimization intent version {}", self.version);
        }
        self.workflow.validate()?;
        if self.contract.as_os_str().is_empty() || self.source_config.as_os_str().is_empty() {
            anyhow::bail!("contract and source_config paths must be non-empty");
        }
        for (label, value) in [
            ("policy", self.policy.as_str()),
            ("preset", self.preset.as_str()),
            ("strong", self.strong.as_str()),
            ("economy", self.economy.as_str()),
            ("evaluator agent", self.evaluator.agent.as_str()),
            ("evaluator model", self.evaluator.model.as_str()),
        ] {
            if value.trim().is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
                anyhow::bail!("{label} must be a non-empty bounded identifier");
            }
        }
        if self.strong == self.economy {
            anyhow::bail!("strong and economy routes must be distinct");
        }
        match (self.preference, self.custom_quality.as_ref()) {
            (OptimizationPreference::Custom, None) => {
                anyhow::bail!("custom preference requires custom_quality gates")
            }
            (OptimizationPreference::Custom, Some(gate)) => gate.validate()?,
            (_, Some(_)) => {
                anyhow::bail!("custom_quality is valid only with the custom preference")
            }
            (_, None) => {}
        }
        Ok(())
    }

    pub fn semantic_digest(&self) -> Result<String> {
        self.validate()?;
        let canonical = serde_json::to_vec(self).context("serializing optimization intent")?;
        Ok(format!("sha256:{}", hex::encode(Sha256::digest(canonical))))
    }

    pub fn promotion_quality_criteria(
        &self,
    ) -> Result<crate::policy_compile::PromotionQualityCriteria> {
        self.validate()?;
        match self.preference {
            OptimizationPreference::QualityFirst => {
                Ok(crate::policy_compile::PromotionQualityCriteria::quality_first())
            }
            OptimizationPreference::Balanced | OptimizationPreference::SavingsFirst => {
                Ok(crate::policy_compile::PromotionQualityCriteria::manual_review())
            }
            OptimizationPreference::Custom => {
                let gate = self.custom_quality.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("custom preference requires custom_quality gates")
                })?;
                crate::policy_compile::PromotionQualityCriteria::custom(
                    i64::from(gate.minimum_pass_rate_ppm),
                    i64::from(gate.maximum_quality_loss_ppm),
                )
            }
        }
    }
}

impl CustomQualityGate {
    fn validate(&self) -> Result<()> {
        if self.minimum_pass_rate_ppm > 1_000_000 || self.maximum_quality_loss_ppm > 1_000_000 {
            anyhow::bail!("custom quality gates must be between 0 and 1000000 ppm");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptimizationPaths {
    pub intent: PathBuf,
    pub lock: PathBuf,
    pub contract: PathBuf,
    pub source_config: PathBuf,
    pub private_runs: PathBuf,
}

impl OptimizationPaths {
    pub fn for_intent(intent: PathBuf) -> Self {
        let intent = absolute_path(intent);
        let root = intent.parent().unwrap_or(Path::new(".")).to_path_buf();
        let project_digest = hex::encode(Sha256::digest(root.to_string_lossy().as_bytes()));
        let runtime_home = std::env::var_os("BITROUTER_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|value| !value.is_empty())
                    .map(|home| PathBuf::from(home).join(".bitrouter"))
            })
            .unwrap_or_else(|| std::env::temp_dir().join("bitrouter"));
        Self {
            intent,
            lock: root.join(DEFAULT_LOCK_FILENAME),
            contract: root.join(DEFAULT_CONTRACT_FILENAME),
            source_config: root.join("bitrouter.yaml"),
            private_runs: runtime_home
                .join("optimization")
                .join(&project_digest[..16])
                .join("runs"),
        }
    }

    fn with_intent_paths(mut self, value: &OptimizationIntent) -> Self {
        let root = self.intent.parent().unwrap_or(Path::new("."));
        self.contract = resolve_relative(root, &value.contract);
        self.source_config = resolve_relative(root, &value.source_config);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedIntent {
    pub intent: OptimizationIntent,
    pub digest: String,
    pub paths: OptimizationPaths,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluatorLock {
    pub agent: String,
    pub agent_version: String,
    pub model: String,
    pub route: EvaluatorRoute,
    pub skill_digest: String,
    pub contract_digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptimizationVerdict {
    Pass,
    Fail,
    Inconclusive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutcomeSummary {
    pub verdict: OptimizationVerdict,
    pub evidence_digest: String,
    pub policy_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_cost_micro_usd: Option<u64>,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptimizationRunLock {
    pub run_id: String,
    pub source_policy_digest: String,
    pub target_request_key: String,
    pub baseline: OutcomeSummary,
    pub candidate: OutcomeSummary,
    pub eval_snapshot_digest: String,
    pub candidate_digest: String,
    pub report_digest: String,
    pub publishable: bool,
    pub published: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptimizationLock {
    pub lockfile_version: u32,
    pub intent_digest: String,
    pub active_policy_digest: String,
    pub evaluator: EvaluatorLock,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_run: Option<OptimizationRunLock>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedOptimizationLock {
    pub path: PathBuf,
    pub digest: String,
    pub document: OptimizationLock,
}

impl OptimizationLock {
    pub fn validate(&self) -> Result<()> {
        if self.lockfile_version != OPTIMIZATION_SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported optimization lockfile version {}",
                self.lockfile_version
            );
        }
        validate_sha256("intent_digest", &self.intent_digest)?;
        validate_sha256("active_policy_digest", &self.active_policy_digest)?;
        self.evaluator.validate()?;
        if let Some(run) = &self.latest_run {
            run.validate()?;
        }
        Ok(())
    }

    pub fn semantic_digest(&self) -> Result<String> {
        self.validate()?;
        let canonical = serde_json::to_vec(self).context("serializing optimization lock")?;
        Ok(format!("sha256:{}", hex::encode(Sha256::digest(canonical))))
    }
}

impl EvaluatorLock {
    fn validate(&self) -> Result<()> {
        for (label, value) in [
            ("agent", self.agent.as_str()),
            ("agent_version", self.agent_version.as_str()),
            ("model", self.model.as_str()),
        ] {
            validate_identifier(label, value)?;
        }
        validate_sha256("skill_digest", &self.skill_digest)?;
        validate_sha256("contract_digest", &self.contract_digest)?;
        Ok(())
    }
}

impl OutcomeSummary {
    fn validate(&self, label: &str) -> Result<()> {
        validate_sha256(&format!("{label}.evidence_digest"), &self.evidence_digest)?;
        validate_sha256(&format!("{label}.policy_digest"), &self.policy_digest)?;
        Ok(())
    }
}

impl OptimizationRunLock {
    fn validate(&self) -> Result<()> {
        validate_identifier("latest_run.run_id", &self.run_id)?;
        validate_identifier("latest_run.target_request_key", &self.target_request_key)?;
        validate_sha256(
            "latest_run.source_policy_digest",
            &self.source_policy_digest,
        )?;
        validate_sha256(
            "latest_run.eval_snapshot_digest",
            &self.eval_snapshot_digest,
        )?;
        validate_sha256("latest_run.candidate_digest", &self.candidate_digest)?;
        validate_sha256("latest_run.report_digest", &self.report_digest)?;
        self.baseline.validate("latest_run.baseline")?;
        self.candidate.validate("latest_run.candidate")?;
        if self.published && !self.publishable {
            anyhow::bail!("latest_run cannot be published when it is not publishable");
        }
        Ok(())
    }
}

pub async fn load_intent(path: &Path) -> Result<LoadedIntent> {
    let paths = OptimizationPaths::for_intent(path.to_path_buf());
    let raw = tokio::fs::read_to_string(&paths.intent)
        .await
        .with_context(|| format!("reading {}", paths.intent.display()))?;
    let intent: OptimizationIntent = serde_saphyr::from_str(&raw)
        .with_context(|| format!("parsing {}", paths.intent.display()))?;
    intent.validate()?;
    let digest = intent.semantic_digest()?;
    let paths = paths.with_intent_paths(&intent);
    Ok(LoadedIntent {
        intent,
        digest,
        paths,
    })
}

pub async fn write_intent_create_new(path: &Path, intent: &OptimizationIntent) -> Result<()> {
    intent.validate()?;
    let mut rendered =
        serde_saphyr::to_string(intent).context("serializing optimization intent")?;
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .await
        .with_context(|| {
            if path.exists() {
                format!("{} already exists; refusing to overwrite", path.display())
            } else {
                format!("creating {}", path.display())
            }
        })?;
    file.write_all(rendered.as_bytes())
        .await
        .with_context(|| format!("writing {}", path.display()))?;
    file.flush()
        .await
        .with_context(|| format!("flushing {}", path.display()))?;
    Ok(())
}

pub async fn load_lock(path: &Path) -> Result<LoadedOptimizationLock> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading optimization lock {}", path.display()))?;
    let document: OptimizationLock = serde_saphyr::from_str(&raw)
        .with_context(|| format!("parsing optimization lock {}", path.display()))?;
    document.validate()?;
    let digest = document.semantic_digest()?;
    Ok(LoadedOptimizationLock {
        path: path.to_path_buf(),
        digest,
        document,
    })
}

pub async fn write_lock_compare_and_swap(
    path: &Path,
    expected_digest: Option<&str>,
    document: &OptimizationLock,
) -> Result<()> {
    document.validate()?;
    let mut rendered =
        serde_saphyr::to_string(document).context("serializing optimization lock")?;
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    let Some(expected_digest) = expected_digest else {
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .await
            .with_context(|| {
                if path.exists() {
                    format!("{} already exists; refusing to overwrite", path.display())
                } else {
                    format!("creating {}", path.display())
                }
            })?;
        file.write_all(rendered.as_bytes())
            .await
            .with_context(|| format!("writing {}", path.display()))?;
        file.flush()
            .await
            .with_context(|| format!("flushing {}", path.display()))?;
        return Ok(());
    };

    validate_sha256("expected optimization lock digest", expected_digest)?;
    let current_raw = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading optimization lock {}", path.display()))?;
    let current: OptimizationLock = serde_saphyr::from_str(&current_raw)
        .with_context(|| format!("parsing optimization lock {}", path.display()))?;
    current.validate()?;
    if current.semantic_digest()? != expected_digest {
        anyhow::bail!(
            "optimization lock changed concurrently; refusing to overwrite {}",
            path.display()
        );
    }
    crate::policy_lock::write_text_atomic(path, &current_raw, &rendered).map_err(|error| {
        if error.to_string().contains("changed since it was loaded") {
            anyhow::anyhow!(
                "optimization lock changed concurrently; refusing to overwrite {}",
                path.display()
            )
        } else {
            error.context(format!("publishing optimization lock {}", path.display()))
        }
    })
}

fn validate_identifier(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        anyhow::bail!("{label} must be a non-empty bounded identifier");
    }
    Ok(())
}

fn validate_sha256(label: &str, value: &str) -> Result<()> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        anyhow::bail!("{label} must be a sha256 digest");
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        anyhow::bail!("{label} must be a lowercase sha256 digest");
    }
    Ok(())
}

fn resolve_relative(root: &Path, value: &Path) -> PathBuf {
    if value.is_absolute() {
        value.to_path_buf()
    } else {
        root.join(value)
    }
}

fn absolute_path(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        EvaluatorLock, EvaluatorRoute, OptimizationIntent, OptimizationLock, OptimizationPaths,
        OptimizationPreference, ResolvedEvaluator, WorkflowCommand, load_intent, load_lock,
        write_intent_create_new, write_lock_compare_and_swap,
    };

    fn intent() -> OptimizationIntent {
        OptimizationIntent {
            version: 1,
            workflow: WorkflowCommand {
                command: vec!["python3".into(), "eval.py".into()],
                timeout_secs: 900,
            },
            contract: PathBuf::from("bitrouter.eval.md"),
            source_config: PathBuf::from("bitrouter.yaml"),
            policy: "auto".into(),
            preset: "auto".into(),
            strong: "bitrouter:openai/gpt-5.6".into(),
            economy: "bitrouter:deepseek/deepseek-v4-flash-0731".into(),
            preference: OptimizationPreference::Balanced,
            custom_quality: None,
            evaluator: ResolvedEvaluator {
                agent: "codex-acp".into(),
                model: "bitrouter:openai/gpt-5.6".into(),
                route: EvaluatorRoute::Cloud,
            },
        }
    }

    #[tokio::test]
    async fn intent_round_trips_with_stable_digest_and_resolved_paths() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("bitrouter.optimize.yaml");
        write_intent_create_new(&path, &intent()).await?;

        let loaded = load_intent(&path).await?;

        assert_eq!(loaded.intent, intent());
        assert_eq!(
            loaded.paths.source_config,
            dir.path().join("bitrouter.yaml")
        );
        assert_eq!(loaded.paths.contract, dir.path().join("bitrouter.eval.md"));
        assert_eq!(
            loaded.paths.lock,
            dir.path().join("bitrouter.optimize.lock.yaml")
        );
        assert_eq!(loaded.digest, loaded.intent.semantic_digest()?);
        assert!(loaded.digest.starts_with("sha256:"));
        Ok(())
    }

    #[tokio::test]
    async fn setup_never_overwrites_an_existing_intent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("bitrouter.optimize.yaml");
        tokio::fs::write(&path, "owned: true\n").await?;

        let error = write_intent_create_new(&path, &intent()).await.unwrap_err();

        assert!(error.to_string().contains("already exists"));
        assert_eq!(tokio::fs::read_to_string(path).await?, "owned: true\n");
        Ok(())
    }

    #[test]
    fn intent_rejects_ambiguous_or_unsafe_optimization_inputs() {
        let mut value = intent();
        value.workflow.command.clear();
        assert!(
            value
                .validate()
                .unwrap_err()
                .to_string()
                .contains("workflow")
        );

        let mut value = intent();
        value.economy = value.strong.clone();
        assert!(
            value
                .validate()
                .unwrap_err()
                .to_string()
                .contains("distinct")
        );

        let mut value = intent();
        value.preference = OptimizationPreference::Custom;
        assert!(
            value
                .validate()
                .unwrap_err()
                .to_string()
                .contains("custom_quality")
        );

        let mut value = intent();
        value.workflow.timeout_secs = 0;
        assert!(
            value
                .validate()
                .unwrap_err()
                .to_string()
                .contains("timeout")
        );
    }

    #[test]
    fn intent_resolves_qualitative_profiles_without_latency_gates() -> anyhow::Result<()> {
        let mut value = intent();
        value.preference = OptimizationPreference::QualityFirst;
        assert_eq!(
            value.promotion_quality_criteria()?,
            crate::policy_compile::PromotionQualityCriteria::quality_first()
        );

        value.preference = OptimizationPreference::Balanced;
        assert_eq!(
            value.promotion_quality_criteria()?,
            crate::policy_compile::PromotionQualityCriteria::manual_review()
        );
        value.preference = OptimizationPreference::SavingsFirst;
        assert_eq!(
            value.promotion_quality_criteria()?,
            crate::policy_compile::PromotionQualityCriteria::manual_review()
        );

        value.preference = OptimizationPreference::Custom;
        value.custom_quality = Some(super::CustomQualityGate {
            minimum_pass_rate_ppm: 700_000,
            maximum_quality_loss_ppm: 200_000,
        });
        assert_eq!(
            value.promotion_quality_criteria()?,
            crate::policy_compile::PromotionQualityCriteria::custom(700_000, 200_000)?
        );
        Ok(())
    }

    #[test]
    fn default_paths_are_version_controlled_and_private_runs_are_external() {
        let paths = OptimizationPaths::for_intent(PathBuf::from("/repo/bitrouter.optimize.yaml"));

        assert_eq!(
            paths.lock,
            PathBuf::from("/repo/bitrouter.optimize.lock.yaml")
        );
        assert_eq!(paths.contract, PathBuf::from("/repo/bitrouter.eval.md"));
        assert!(!paths.private_runs.starts_with("/repo"));
    }

    fn lock(intent_digest: &str) -> OptimizationLock {
        OptimizationLock {
            lockfile_version: 1,
            intent_digest: intent_digest.into(),
            active_policy_digest:
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            evaluator: EvaluatorLock {
                agent: "codex-acp".into(),
                agent_version: "codex 1.2.3".into(),
                model: "bitrouter:openai/gpt-5.6".into(),
                route: EvaluatorRoute::Cloud,
                skill_digest:
                    "sha256:1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                contract_digest:
                    "sha256:2123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            },
            latest_run: None,
        }
    }

    #[tokio::test]
    async fn optimization_lock_is_deterministic_and_compare_and_swap_protected()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("bitrouter.optimize.lock.yaml");
        let first = lock("sha256:3123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
        write_lock_compare_and_swap(&path, None, &first).await?;
        let loaded = load_lock(&path).await?;
        assert_eq!(loaded.document, first);
        assert_eq!(loaded.digest, first.semantic_digest()?);

        let mut second = first.clone();
        second.active_policy_digest =
            "sha256:4123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into();
        let stale = write_lock_compare_and_swap(
            &path,
            Some("sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"),
            &second,
        )
        .await
        .unwrap_err();
        assert!(stale.to_string().contains("changed concurrently"));
        assert_eq!(load_lock(&path).await?.document, first);

        write_lock_compare_and_swap(&path, Some(&loaded.digest), &second).await?;
        assert_eq!(load_lock(&path).await?.document, second);
        Ok(())
    }

    #[test]
    fn optimization_lock_rejects_unpinned_or_malformed_evaluator_identity() {
        let mut value =
            lock("sha256:3123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
        value.evaluator.agent_version.clear();
        assert!(
            value
                .validate()
                .unwrap_err()
                .to_string()
                .contains("agent_version")
        );

        let value = lock("not-a-digest");
        assert!(
            value
                .validate()
                .unwrap_err()
                .to_string()
                .contains("intent_digest")
        );
    }
}
