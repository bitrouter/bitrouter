//! Current model-only policy source, atomic publication and serving snapshots.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::policy::{Document, Policy, SpecVersion, Target};
use serde::{Deserialize, Serialize};

use crate::actions::administration::PolicyReportRuntime;
use crate::config::Config;
use crate::continuation::ContinuationRequestPlan;
use crate::gateway::observability::decision::{EvalInvocation, PendingEvalDecisionStore};
use crate::gateway::pipeline::context::PipelineContext;
use crate::gateway::pipeline::hooks::RouteHook;
use crate::gateway::pipeline::routing::ModelSelector;
use crate::gateway::pipeline::types::RoutingTarget;
use crate::gateway::routing::workflow::decision::PolicyDecisionJsonlRecorder;
#[cfg(feature = "multi-instance")]
use crate::model_policy::CompiledPolicy;
use crate::policy_table_router::{PolicyTable, PolicyTableRouter};

pub const DEFAULT_POLICY_FILENAME: &str = "policy.yaml";
const BUNDLED_POLICY: &str = include_str!("../policy/default.yaml");

/// Product defaults stay in the host; SDK embedders keep explicit routing.
pub fn apply_defaults(config: &mut Config, config_path: Option<&Path>) {
    if !config.inherit_defaults || config.tenancy.enabled {
        return;
    }
    if !config.routers.contains_key("auto")
        && config.policy.path.is_none()
        && !resolve_path(config, config_path).is_some_and(|path| path.exists())
        && bound_policy_names(config).is_empty()
    {
        config.routers.insert(
            "auto".into(),
            crate::config::router::RouterConfig {
                selection: crate::config::router::RouterSelection::Policy {
                    policy: "auto".into(),
                    base_model: "openai-codex:gpt-5.6-sol".into(),
                    routing: Default::default(),
                },
                defaults: Default::default(),
                checks: Default::default(),
            },
        );
    }
}

/// Parsed source plus its runtime-computed identity.
#[derive(Debug, Clone)]
pub struct LoadedPolicy {
    /// None identifies the immutable policy bundled with the binary.
    pub path: Option<PathBuf>,
    pub digest: String,
    pub document: Document,
}

impl LoadedPolicy {
    pub fn file_path(&self) -> Result<&Path> {
        self.path
            .as_deref()
            .context("the bundled policy is read-only; initialize a policy.yaml before publication")
    }
}

/// Resolve only current policy.yaml discovery or an explicitly configured path.
pub fn resolve_path(config: &Config, config_path: Option<&Path>) -> Option<PathBuf> {
    match config.policy.path.as_deref() {
        Some(path) if path.is_absolute() => Some(path.to_path_buf()),
        configured => config_path.map(|config_path| {
            let parent = config_path
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .map_or(Path::new("."), |path| path);
            parent.join(configured.map_or_else(|| Path::new(DEFAULT_POLICY_FILENAME), |path| path))
        }),
    }
}

pub fn bound_policy_names(config: &Config) -> BTreeSet<String> {
    config
        .router_policy_bindings()
        .map(|(_, policy, _)| policy.to_owned())
        .collect()
}

/// Load an explicit current source first. Missing explicit files never fall back.
pub async fn load_for_config(
    config: &Config,
    config_path: Option<&Path>,
) -> Result<Option<LoadedPolicy>> {
    if let Some(path) = resolve_path(config, config_path) {
        match tokio::fs::metadata(&path).await {
            Ok(_) => {
                let loaded = load(&path).await?;
                validate_for_config(config, &loaded.document)?;
                return Ok(Some(loaded));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading policy source {}", path.display()));
            }
        }
    }
    ensure!(
        config.policy.path.is_none(),
        "explicit policy source is unavailable; check policy.path and its configuration directory"
    );
    if config.inherit_defaults && !config.tenancy.enabled {
        let document = crate::model_policy::decode(BUNDLED_POLICY)?;
        validate_for_config(config, &document)?;
        return Ok(Some(LoadedPolicy {
            path: None,
            digest: semantic_digest(&document)?,
            document,
        }));
    }
    ensure!(
        bound_policy_names(config).is_empty(),
        "policy bindings require an available policy.yaml"
    );
    Ok(None)
}

pub async fn load(path: &Path) -> Result<LoadedPolicy> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading policy source {}", path.display()))?;
    let document = crate::model_policy::decode(&raw)
        .with_context(|| format!("parsing policy source {}", path.display()))?;
    Ok(LoadedPolicy {
        path: Some(path.to_path_buf()),
        digest: semantic_digest(&document)?,
        document,
    })
}

pub fn validate_document(document: &Document) -> Result<()> {
    crate::model_policy::compile(document)?;
    Ok(())
}

pub fn validate_for_config(config: &Config, document: &Document) -> Result<()> {
    validate_document(document)?;
    for (router, policy, base_model) in config.router_policy_bindings() {
        ensure!(
            base_model.is_some_and(|model| !model.trim().is_empty()),
            "router '{router}' must define a base model before binding policy '{policy}'"
        );
        ensure!(
            document.policies.contains_key(policy),
            "router '{router}' references missing policy '{policy}'"
        );
    }
    Ok(())
}

