//! `ConfigRoutingTable` — a `RoutingTable` backed by `bitrouter.yaml`.
//!
//! Implements the full model-name resolution pipeline:
//!
//! - **Stage 0** — strip `@preset` / `:variant`, derive `RoutingPrefs`.
//! - **Strategy 1** — `provider:model_id` → direct route (chain length 1).
//! - **Strategy 2** — an explicit `models:` virtual model → its endpoint chain.
//! - **Strategy 3** — *auto-cascade* (the v1 built-in default): scan every
//!   active provider that declares the model and order them into a fallback
//!   chain. There is **no `DEFAULT_PROVIDER` fallback** — a model no provider
//!   declares is a clean 404.

use std::sync::RwLock;

use async_trait::async_trait;

use crate::caller::CallerContext;
use crate::config::{Config, presets::resolve_presets};
use crate::error::{BitrouterError, Result};
use crate::language_model::routing::{ModelInfo, RoutingPrefs, RoutingTable, SortOrder};
use crate::language_model::types::RoutingTarget;

/// A `RoutingTable` over an in-memory `bitrouter.yaml` config. Reloadable.
pub struct ConfigRoutingTable {
    config: RwLock<Config>,
    /// The path the config was loaded from, for `reload()`.
    path: Option<std::path::PathBuf>,
    /// Serialises `reload()` against itself. SIGHUP + `bitrouter reload`
    /// arriving close together used to race: each call did its own
    /// `load + discover_models` then wrote the result, last writer wins.
    /// Now we hold this mutex for the full reload sequence.
    reload_lock: tokio::sync::Mutex<()>,
}