pub fn validate_name(name: &str) -> Result<()> {
    crate::model_policy::validate_name(name)
}

/// Canonical source identity; never an authorization or resource revision.
pub fn semantic_digest(document: &Document) -> Result<String> {
    crate::model_policy::source_digest(document)
}

pub fn deterministic_yaml(document: &Document) -> Result<String> {
    validate_document(document)?;
    let mut rendered = serde_saphyr::to_string(document).context("serializing policy source")?;
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    Ok(rendered)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PolicyRouteDiff {
    pub policy: String,
    pub request_key: String,
    pub active_tier: Option<String>,
    pub candidate_tier: Option<String>,
}

pub fn diff_documents(active: &Document, candidate: &Document) -> Vec<PolicyRouteDiff> {
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
                    request_key: request_key.to_string(),
                    active_tier,
                    candidate_tier,
                });
            }
        }
    }
    differences
}

pub fn diff_explanations(active: &Document, candidate: &Document) -> Vec<String> {
    let mut changes = diff_documents(active, candidate)
        .into_iter()
        .map(|difference| {
            format!(
                "{}: route {} {:?} -> {:?}",
                difference.policy,
                difference.request_key,
                difference.active_tier,
                difference.candidate_tier
            )
        })
        .collect::<Vec<_>>();
    let names = active
        .policies
        .keys()
        .chain(candidate.policies.keys())
        .collect::<BTreeSet<_>>();
    for name in names {
        match (active.policies.get(name), candidate.policies.get(name)) {
            (Some(before), Some(after)) => {
                let tiers = before
                    .tiers
                    .keys()
                    .chain(after.tiers.keys())
                    .collect::<BTreeSet<_>>();
                for tier in tiers {
                    if before.tiers.get(tier) != after.tiers.get(tier) {
                        changes.push(format!(
                            "{name}: tier {tier} {:?} -> {:?}",
                            before.tiers.get(tier),
                            after.tiers.get(tier)
                        ));
                    }
                }
                if before.default_tier != after.default_tier {
                    changes.push(format!(
                        "{name}: default_tier {} -> {}",
                        before.default_tier, after.default_tier
                    ));
                }
                if before.tool_use_tier != after.tool_use_tier {
                    changes.push(format!(
                        "{name}: tool_use_tier {:?} -> {:?}",
                        before.tool_use_tier, after.tool_use_tier
                    ));
                }
                if before.tool_safe_tiers != after.tool_safe_tiers {
                    changes.push(format!(
                        "{name}: tool_safe_tiers {:?} -> {:?}",
                        before.tool_safe_tiers, after.tool_safe_tiers
                    ));
                }
                if before.classifier_digest != after.classifier_digest {
                    changes.push(format!(
                        "{name}: classifier_digest {:?} -> {:?}",
                        before.classifier_digest, after.classifier_digest
                    ));
                }
            }
            (None, Some(_)) => changes.push(format!("{name}: added policy")),
            (Some(_), None) => changes.push(format!("{name}: removed policy")),
            (None, None) => {}
        }
    }
    changes
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
    candidate: &Document,
    history_dir: &Path,
) -> Result<PromotionRecord> {
    let _publication_lock = acquire_publication_lock(active_path)?;
    publish_candidate_unlocked(active_path, expected_digest, candidate, history_dir)
}

/// Publish while the caller holds [`acquire_publication_lock`] for `active_path`.
pub fn publish_candidate_unlocked(
    active_path: &Path,
    expected_digest: &str,
    candidate: &Document,
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
    validate_sha256_digest(target_digest)?;
    let target_path = history_snapshot_path(history_dir, target_digest)?;
    let target_bytes = std::fs::read(&target_path)
        .with_context(|| format!("reading policy history {}", target_path.display()))?;
    let target_raw = std::str::from_utf8(&target_bytes).context("policy history is not UTF-8")?;
    let target = crate::model_policy::decode(target_raw)
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
    target: &Document,
    history_dir: &Path,
    action: &str,
) -> Result<PromotionRecord> {
    let parent_bytes = std::fs::read(active_path)
        .with_context(|| format!("reading active policy source {}", active_path.display()))?;
    let parent_raw =
        std::str::from_utf8(&parent_bytes).context("active policy source is not UTF-8")?;
    let parent = crate::model_policy::decode(parent_raw)
        .with_context(|| format!("parsing active policy source {}", active_path.display()))?;
    validate_document(&parent)?;
    let parent_digest = semantic_digest(&parent)?;
    if parent_digest != expected_digest {
        anyhow::bail!(
            "policy source changed since it was loaded (expected {expected_digest}, found {parent_digest}); refusing to overwrite"
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
    if let Err(error) = append_promotion_record(history_dir, &record) {
        return match write_bytes_atomic_unlocked(active_path, &parent_bytes) {
            Ok(()) => Err(error.context("policy publication log failed; restored active source")),
            Err(recovery) => Err(error.context(format!(
                "policy publication log failed and active source recovery failed: {recovery:#}"
            ))),
        };
    }
    Ok(record)
}

/// Load and verify one immutable policy history snapshot.
pub fn load_history_snapshot(history_dir: &Path, digest: &str) -> Result<Document> {
    validate_sha256_digest(digest)?;
    let path = history_snapshot_path(history_dir, digest)?;
    let bytes = std::fs::read(&path)
        .with_context(|| format!("reading policy history {}", path.display()))?;
    let raw = std::str::from_utf8(&bytes).context("policy history is not UTF-8")?;
    let target = crate::model_policy::decode(raw)
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
    validate_sha256_digest(digest)?;
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
    write_new_file_atomic(&path, bytes)
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

fn validate_sha256_digest(value: &str) -> Result<()> {
    let hex = value
        .strip_prefix("sha256:")
        .context("policy digest must use sha256")?;
    ensure!(
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "invalid policy digest"
    );
    Ok(())
}

pub fn edit_config_router_policy_with_model(
    raw: &str,
    router: &str,
    policy: &str,
    model: Option<&str>,
) -> Result<String> {
    validate_router_name(router)?;
    validate_name(policy)?;
    let parsed = crate::config::parse(raw).context("parsing bitrouter.yaml")?;
    if let Some(existing) = parsed.routers.get(router) {
        match &existing.selection {
            crate::config::router::RouterSelection::Policy {
                policy: existing_policy,
                base_model,
                ..
            } => {
                if existing_policy != policy {
                    anyhow::bail!(
                        "router '{router}' already binds policy '{existing_policy}', not '{policy}'"
                    );
                }
                if let Some(model) = model
                    && base_model != model
                {
                    anyhow::bail!(
                        "router '{router}' already uses base model '{base_model}', not '{model}'"
                    );
                }
                return Ok(raw.to_string());
            }
            crate::config::router::RouterSelection::Model {
                model: existing_model,
                ..
            } => {
                anyhow::bail!(
                    "router '{router}' already uses fixed model '{existing_model}'; refusing to replace it with a policy binding"
                );
            }
        }
    }
    let Some(model) = model else {
        anyhow::bail!("router '{router}' does not exist; pass --strong <model> to initialize it");
    };

    let mut lines = source_lines(raw);
    bind_router(&mut lines, router, policy, model)?;
    let edited = render_source_lines(lines, raw.ends_with('\n'));
    let checked = crate::config::parse(&edited).context("validating edited bitrouter.yaml")?;
    let Some(created) = checked.routers.get(router) else {
        anyhow::bail!("edited config did not create router '{router}'");
    };
    match &created.selection {
        crate::config::router::RouterSelection::Policy {
            policy: checked_policy,
            base_model,
            ..
        } if checked_policy == policy && base_model == model => Ok(edited),
        _ => anyhow::bail!(
            "edited config did not bind router '{router}' to policy '{policy}' with base model '{model}'"
        ),
    }
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

fn bind_router(lines: &mut Vec<String>, router: &str, policy: &str, model: &str) -> Result<()> {
    let policy = serde_json::to_string(policy).context("encoding router policy as YAML scalar")?;
    let model = serde_json::to_string(model).context("encoding router model as YAML scalar")?;
    if let Some((start, end)) = block_range(lines, "routers", 0) {
        let inline_empty = lines[start]
            .split_once(':')
            .is_some_and(|(_, value)| matches!(value.trim(), "{}" | "{ }"));
        if inline_empty {
            lines[start] = "routers:".into();
        } else {
            require_block_header(&lines[start], "routers")?;
        }
        let insert_at = if inline_empty { start + 1 } else { end };
        lines.insert(insert_at, format!("  {router}:"));
        lines.insert(insert_at + 1, "    selection:".into());
        lines.insert(insert_at + 2, "      kind: policy".into());
        lines.insert(insert_at + 3, format!("      policy: {policy}"));
        lines.insert(insert_at + 4, format!("      base_model: {model}"));
        return Ok(());
    }

    if !lines.is_empty() && !lines.last().is_some_and(|line| line.is_empty()) {
        lines.push(String::new());
    }
    lines.push("routers:".into());
    lines.push(format!("  {router}:"));
    lines.push("    selection:".into());
    lines.push("      kind: policy".into());
    lines.push(format!("      policy: {policy}"));
    lines.push(format!("      base_model: {model}"));
    Ok(())
}

fn validate_router_name(name: &str) -> Result<()> {
    let mut bytes = name.bytes();
    let valid = name.len() <= 64
        && bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        });
    if !valid {
        anyhow::bail!(
            "invalid router id '{name}' (use 1-64 lowercase ASCII letters, digits, '_' or '-', starting with a letter)"
        );
    }
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

pub fn default_history_dir(active_path: &Path) -> PathBuf {
    active_path
        .parent()
        .map(|parent| parent.join(".bitrouter-policy-history"))
        .unwrap_or_else(|| PathBuf::from(".bitrouter-policy-history"))
}

pub async fn validate_routable_model(source: &crate::config::Config, route: &str) -> Result<()> {
    let (provider, model) = route
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("policy tier route '{route}' must be provider-qualified"))?;
    if provider.is_empty()
        || model.is_empty()
        || model.starts_with('@')
        || model.starts_with("bitrouter/")
    {
        anyhow::bail!("policy tier route '{route}' is not a concrete provider model");
    }
    let mut resolved = source.clone();
    crate::assemble::merge_registry_into(&mut resolved).await;
    crate::providers::apply::apply_builtin_defaults(&mut resolved);
    let provider_config = resolved.providers.get(provider).ok_or_else(|| {
        anyhow::anyhow!(
            "policy provider '{provider}' is not available from the config or provider registry"
        )
    })?;
    if !provider_config.active || provider_config.api_base.trim().is_empty() {
        anyhow::bail!(
            "policy provider '{provider}' is not active and routable; configure its credential or provider entry first"
        );
    }
    crate::config::routing_table::resolve_route_chain(
        &resolved,
        route,
        &crate::gateway::pipeline::routing::RoutingPrefs::default(),
    )
    .with_context(|| format!("resolving policy tier route '{route}'"))?;
    Ok(())
}

pub async fn validate_routable_effort(
    source: &crate::config::Config,
    route: &str,
    effort: Option<bitrouter_ai::types::ReasoningEffort>,
) -> Result<()> {
    let Some(effort) = effort else {
        return Ok(());
    };
    let mut resolved = source.clone();
    crate::assemble::merge_registry_into(&mut resolved).await;
    crate::providers::apply::apply_builtin_defaults(&mut resolved);
    let chain = crate::config::routing_table::resolve_route_chain(
        &resolved,
        route,
        &crate::gateway::pipeline::routing::RoutingPrefs::default(),
    )
    .with_context(|| format!("resolving policy tier route '{route}'"))?;
    if !chain.iter().any(|target| {
        target
            .reasoning_effort
            .as_ref()
            .is_some_and(|support| support.levels.contains(&effort))
    }) {
        anyhow::bail!(
            "policy tier route '{route}' has no target with positive support for effort '{effort}'"
        );
    }
    Ok(())
}

fn validate_tier_model(model: &str, tier: &str) -> Result<()> {
    if model.trim().is_empty()
        || model.starts_with('@')
        || model.starts_with("bitrouter/")
        || crate::config::resolution::is_reserved(model)
    {
        anyhow::bail!("{tier} model must be a non-empty model id, not a preset");
    }
    Ok(())
}

/// Replace text while the caller holds the target publication lock.
pub fn write_text_atomic_unlocked(path: &Path, expected: &str, updated: &str) -> Result<()> {
    write_text_atomic_unlocked_with_parent_sync(path, expected, updated, sync_parent)
}

pub(crate) fn write_text_atomic_unlocked_with_parent_sync<F>(
    path: &Path,
    expected: &str,
    updated: &str,
    parent_sync: F,
) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
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
        parent_sync(path)?;
        Ok(())
    })();
    if result.is_err()
        && let Err(error) = std::fs::remove_file(&tmp)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, path = %tmp.display(), "failed to remove publication temporary file");
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
            .with_context(|| format!("publishing policy source {}", path.display()))?;
        sync_parent(path)
    })();
    if result.is_err()
        && let Err(error) = std::fs::remove_file(&tmp)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, path = %tmp.display(), "failed to remove publication temporary file");
    }
    result
}