impl ConfigRoutingTable {
    /// Build a routing table from an already-parsed config (no reload source).
    pub fn from_config(config: Config) -> Self {
        Self {
            config: RwLock::new(config),
            path: None,
            reload_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Build a routing table from an already-parsed config, **remembering the
    /// path it came from** so `reload()` can re-read it. Use this when the
    /// config was parsed (or post-processed, e.g. `auto_discover`) before the
    /// table was built but hot-reload is still wanted.
    pub fn from_config_with_path(config: Config, path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            config: RwLock::new(config),
            path: Some(path.into()),
            reload_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Load a routing table from a `bitrouter.yaml` path (enables `reload()`).
    pub async fn load(path: impl Into<std::path::PathBuf>) -> Result<Self> {
        let path = path.into();
        let config = crate::config::load(&path).await?;
        Ok(Self {
            config: RwLock::new(config),
            path: Some(path),
            reload_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Snapshot the current config (used by `RegistryRoutingTable`).
    pub fn snapshot_config(&self) -> Config {
        self.config.read().expect("config lock poisoned").clone()
    }

    /// Swap the table's `Config` for `fresh`, running model discovery
    /// against the new config first. Used by the daemon's reload path
    /// when there's no source file to re-read from (zero-config mode):
    /// the caller produces a fresh `Config` from
    /// `bitrouter_providers::zero_config` and hands it here. Holds the
    /// same `reload_lock` as the `RoutingTable::reload` impl so the two
    /// paths serialise against each other.
    pub async fn replace_config(&self, fresh: Config) -> Result<()> {
        let _guard = self.reload_lock.lock().await;
        let mut fresh = fresh;
        crate::config::discover_models(&mut fresh).await;
        *self.config.write().expect("config lock poisoned") = fresh;
        Ok(())
    }
}

fn build_target(
    provider_id: &str,
    provider: &crate::config::ProviderConfig,
    model_id: &str,
) -> RoutingTarget {
    RoutingTarget {
        provider_name: provider_id.to_string(),
        service_id: model_id.to_string(),
        api_base: provider.api_base.clone(),
        api_key: provider.api_key.clone(),
        api_protocol: provider.protocol_for(model_id),
        api_key_override: None,
        api_base_override: None,
    }
}

/// Whether a provider carries every tag in `require_tags`.
fn provider_matches_tags(provider: &crate::config::ProviderConfig, require: &[String]) -> bool {
    require.iter().all(|t| provider.tags.contains(t))
}

/// The shared Stage-0 + Strategy-1/2/3 resolution. Synchronous — both
/// `ConfigRoutingTable` and `RegistryRoutingTable` call it under a read lock.
pub fn resolve_route_chain(
    config: &Config,
    model: &str,
    caller_prefs: &RoutingPrefs,
) -> Result<Vec<RoutingTarget>> {
    // ---- Stage 0: strip @preset / :variant, derive prefs ----
    let resolution = resolve_presets(model, &config.presets, &config.variants)?;
    let clean = resolution.clean_model;
    // Caller-supplied prefs are additive on top of the preset-derived ones.
    let mut prefs = resolution.prefs;
    merge_prefs(&mut prefs, caller_prefs);

    // ---- Strategy 1: provider:model_id direct route ----
    if let Some((provider_id, model_id)) = clean.split_once(':')
        && let Some(provider) = config.providers.get(provider_id)
        && provider.active
    {
        return Ok(vec![build_target(provider_id, provider, model_id)]);
    }

    // ---- Strategy 2: explicit virtual model ----
    if let Some(virtual_model) = config.models.get(&clean) {
        let mut chain = Vec::new();
        for endpoint in &virtual_model.endpoints {
            if let Some(provider) = config.providers.get(&endpoint.provider)
                && provider.active
            {
                chain.push(build_target(
                    &endpoint.provider,
                    provider,
                    &endpoint.service_id,
                ));
            }
        }
        if chain.is_empty() {
            return Err(BitrouterError::NotFound(format!(
                "virtual model '{clean}' has no active endpoints"
            )));
        }
        return Ok(chain);
    }

    // ---- Strategy 3: auto-cascade across every provider declaring it ----
    let mut chain: Vec<(String, RoutingTarget)> = Vec::new();
    for (provider_id, provider) in &config.providers {
        if !provider.active {
            continue;
        }
        if !prefs.only.is_empty() && !prefs.only.contains(provider_id) {
            continue;
        }
        if prefs.ignore.contains(provider_id) {
            continue;
        }
        if !provider_matches_tags(provider, &prefs.require_tags) {
            continue;
        }
        if provider.models.iter().any(|m| m.id == clean) {
            chain.push((
                provider_id.clone(),
                build_target(provider_id, provider, &clean),
            ));
        }
    }

    if chain.is_empty() {
        // No `DEFAULT_PROVIDER` fallback — a clean 404.
        return Err(BitrouterError::NotFound(format!(
            "no active provider declares model '{clean}'"
        )));
    }

    // Order the cascade. `Latency` / `Cost` have no metrics source yet, so
    // they fall back to the alphabetical initial sort.
    match prefs.sort {
        SortOrder::Alphabetical | SortOrder::Latency | SortOrder::Cost => {
            chain.sort_by(|a, b| a.0.cmp(&b.0));
        }
    }
    Ok(chain.into_iter().map(|(_, t)| t).collect())
}

/// List models for a config (the §5.7 aggregation logic, shared by both tables).
pub fn list_models_for(config: &Config) -> Vec<ModelInfo> {
    // §5.7: an explicit `models:` segment is the source of truth when set.
    if !config.models.is_empty() {
        return config
            .models
            .iter()
            .map(|(id, vm)| ModelInfo {
                id: id.clone(),
                providers: vm.endpoints.iter().map(|e| e.provider.clone()).collect(),
            })
            .collect();
    }
    // Otherwise: the de-duplicated union of every active provider's models.
    let mut by_model: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for (provider_id, provider) in &config.providers {
        if !provider.active {
            continue;
        }
        for model in &provider.models {
            by_model
                .entry(model.id.clone())
                .or_default()
                .push(provider_id.clone());
        }
    }
    by_model
        .into_iter()
        .map(|(id, mut providers)| {
            providers.sort();
            ModelInfo { id, providers }
        })
        .collect()
}

#[async_trait]
impl RoutingTable for ConfigRoutingTable {
    async fn route_chain(
        &self,
        model: &str,
        prefs: &RoutingPrefs,
        _caller: &CallerContext,
    ) -> Result<Vec<RoutingTarget>> {
        // The resolution is synchronous; the read guard is dropped before we
        // return (no `.await` is held across it).
        let config = self.config.read().expect("config lock poisoned");
        resolve_route_chain(&config, model, prefs)
    }

    fn list_models(&self) -> Vec<ModelInfo> {
        let config = self.config.read().expect("config lock poisoned");
        list_models_for(&config)
    }

    fn model_info(&self, model: &str) -> Option<ModelInfo> {
        self.list_models().into_iter().find(|m| m.id == model)
    }

    async fn reload(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        // Hold the reload mutex for the whole load+discover+swap sequence so
        // a SIGHUP racing the control-socket `reload` command can't end up
        // with last-writer-wins between two concurrent fetches.
        let _guard = self.reload_lock.lock().await;
        let mut fresh = crate::config::load(path).await?;
        // Re-run model discovery so `auto_discover: true` providers pick up
        // upstream additions / removals. Best-effort: discovery failures
        // WARN; they do not abort the reload — same policy as the initial
        // assembly path.
        crate::config::discover_models(&mut fresh).await;
        *self.config.write().expect("config lock poisoned") = fresh;
        Ok(())
    }

    async fn preset_overrides(&self, model: &str) -> Result<crate::config::PromptOverrides> {
        // Same resolution as `route_chain` (Stage 0): strip `@preset:variant`
        // and return the preset's prompt body overrides. The synchronous part
        // is wrapped in a brief read-lock; no `.await` is held across it.
        let config = self.config.read().expect("config lock poisoned");
        let resolution = crate::config::resolve_presets(model, &config.presets, &config.variants)?;
        Ok(resolution.overrides)
    }
}

/// Merge `extra`'s knobs additively into `base` (caller prefs refine preset ones).
fn merge_prefs(base: &mut RoutingPrefs, extra: &RoutingPrefs) {
    if extra.sort != SortOrder::default() {
        base.sort = extra.sort;
    }
    for t in &extra.require_tags {
        if !base.require_tags.contains(t) {
            base.require_tags.push(t.clone());
        }
    }
    for p in &extra.only {
        if !base.only.contains(p) {
            base.only.push(p.clone());
        }
    }
    for p in &extra.ignore {
        if !base.ignore.contains(p) {
            base.ignore.push(p.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse;

    fn table(yaml: &str) -> ConfigRoutingTable {
        ConfigRoutingTable::from_config(parse(yaml).unwrap())
    }

    const PROVIDERS: &str = r#"
providers:
  openai:
    api_base: https://api.openai.com/v1
    api_key: k-openai
    models: [{ id: gpt-5 }, { id: shared-model }]
  anthropic:
    api_base: https://api.anthropic.com/v1
    api_key: k-anthropic
    models: [{ id: claude-sonnet-4-6 }, { id: shared-model }]
"#;

    #[tokio::test]
    async fn strategy_1_provider_prefix_routes_direct() {
        let t = table(PROVIDERS);
        let chain = t
            .route_chain(
                "openai:gpt-5",
                &RoutingPrefs::default(),
                &CallerContext::local(),
            )
            .await
            .unwrap();
        assert_eq!(chain.len(), 1, "provider:model is a length-1 chain");
        assert_eq!(chain[0].provider_name, "openai");
        assert_eq!(chain[0].service_id, "gpt-5");
    }

    #[tokio::test]
    async fn strategy_3_bare_name_auto_cascades_alphabetically() {
        let t = table(PROVIDERS);
        // `shared-model` is declared by both providers — auto-cascade builds a
        // fallback chain, ordered alphabetically by provider name.
        let chain = t
            .route_chain(
                "shared-model",
                &RoutingPrefs::default(),
                &CallerContext::local(),
            )
            .await
            .unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].provider_name, "anthropic");
        assert_eq!(chain[1].provider_name, "openai");
    }

    #[tokio::test]
    async fn strategy_2_virtual_model_beats_auto_cascade() {
        // `shared-model` is also defined as an explicit virtual model — the
        // explicit definition wins over Strategy-3 auto-cascade.
        let yaml = format!(
            "{PROVIDERS}\nmodels:\n  shared-model:\n    strategy: priority\n    \
             endpoints:\n      - {{ provider: openai, service_id: gpt-5 }}\n"
        );
        let t = table(&yaml);
        let chain = t
            .route_chain(
                "shared-model",
                &RoutingPrefs::default(),
                &CallerContext::local(),
            )
            .await
            .unwrap();
        assert_eq!(chain.len(), 1, "virtual model's explicit endpoints win");
        assert_eq!(chain[0].provider_name, "openai");
        assert_eq!(chain[0].service_id, "gpt-5");
    }

    #[tokio::test]
    async fn no_default_provider_fallback_unknown_model_is_404() {
        let t = table(PROVIDERS);
        // v0 had a hardcoded `DEFAULT_PROVIDER = "bitrouter"` fallback. v1 does
        // not — a model no provider declares is a clean 404.
        let err = t
            .route_chain(
                "totally-unknown-model",
                &RoutingPrefs::default(),
                &CallerContext::local(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.status(), 404);
    }

    #[tokio::test]
    async fn require_tags_filters_the_cascade() {
        let yaml = r#"
providers:
  paid-provider:
    api_base: https://paid.example/v1
    api_key: k1
    tags: [paid]
    models: [{ id: m1 }]
  free-provider:
    api_base: https://free.example/v1
    api_key: k2
    tags: [free]
    models: [{ id: m1 }]
"#;
        let t = table(yaml);
        let prefs = RoutingPrefs {
            require_tags: vec!["paid".to_string()],
            ..Default::default()
        };
        let chain = t
            .route_chain("m1", &prefs, &CallerContext::local())
            .await
            .unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].provider_name, "paid-provider");
    }

    #[tokio::test]
    async fn inactive_provider_is_skipped() {
        let yaml = r#"
providers:
  on:
    api_base: https://on.example/v1
    api_key: k1
    models: [{ id: m1 }]
  off:
    api_base: https://off.example/v1
    api_key: k2
    active: false
    models: [{ id: m1 }]
"#;
        let t = table(yaml);
        let chain = t
            .route_chain("m1", &RoutingPrefs::default(), &CallerContext::local())
            .await
            .unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].provider_name, "on");
    }

    #[tokio::test]
    async fn list_models_unions_provider_models() {
        let t = table(PROVIDERS);
        let models = t.list_models();
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"gpt-5"));
        assert!(ids.contains(&"shared-model"));
        // `shared-model` is offered by both providers
        let shared = models.iter().find(|m| m.id == "shared-model").unwrap();
        assert_eq!(shared.providers.len(), 2);
    }

    #[tokio::test]
    async fn preset_routing_flows_into_cascade() {
        // `@careful` requires the `paid` tag — only paid providers enter the chain.
        let yaml = r#"
providers:
  paid:
    api_base: https://paid.example/v1
    api_key: k1
    tags: [paid]
    models: [{ id: gpt-5 }]
  unpaid:
    api_base: https://unpaid.example/v1
    api_key: k2
    models: [{ id: gpt-5 }]
presets:
  careful:
    model: gpt-5
    routing: { require_tags: [paid] }
"#;
        let t = table(yaml);
        let chain = t
            .route_chain(
                "@careful",
                &RoutingPrefs::default(),
                &CallerContext::local(),
            )
            .await
            .unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].provider_name, "paid");
    }
}