fn sibling_temp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("policy.yaml");
    path.with_file_name(format!(".{file_name}.tmp-{}", uuid::Uuid::new_v4()))
}

/// Durably create a file without ever exposing partial contents or replacing
/// an existing owner-controlled file.
pub fn write_new_file_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or(Path::new("."), |parent| parent);
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
        .unwrap_or("policy.yaml");
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

fn canonical_lock_target(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return std::fs::canonicalize(path)
            .with_context(|| format!("canonicalizing lock target {}", path.display()));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or(Path::new("."), |parent| parent);
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
    {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map_or(Path::new("."), |parent| parent);
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

/// Test setup for publication races; serving uses the locked transaction paths.
#[cfg(test)]
pub(crate) fn write_atomic(
    path: &Path,
    expected_digest: Option<&str>,
    document: &Document,
) -> Result<String> {
    let _publication_lock = acquire_publication_lock(path)?;
    write_atomic_unlocked(path, expected_digest, document)
}

pub fn write_atomic_unlocked(
    path: &Path,
    expected_digest: Option<&str>,
    document: &Document,
) -> Result<String> {
    let rendered = deterministic_yaml(document)?;
    match expected_digest {
        Some(expected) => {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("reading current policy source {}", path.display()))?;
            let parsed = crate::model_policy::decode(&raw)?;
            let actual = semantic_digest(&parsed)?;
            ensure!(
                actual == expected,
                "policy source changed since it was loaded (expected {expected}, found {actual}); refusing to overwrite"
            );
            write_bytes_atomic_unlocked(path, rendered.as_bytes())?;
        }
        None => write_new_file_atomic(path, rendered.as_bytes())?,
    }
    semantic_digest(document)
}

#[derive(Debug, Clone)]
pub struct PolicyFileUpdate {
    pub path: PathBuf,
    pub digest: String,
    /// Management metadata for transactional reload recovery, never source data.
    pub parent_digest: Option<String>,
    pub changes: Vec<String>,
}

/// Initialize current source and an explicit named router as one recoverable edit.
pub async fn initialize_router_files_with_efforts(
    config_path: &Path,
    policy_name: &str,
    router_name: &str,
    strong_model: Option<&str>,
    strong_effort: Option<bitrouter_ai::types::ReasoningEffort>,
    economy_model: &str,
    economy_effort: Option<bitrouter_ai::types::ReasoningEffort>,
) -> Result<PolicyFileUpdate> {
    initialize_with_writer(
        config_path,
        PolicyInitRequest {
            policy_name,
            router_name,
            strong_model,
            strong_effort,
            economy_model,
            economy_effort,
        },
        write_text_atomic_unlocked,
    )
    .await
}

struct PolicyInitRequest<'a> {
    policy_name: &'a str,
    router_name: &'a str,
    strong_model: Option<&'a str>,
    strong_effort: Option<bitrouter_ai::types::ReasoningEffort>,
    economy_model: &'a str,
    economy_effort: Option<bitrouter_ai::types::ReasoningEffort>,
}

async fn initialize_with_writer<F>(
    config_path: &Path,
    request: PolicyInitRequest<'_>,
    write_config: F,
) -> Result<PolicyFileUpdate>
where
    F: FnOnce(&Path, &str, &str) -> Result<()>,
{
    let PolicyInitRequest {
        policy_name,
        router_name,
        strong_model,
        strong_effort,
        economy_model,
        economy_effort,
    } = request;
    let _config_lock = acquire_publication_lock(config_path)?;
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .context("reading configuration for policy initialization")?;
    let config = crate::config::parse(&raw)?;
    let policy_path =
        resolve_path(&config, Some(config_path)).context("cannot resolve policy source path")?;
    ensure!(
        resolved_file_location(&policy_path)? != resolved_file_location(config_path)?,
        "policy.path cannot point to the configuration itself"
    );
    let _policy_lock = acquire_publication_lock(&policy_path)?;
    validate_name(policy_name)?;
    validate_router_name(router_name)?;
    let strong_model = strong_model
        .map(ToOwned::to_owned)
        .or_else(|| {
            config
                .routers
                .get(router_name)
                .and_then(|router| match &router.selection {
                    crate::config::router::RouterSelection::Policy { base_model, .. } => {
                        Some(base_model.clone())
                    }
                    crate::config::router::RouterSelection::Model { .. } => None,
                })
        })
        .context("pass --strong to initialize a router without a policy base model")?;
    validate_tier_model(&strong_model, "strong")?;
    validate_tier_model(economy_model, "economy")?;
    ensure!(
        strong_model != economy_model || strong_effort != economy_effort,
        "strong and economy tiers must use different model/effort targets"
    );
    let mut capabilities = config.clone();
    crate::assemble::merge_registry_into(&mut capabilities).await;
    crate::providers::apply::apply_builtin_defaults(&mut capabilities);
    let mut tool_safe_tiers = vec!["strong".to_string()];
    if route_supports_capability(
        &capabilities,
        economy_model,
        bitrouter_ai::types::Capability::Tools,
    ) {
        tool_safe_tiers.push("economy".into());
    }
    let initialized = Policy {
        tiers: BTreeMap::from([
            (
                "strong".into(),
                Target {
                    model: strong_model.clone(),
                    effort: strong_effort,
                },
            ),
            (
                "economy".into(),
                Target {
                    model: economy_model.to_owned(),
                    effort: economy_effort,
                },
            ),
        ]),
        default_tier: "strong".into(),
        classifier_digest: None,
        routes: BTreeMap::new(),
        tool_use_tier: Some("strong".into()),
        tool_safe_tiers,
    };
    let (original, expected) = match tokio::fs::metadata(&policy_path).await {
        Ok(_) => {
            let loaded = load(&policy_path).await?;
            (Some(loaded.document), Some(loaded.digest))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, None),
        Err(error) => return Err(error).context("checking policy source for initialization"),
    };
    let mut document = original.clone().unwrap_or_else(|| Document {
        spec_version: SpecVersion::Beta1,
        policies: BTreeMap::new(),
    });
    let policy_changed = match document.policies.get(policy_name) {
        Some(existing) => {
            ensure!(
                existing == &initialized,
                "policy '{policy_name}' already exists with a different definition"
            );
            false
        }
        None => {
            document
                .policies
                .insert(policy_name.to_owned(), initialized);
            true
        }
    };
    let edited =
        edit_config_router_policy_with_model(&raw, router_name, policy_name, Some(&strong_model))?;
    let candidate = crate::config::parse(&edited)?;
    validate_for_config(&candidate, &document)?;
    let digest = if policy_changed {
        write_atomic_unlocked(&policy_path, expected.as_deref(), &document)?
    } else {
        expected
            .clone()
            .context("existing policy has no source digest")?
    };
    if edited != raw
        && let Err(error) = write_config(config_path, &raw, &edited)
    {
        if !policy_changed {
            return Err(error.context("config update failed; policy source unchanged"));
        }
        let recovery = match original {
            Some(original) => {
                write_atomic_unlocked(&policy_path, Some(&digest), &original).map(|_| ())
            }
            None => {
                let current = load(&policy_path).await?;
                ensure!(
                    current.digest == digest,
                    "created policy changed before initialization recovery"
                );
                std::fs::remove_file(&policy_path)
                    .context("removing created policy after config update failed")
                    .and_then(|()| sync_parent(&policy_path))
            }
        };
        return match recovery {
            Ok(()) => Err(error.context("restored policy after config update failed")),
            Err(recovery) => Err(error.context(format!(
                "config update and policy recovery failed: {recovery:#}"
            ))),
        };
    }
    let mut changes = Vec::new();
    if policy_changed {
        changes.push(format!("created policy '{policy_name}'"));
    }
    if edited != raw {
        changes.push(format!("bound router 'bitrouter/{router_name}'"));
    }
    Ok(PolicyFileUpdate {
        path: policy_path,
        digest,
        parent_digest: expected,
        changes,
    })
}

fn route_supports_capability(
    config: &Config,
    route: &str,
    capability: bitrouter_ai::types::Capability,
) -> bool {
    match crate::config::routing_table::resolve_route_chain(
        config,
        route,
        &crate::gateway::pipeline::routing::RoutingPrefs::default(),
    ) {
        Ok(targets) => {
            !targets.is_empty()
                && targets
                    .iter()
                    .all(|target| target.model_constraints.capabilities.contains(&capability))
        }
        Err(error) => {
            tracing::debug!(%error, "policy initialization has no positive capability evidence");
            false
        }
    }
}

/// Publish source using a caller-supplied CAS token; source grants no authority.
pub async fn publish_candidate_file(
    config_path: &Path,
    candidate_path: &Path,
    expected_digest: &str,
) -> Result<PolicyFileUpdate> {
    publish_candidate_file_inner(config_path, candidate_path, expected_digest).await
}

async fn publish_candidate_file_inner(
    config_path: &Path,
    candidate_path: &Path,
    expected_digest: &str,
) -> Result<PolicyFileUpdate> {
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .context("reading policy publication configuration")?;
    let config = crate::config::parse(&raw)?;
    let active = load_for_config(&config, Some(config_path))
        .await?
        .context("no policy source is configured")?;
    let candidate = load(candidate_path).await?;
    validate_for_config(&config, &candidate.document)?;
    let path = active.file_path()?;
    ensure!(
        resolved_file_location(path)? != resolved_file_location(candidate_path)?,
        "candidate must be a separate file from the active source"
    );
    let changes = diff_explanations(&active.document, &candidate.document);
    let history_dir = default_history_dir(path);
    let record = publish_candidate(path, expected_digest, &candidate.document, &history_dir)?;
    Ok(PolicyFileUpdate {
        path: path.to_path_buf(),
        digest: record.child_digest,
        parent_digest: Some(record.parent_digest),
        changes,
    })
}

pub async fn rollback_files(
    config_path: &Path,
    target_digest: &str,
    expected_digest: &str,
) -> Result<PolicyFileUpdate> {
    rollback_files_inner(config_path, target_digest, expected_digest).await
}

async fn rollback_files_inner(
    config_path: &Path,
    target_digest: &str,
    expected_digest: &str,
) -> Result<PolicyFileUpdate> {
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .context("reading policy rollback configuration")?;
    let config = crate::config::parse(&raw)?;
    let active = load_for_config(&config, Some(config_path))
        .await?
        .context("no policy source is configured")?;
    let path = active.file_path()?;
    let history_dir = default_history_dir(path);
    let document = load_history_snapshot(&history_dir, target_digest)?;
    validate_for_config(&config, &document)?;
    let changes = diff_explanations(&active.document, &document);
    let record = rollback_to_digest(path, expected_digest, target_digest, &history_dir)?;
    Ok(PolicyFileUpdate {
        path: path.to_path_buf(),
        digest: record.child_digest,
        parent_digest: Some(record.parent_digest),
        changes,
    })
}

#[derive(Default)]
struct PolicySnapshot {
    routers: BTreeMap<String, Arc<PolicyTableRouter>>,
    administration: Option<bitrouter_ai::control::administration::PolicyReport>,
    #[cfg(feature = "multi-instance")]
    compiled: Option<CompiledPolicy>,
}

/// Exact serving generation retained across admission, selection and publication.
pub(crate) struct PolicyRoutingSnapshot(Arc<PolicySnapshot>);
impl PolicyRoutingSnapshot {
    #[cfg(feature = "multi-instance")]
    pub(crate) fn digest(&self) -> Option<&str> {
        self.0.compiled.as_ref().map(CompiledPolicy::digest)
    }
    #[cfg(feature = "multi-instance")]
    pub(crate) fn document(&self) -> Option<&Document> {
        self.0.compiled.as_ref().map(CompiledPolicy::source)
    }
}

pub(crate) struct PreparedPolicySnapshot(Arc<PolicySnapshot>);

/// One immutable source generation per request, swapped only after preparation.
pub struct PolicyRuntime {
    snapshot: RwLock<Arc<PolicySnapshot>>,
    decision_recorder: Option<Arc<PolicyDecisionJsonlRecorder>>,
    eval_decisions: PendingEvalDecisionStore,
    continuation_config: crate::config::ContinuationConfig,
    published: bool,
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
    async fn revalidate_context(
        &self,
        chain: &[RoutingTarget],
        ctx: &PipelineContext,
    ) -> crate::gateway::error::Result<()> {
        if ctx.extension::<PredictiveSingleTargetDispatch>().is_some() && chain.len() != 1 {
            return Err(crate::gateway::error::BitrouterError::bad_request(
                "predictive dispatch lost its single-target binding",
            ));
        }
        Ok(())
    }

    async fn resolve(
        &self,
        chain: &mut Vec<RoutingTarget>,
        ctx: &mut PipelineContext,
    ) -> crate::gateway::error::Result<()> {
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
        decision_recorder: Option<Arc<PolicyDecisionJsonlRecorder>>,
        eval_decisions: PendingEvalDecisionStore,
    ) -> Result<Arc<Self>> {
        let runtime = Arc::new(Self {
            snapshot: RwLock::new(Arc::new(PolicySnapshot::default())),
            decision_recorder,
            eval_decisions,
            continuation_config: config.continuation.clone(),
            published: false,
        });
        runtime.reload_for_config(config, config_path).await?;
        Ok(runtime)
    }

    /// Build from explicit in-memory source without host file/default discovery.
    pub(crate) fn from_document(
        config: &Config,
        document: Option<Document>,
        eval_decisions: PendingEvalDecisionStore,
    ) -> Result<Arc<Self>> {
        ensure!(
            config.policy.path.is_none() && !config.inherit_defaults,
            "published policy runtime requires explicit in-memory authority"
        );
        let loaded = match document {
            Some(document) => {
                validate_for_config(config, &document)?;
                Some(LoadedPolicy {
                    path: None,
                    digest: semantic_digest(&document)?,
                    document,
                })
            }
            None => {
                ensure!(
                    bound_policy_names(config).is_empty(),
                    "published policy bindings require a document"
                );
                None
            }
        };
        let runtime = Arc::new(Self {
            snapshot: RwLock::new(Arc::new(PolicySnapshot::default())),
            decision_recorder: None,
            eval_decisions,
            continuation_config: config.continuation.clone(),
            published: true,
        });
        runtime.commit(runtime.prepare_loaded(config, loaded)?);
        Ok(runtime)
    }

    pub(crate) fn from_retained_document(
        config: &Config,
        document: Document,
        eval_decisions: PendingEvalDecisionStore,
        selected: &str,
    ) -> Result<Arc<Self>> {
        ensure!(
            document.policies.contains_key(selected),
            "selected retained policy missing"
        );
        Self::from_document(config, Some(document), eval_decisions)
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
        ensure!(
            !self.published,
            "published policy runtime must be replaced as a complete configuration"
        );
        ensure!(
            config.continuation == self.continuation_config,
            "changing continuation settings requires a daemon restart"
        );
        self.prepare_loaded(config, load_for_config(config, config_path).await?)
    }

    fn prepare_loaded(
        &self,
        config: &Config,
        loaded: Option<LoadedPolicy>,
    ) -> Result<PreparedPolicySnapshot> {
        let compiled = loaded
            .as_ref()
            .map(|loaded| crate::model_policy::compile(&loaded.document))
            .transpose()?;
        let mut routers = BTreeMap::new();
        if let Some(compiled) = &compiled {
            for (name, selection) in compiled.selections() {
                let mut router =
                    PolicyTableRouter::new(PolicyTable::from_model_selection(selection))
                        .with_eval_observer(
                            self.eval_decisions.clone(),
                            name.clone(),
                            compiled.digest().to_owned(),
                            Default::default(),
                            Some(selection.default_tier.clone()),
                        );
                if let Some(recorder) = &self.decision_recorder {
                    router = router.with_shared_decision_recorder(recorder.clone());
                }
                routers.insert(name.clone(), Arc::new(router));
            }
        }
        Ok(PreparedPolicySnapshot(Arc::new(PolicySnapshot {
            routers,
            #[cfg(feature = "multi-instance")]
            compiled,
            administration: Some(
                bitrouter_ai::control::administration::PolicyReport::from_loaded(
                    config,
                    loaded.as_ref(),
                    bitrouter_ai::control::administration::PolicyView::Active,
                ),
            ),
        })))
    }

    pub(crate) fn commit(&self, prepared: PreparedPolicySnapshot) {
        match self.snapshot.write() {
            Ok(mut current) => *current = prepared.0,
            Err(poisoned) => *poisoned.into_inner() = prepared.0,
        }
    }

    pub(crate) fn routing_snapshot(&self) -> PolicyRoutingSnapshot {
        let snapshot = match self.snapshot.read() {
            Ok(current) => current.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        PolicyRoutingSnapshot(snapshot)
    }

    pub(crate) fn matches_routing_snapshot(&self, expected: &PolicyRoutingSnapshot) -> bool {
        Arc::ptr_eq(&self.routing_snapshot().0, &expected.0)
    }

    pub fn administration_snapshot(&self) -> bitrouter_ai::control::administration::PolicyReport {
        self.routing_snapshot()
            .0
            .administration
            .clone()
            .unwrap_or_else(|| {
                bitrouter_ai::control::administration::PolicyReport::from_loaded(
                    &Config::default(),
                    None,
                    bitrouter_ai::control::administration::PolicyView::Active,
                )
            })
    }
}

#[async_trait::async_trait]
impl ModelSelector for PolicyRuntime {
    fn preserves_context(&self) -> bool {
        true
    }

    async fn prepare_selection(
        &self,
        policy: &str,
        ctx: &mut PipelineContext,
    ) -> crate::gateway::error::Result<()> {
        let snapshot = self.routing_snapshot();
        if !snapshot.0.routers.contains_key(policy) {
            return Err(crate::gateway::error::BitrouterError::bad_request(format!(
                "router references unavailable policy '{policy}'"
            )));
        }
        ctx.insert_extension(Arc::new(snapshot));
        Ok(())
    }

    async fn select_policy(
        &self,
        policy: &str,
        ctx: &mut PipelineContext,
    ) -> crate::gateway::error::Result<()> {
        #[cfg(feature = "multi-instance")]
        if ctx
            .extension::<crate::gateway::shared::admitted::SharedAdmission>()
            .is_some()
            && ctx.extension::<PolicyRoutingSnapshot>().is_none()
        {
            return Err(crate::gateway::error::BitrouterError::UpstreamUnavailable);
        }
        let snapshot = match ctx.extension::<PolicyRoutingSnapshot>() {
            Some(snapshot) => snapshot.0.clone(),
            None => self.routing_snapshot().0,
        };
        let router = snapshot.routers.get(policy).ok_or_else(|| {
            crate::gateway::error::BitrouterError::bad_request(format!(
                "router references unavailable policy '{policy}'"
            ))
        })?;
        let invocation = match ctx.extension::<EvalInvocation>() {
            Some(invocation) => invocation,
            None => Arc::new(EvalInvocation::new(ctx.caller().user_id())),
        };
        if !ctx.has_event::<EvalInvocation>() {
            ctx.emit(invocation.as_ref().clone());
        }
        ctx.insert_extension(invocation.clone());
        let prepared = ctx.extension::<crate::gateway::routing::preparation::Prepared>();
        let assessment = prepared
            .as_ref()
            .and_then(|prepared| prepared.receipt.as_ref())
            .and_then(|receipt| receipt.assessment.as_ref());
        let input_model = ctx.model().to_string();
        let input_effort = ctx.require_generation_prompt()?.params.reasoning_effort;
        let mut decision = router.decision_for_routing(
            ctx.require_generation_prompt()?,
            ctx.headers(),
            assessment,
            false,
        );
        if let Some(plan) = ctx.extension::<ContinuationRequestPlan>()
            && let Some(adjustment) = plan.adjustment.as_ref()
        {
            router.apply_continuation_adjustment(&mut decision, adjustment)?;
        }
        let route_projection = decision.route_projection.clone();
        let (selected, captured) = router.record_bound_policy_decision(
            (ctx.request_id(), ctx.request_id()),
            invocation.as_ref(),
            input_model,
            input_effort,
            decision,
            ctx.headers(),
        );
        #[cfg(feature = "multi-instance")]
        if let Some(admission) =
            ctx.extension::<crate::gateway::shared::admitted::SharedAdmission>()
        {
            let name = router
                .eval_policy_name()
                .ok_or(crate::gateway::error::BitrouterError::UpstreamUnavailable)?;
            admission.retain_policy(
                name,
                false,
                captured.as_ref(),
                &PolicyRoutingSnapshot(snapshot.clone()),
            )?;
        }
        #[cfg(not(feature = "multi-instance"))]
        drop(captured);
        if let Some(target) = selected {
            ctx.set_model(target.model());
            if let Some(effort) = target.effort() {
                ctx.set_policy_reasoning_effort(effort)?;
            }
            if let Some(effort) = pinned_continuation_effort(ctx) {
                ctx.set_policy_reasoning_effort_override(effort)?;
            }
            mark_predictive_single_target(&route_projection, ctx);
        }
        Ok(())
    }
}
fn pinned_continuation_effort(
    ctx: &PipelineContext,
) -> Option<Option<bitrouter_ai::types::ReasoningEffort>> {
    ctx.extension::<ContinuationRequestPlan>().and_then(|plan| {
        plan.adjustment
            .as_ref()
            .and_then(crate::continuation::ContinuationAdjustment::pinned_effort_override)
    })
}

fn mark_predictive_single_target(route_projection: &str, ctx: &mut PipelineContext) {
    if bitrouter_ai::policy::RouteClass::parse_key(route_projection).is_some() {
        ctx.insert_extension(Arc::new(PredictiveSingleTargetDispatch));
    }
}

#[cfg(test)]
#[path = "model_policy/files_tests.rs"]
mod files_tests;
