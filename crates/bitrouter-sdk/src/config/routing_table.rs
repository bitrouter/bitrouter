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
use crate::language_model::types::{ApiProtocol, RoutingTarget};

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

    /// Snapshot the current `Config` — used by the daemon reload integration
    /// test to assert the table adopted a hot-reloaded config.
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

/// Build the routing target(s) for one `(provider, model)` pair.
///
/// A single-credential provider yields exactly one target. A
/// multi-account provider (`accounts:` set) yields one target per
/// account: `failover` keeps the declared order, `balance` applies a
/// round-robin rotation so the primary advances by one each request
/// and the load splits evenly. Either way the remaining accounts sit
/// later in the chain that `execute_with_fallback` walks, so they act
/// as failover targets for that request.
fn build_targets(
    provider_id: &str,
    provider: &crate::config::ProviderConfig,
    model_id: &str,
    inbound: Option<&ApiProtocol>,
) -> Vec<RoutingTarget> {
    // Protocol-native routing: prefer the inbound protocol when this upstream
    // supports it (a faithful same-protocol round-trip), else the provider's
    // configured default head.
    let protocol = select_protocol(&provider.protocols_for(model_id), inbound);
    // A per-protocol endpoint override lets one provider serve different
    // protocols at different paths (e.g. OpenAI under `/v1`, Anthropic Messages
    // under `/anthropic`). It applies to the provider base, not to an account
    // that pins its own host.
    let protocol_base = provider.endpoint_for(&protocol);

    if provider.accounts.is_empty() {
        let api_base = protocol_base
            .map(str::to_string)
            .unwrap_or_else(|| provider.api_base.clone());
        return vec![RoutingTarget {
            provider_name: provider_id.to_string(),
            service_id: model_id.to_string(),
            api_base,
            api_key: provider.api_key.clone(),
            api_protocol: protocol,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
        }];
    }

    let n = provider.accounts.len();
    let offset = match provider.account_strategy {
        crate::config::AccountStrategy::Failover => 0,
        crate::config::AccountStrategy::Balance => balance_offset(provider_id, n),
    };
    (0..n)
        .map(|i| {
            let idx = (i + offset) % n;
            let account = &provider.accounts[idx];
            let label = if account.label.is_empty() {
                format!("account-{}", idx + 1)
            } else {
                account.label.clone()
            };
            // An account that pins its own base keeps it (multi-region /
            // multi-org); otherwise the per-protocol endpoint, else the
            // provider base.
            let api_base = if !account.api_base.is_empty() {
                account.api_base.clone()
            } else {
                protocol_base
                    .map(str::to_string)
                    .unwrap_or_else(|| provider.api_base.clone())
            };
            RoutingTarget {
                provider_name: provider_id.to_string(),
                service_id: model_id.to_string(),
                api_base,
                api_key: account.api_key.clone(),
                api_protocol: protocol.clone(),
                account_label: Some(label),
                api_key_override: None,
                api_base_override: None,
                auth_scheme: Default::default(),
            }
        })
        .collect()
}

/// Pick the wire protocol for a target: the inbound protocol when the upstream
/// supports it (native — a faithful same-protocol round-trip), else the
/// provider's preferred head. `protocols` is non-empty —
/// [`ProviderConfig::protocols_for`](crate::config::ProviderConfig::protocols_for)
/// always yields at least one.
fn select_protocol(protocols: &[ApiProtocol], inbound: Option<&ApiProtocol>) -> ApiProtocol {
    if let Some(p) = inbound
        && protocols.contains(p)
    {
        return p.clone();
    }
    protocols
        .first()
        .cloned()
        .unwrap_or(ApiProtocol::ChatCompletions)
}

/// The rotation offset in `0..n` for a `balance` provider's next
/// request — a round-robin counter, so the primary account advances by
/// one each call and the load splits *exactly* evenly.
///
/// The counter is keyed per provider id (a `balance` provider with 2
/// accounts and one with 3 must each cycle over their own `n`, not a
/// shared sequence) and lives in a process-global map. A wall-clock
/// seed was tried first but fails on platforms whose `SystemTime` has
/// only microsecond granularity — `subsec_nanos() % 2` is then always
/// `0`. The brief `Mutex` (one `HashMap` lookup + increment) is the
/// one bit of state in this otherwise-pure resolution path.
fn balance_offset(provider_id: &str, n: usize) -> usize {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static COUNTERS: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    if n <= 1 {
        return 0;
    }
    let mut counters = COUNTERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("balance counter lock poisoned");
    let counter = counters.entry(provider_id.to_string()).or_insert(0);
    let offset = *counter % n;
    *counter = counter.wrapping_add(1);
    offset
}

/// Whether a provider carries every tag in `require_tags`.
fn provider_matches_tags(provider: &crate::config::ProviderConfig, require: &[String]) -> bool {
    require.iter().all(|t| provider.tags.contains(t))
}

/// Build the fallback chain for an explicit virtual model (Strategy 2),
/// honouring its [`VirtualModelStrategy`].
///
/// Each endpoint that names an *active* provider contributes one-or-more
/// (account-expanded) targets. The endpoint's `strategy` then decides the
/// chain order:
///
/// - [`Priority`](crate::config::VirtualModelStrategy::Priority): keep the
///   endpoints in declared YAML order; `prefs` do not reorder or filter them.
///   The operator's declared priority is authoritative — the chain only
///   *advances* past an endpoint when it fails with a retryable error.
/// - [`Cascade`](crate::config::VirtualModelStrategy::Cascade): treat the
///   endpoints as an unordered candidate set — apply `prefs.only` /
///   `prefs.ignore` / `prefs.require_tags` per-endpoint, then sort by
///   `prefs.sort` exactly as Strategy-3 auto-cascade does. Ordering is keyed
///   on the endpoint's provider id so account-expanded targets stay grouped.
fn resolve_virtual_model(
    clean: &str,
    virtual_model: &crate::config::VirtualModel,
    config: &Config,
    prefs: &RoutingPrefs,
) -> Result<Vec<RoutingTarget>> {
    use crate::config::VirtualModelStrategy;

    // Per-endpoint targets, paired with the provider id so `cascade` can sort
    // on it while keeping each provider's account-expanded targets together.
    let mut endpoints: Vec<(String, Vec<RoutingTarget>)> = Vec::new();
    for endpoint in &virtual_model.endpoints {
        let Some(provider) = config.providers.get(&endpoint.provider) else {
            continue;
        };
        if !provider.active {
            continue;
        }
        if virtual_model.strategy == VirtualModelStrategy::Cascade {
            // `priority` is an explicit, authoritative order, so only the
            // cascade strategy consults the `only` / `ignore` / `require_tags`
            // filters — the same filter set Strategy-3 auto-cascade applies.
            if !prefs.only.is_empty() && !prefs.only.contains(&endpoint.provider) {
                continue;
            }
            if prefs.ignore.contains(&endpoint.provider) {
                continue;
            }
            if !provider_matches_tags(provider, &prefs.require_tags) {
                continue;
            }
        }
        endpoints.push((
            endpoint.provider.clone(),
            build_targets(
                &endpoint.provider,
                provider,
                &endpoint.service_id,
                prefs.inbound_protocol.as_ref(),
            ),
        ));
    }

    // `cascade` re-orders the candidate endpoints by the request's SortOrder
    // (`Latency` / `Cost` have no metrics source yet, so they currently fall
    // back to the alphabetical-by-provider order — same as Strategy-3). A
    // stable sort keeps the declared order as the tiebreaker for endpoints
    // that share a provider id.
    if virtual_model.strategy == VirtualModelStrategy::Cascade {
        match prefs.sort {
            SortOrder::Alphabetical | SortOrder::Latency | SortOrder::Cost => {
                endpoints.sort_by(|a, b| a.0.cmp(&b.0));
            }
        }
    }

    let chain: Vec<RoutingTarget> = endpoints.into_iter().flat_map(|(_, t)| t).collect();
    if chain.is_empty() {
        return Err(BitrouterError::NotFound(format!(
            "virtual model '{clean}' has no active endpoints"
        )));
    }
    Ok(chain)
}

/// The shared Stage-0 + Strategy-1/2/3 resolution. Synchronous —
/// `ConfigRoutingTable` calls it under a read lock.
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
        return Ok(build_targets(
            provider_id,
            provider,
            model_id,
            prefs.inbound_protocol.as_ref(),
        ));
    }

    // ---- Strategy 2: explicit virtual model ----
    if let Some(virtual_model) = config.models.get(&clean) {
        return resolve_virtual_model(&clean, virtual_model, config, &prefs);
    }

    // ---- Strategy 3: auto-cascade across every provider declaring it ----
    // Collect per-provider so the cascade sort is by provider id; each
    // provider then contributes one-or-more (account-expanded) targets.
    let mut chain: Vec<(String, Vec<RoutingTarget>)> = Vec::new();
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
                build_targets(
                    provider_id,
                    provider,
                    &clean,
                    prefs.inbound_protocol.as_ref(),
                ),
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
    // they fall back to the alphabetical initial sort. Account-expanded
    // targets within one provider keep their build order.
    //
    // Native-protocol preference is a *tie-break only*: a stable secondary key
    // ranking providers that already serve the inbound protocol natively ahead
    // of those that would translate — but only among candidates the primary
    // order ranks equal. Today the primary key (provider id) is total, so this
    // never reorders; it becomes load-bearing once cost/latency scoring (which
    // can tie) replaces the alphabetical fallback. It never overrides the
    // primary order, so cost/latency stays authoritative.
    let inbound = prefs.inbound_protocol.as_ref();
    let serves_inbound_natively = |targets: &[RoutingTarget]| -> bool {
        matches!(inbound, Some(p) if targets.first().is_some_and(|t| &t.api_protocol == p))
    };
    match prefs.sort {
        SortOrder::Alphabetical | SortOrder::Latency | SortOrder::Cost => {
            chain.sort_by(|a, b| {
                // native-capable (true) sorts first → compare b against a.
                a.0.cmp(&b.0)
                    .then_with(|| serves_inbound_natively(&b.1).cmp(&serves_inbound_natively(&a.1)))
            });
        }
    }
    Ok(chain.into_iter().flat_map(|(_, t)| t).collect())
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
    // The inbound protocol is a per-request fact set by the pipeline (not a
    // preset knob); carry it so target construction can route natively.
    if extra.inbound_protocol.is_some() {
        base.inbound_protocol = extra.inbound_protocol.clone();
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

    // A virtual model whose endpoints are declared in *non-alphabetical*
    // provider order, so `priority` (declared order) and `cascade`
    // (alphabetical re-sort) produce observably different chains.
    const VIRTUAL_OUT_OF_ORDER: &str = r#"
providers:
  alpha:
    api_base: https://alpha.example/v1
    api_key: k-alpha
    models: [{ id: backend-a }]
  zeta:
    api_base: https://zeta.example/v1
    api_key: k-zeta
    models: [{ id: backend-z }]
models:
  combo:
    strategy: STRATEGY
    endpoints:
      - { provider: zeta, service_id: backend-z }
      - { provider: alpha, service_id: backend-a }
"#;

    #[tokio::test]
    async fn strategy_2_priority_keeps_declared_endpoint_order() {
        // `priority` is authoritative: the chain is exactly the declared
        // order (zeta, then alpha), regardless of provider-name ordering.
        let t = table(&VIRTUAL_OUT_OF_ORDER.replace("STRATEGY", "priority"));
        let chain = t
            .route_chain("combo", &RoutingPrefs::default(), &CallerContext::local())
            .await
            .unwrap();
        let order: Vec<&str> = chain.iter().map(|h| h.provider_name.as_str()).collect();
        assert_eq!(order, vec!["zeta", "alpha"], "declared order preserved");
    }

    #[tokio::test]
    async fn strategy_2_cascade_reorders_endpoints_by_sort() {
        // `cascade` treats the endpoints as a candidate set and applies the
        // cascade ordering (alphabetical-by-provider today) — flipping the
        // declared (zeta, alpha) into (alpha, zeta). Same config, different
        // strategy, provably different chain.
        let t = table(&VIRTUAL_OUT_OF_ORDER.replace("STRATEGY", "cascade"));
        let chain = t
            .route_chain("combo", &RoutingPrefs::default(), &CallerContext::local())
            .await
            .unwrap();
        let order: Vec<&str> = chain.iter().map(|h| h.provider_name.as_str()).collect();
        assert_eq!(order, vec!["alpha", "zeta"], "cascade re-sorts the chain");
    }

    #[tokio::test]
    async fn strategy_2_priority_ignores_routing_prefs_filters() {
        // The declared priority order is authoritative: an `ignore` pref must
        // NOT prune a `priority` virtual model's endpoints (contrast the
        // cascade test below).
        let t = table(&VIRTUAL_OUT_OF_ORDER.replace("STRATEGY", "priority"));
        let prefs = RoutingPrefs {
            ignore: vec!["zeta".to_string()],
            ..Default::default()
        };
        let chain = t
            .route_chain("combo", &prefs, &CallerContext::local())
            .await
            .unwrap();
        let order: Vec<&str> = chain.iter().map(|h| h.provider_name.as_str()).collect();
        assert_eq!(
            order,
            vec!["zeta", "alpha"],
            "priority is authoritative — ignore pref has no effect"
        );
    }

    #[tokio::test]
    async fn strategy_2_cascade_honors_ignore_pref() {
        // Under `cascade` the endpoints are an unordered candidate set, so an
        // `ignore` pref drops the matching endpoint from the chain.
        let t = table(&VIRTUAL_OUT_OF_ORDER.replace("STRATEGY", "cascade"));
        let prefs = RoutingPrefs {
            ignore: vec!["zeta".to_string()],
            ..Default::default()
        };
        let chain = t
            .route_chain("combo", &prefs, &CallerContext::local())
            .await
            .unwrap();
        let order: Vec<&str> = chain.iter().map(|h| h.provider_name.as_str()).collect();
        assert_eq!(order, vec!["alpha"], "cascade drops the ignored endpoint");
    }

    // `combo` over a tagged `alpha` (paid) and an untagged `zeta`, so a
    // `require_tags=[paid]` pref can prove cascade filters on tags while
    // priority does not.
    const VIRTUAL_TAGGED: &str = r#"
providers:
  alpha:
    api_base: https://alpha.example/v1
    api_key: k-alpha
    tags: [paid]
    models: [{ id: backend-a }]
  zeta:
    api_base: https://zeta.example/v1
    api_key: k-zeta
    models: [{ id: backend-z }]
models:
  combo:
    strategy: STRATEGY
    endpoints:
      - { provider: zeta, service_id: backend-z }
      - { provider: alpha, service_id: backend-a }
"#;

    #[tokio::test]
    async fn strategy_2_cascade_honors_require_tags() {
        // Cascade's filter set matches Strategy-3, which includes
        // `require_tags`: the untagged `zeta` endpoint is dropped.
        let t = table(&VIRTUAL_TAGGED.replace("STRATEGY", "cascade"));
        let prefs = RoutingPrefs {
            require_tags: vec!["paid".to_string()],
            ..Default::default()
        };
        let chain = t
            .route_chain("combo", &prefs, &CallerContext::local())
            .await
            .unwrap();
        let order: Vec<&str> = chain.iter().map(|h| h.provider_name.as_str()).collect();
        assert_eq!(
            order,
            vec!["alpha"],
            "cascade drops the endpoint whose provider lacks the required tag"
        );
    }

    #[tokio::test]
    async fn strategy_2_priority_ignores_require_tags() {
        // `priority` is authoritative — `require_tags` must not prune it.
        let t = table(&VIRTUAL_TAGGED.replace("STRATEGY", "priority"));
        let prefs = RoutingPrefs {
            require_tags: vec!["paid".to_string()],
            ..Default::default()
        };
        let chain = t
            .route_chain("combo", &prefs, &CallerContext::local())
            .await
            .unwrap();
        let order: Vec<&str> = chain.iter().map(|h| h.provider_name.as_str()).collect();
        assert_eq!(
            order,
            vec!["zeta", "alpha"],
            "priority is authoritative — require_tags has no effect"
        );
    }

    #[tokio::test]
    async fn strategy_2_defaults_to_priority_order() {
        // No explicit `strategy:` ⇒ Priority ⇒ declared order. Guards the
        // backward-compat default for pre-existing configs.
        let yaml = VIRTUAL_OUT_OF_ORDER
            .replace("    strategy: STRATEGY\n", "")
            .to_string();
        let t = table(&yaml);
        let chain = t
            .route_chain("combo", &RoutingPrefs::default(), &CallerContext::local())
            .await
            .unwrap();
        let order: Vec<&str> = chain.iter().map(|h| h.provider_name.as_str()).collect();
        assert_eq!(order, vec!["zeta", "alpha"], "default strategy = priority");
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

    // ===== multi-account provider =====

    const TWO_ACCOUNTS: &str = r#"
providers:
  opencode-go:
    api_base: https://opencode.ai/zen/go/v1
    models: [{ id: glm-5.1 }]
    accounts:
      - { api_key: key-a, label: sub-a }
      - { api_key: key-b, label: sub-b }
"#;

    #[tokio::test]
    async fn multi_account_expands_into_one_target_per_account() {
        // A provider:model route to a 2-account provider yields a
        // 2-target chain — execute_with_fallback walks it as failover.
        let t = table(TWO_ACCOUNTS);
        let chain = t
            .route_chain(
                "opencode-go:glm-5.1",
                &RoutingPrefs::default(),
                &CallerContext::local(),
            )
            .await
            .unwrap();
        assert_eq!(chain.len(), 2, "one target per account");
        // failover (the default) keeps declared order.
        assert_eq!(chain[0].api_key, "key-a");
        assert_eq!(chain[0].account_label.as_deref(), Some("sub-a"));
        assert_eq!(chain[1].api_key, "key-b");
        assert_eq!(chain[1].account_label.as_deref(), Some("sub-b"));
        // every target keeps the provider identity + service id.
        for hop in &chain {
            assert_eq!(hop.provider_name, "opencode-go");
            assert_eq!(hop.service_id, "glm-5.1");
            assert_eq!(hop.api_base, "https://opencode.ai/zen/go/v1");
        }
    }

    #[tokio::test]
    async fn single_credential_provider_still_yields_one_target() {
        // Regression: a provider with no `accounts:` is unchanged — one
        // target, `account_label` is None.
        let t = table(PROVIDERS);
        let chain = t
            .route_chain(
                "openai:gpt-5",
                &RoutingPrefs::default(),
                &CallerContext::local(),
            )
            .await
            .unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].api_key, "k-openai");
        assert!(chain[0].account_label.is_none());
    }

    #[tokio::test]
    async fn balance_strategy_keeps_every_account_in_the_chain() {
        // `balance` rotates the primary; the chain still contains all
        // accounts (the rest are that request's failover targets).
        let yaml = r#"
providers:
  opencode-go:
    api_base: https://opencode.ai/zen/go/v1
    models: [{ id: glm-5.1 }]
    account_strategy: balance
    accounts:
      - { api_key: key-a }
      - { api_key: key-b }
"#;
        let t = table(yaml);
        let chain = t
            .route_chain(
                "opencode-go:glm-5.1",
                &RoutingPrefs::default(),
                &CallerContext::local(),
            )
            .await
            .unwrap();
        assert_eq!(chain.len(), 2);
        let keys: std::collections::HashSet<&str> =
            chain.iter().map(|t| t.api_key.as_str()).collect();
        assert_eq!(
            keys,
            ["key-a", "key-b"].into_iter().collect(),
            "both accounts present regardless of rotation"
        );
        // unlabelled accounts get a positional `account-<n>` label.
        for hop in &chain {
            assert!(
                hop.account_label
                    .as_deref()
                    .is_some_and(|l| l.starts_with("account-")),
                "default label: {:?}",
                hop.account_label
            );
        }
    }

    #[tokio::test]
    async fn balance_strategy_rotates_the_primary_round_robin() {
        // The whole point of `balance` — consecutive requests must not
        // all land on the same primary account. Use a provider id
        // unique to this test so the round-robin counter starts clean.
        let yaml = r#"
providers:
  bal-rr-provider:
    api_base: https://example.invalid/v1
    models: [{ id: m }]
    account_strategy: balance
    accounts:
      - { api_key: key-a, label: a }
      - { api_key: key-b, label: b }
"#;
        let t = table(yaml);
        let mut primaries = Vec::new();
        for _ in 0..4 {
            let chain = t
                .route_chain(
                    "bal-rr-provider:m",
                    &RoutingPrefs::default(),
                    &CallerContext::local(),
                )
                .await
                .unwrap();
            primaries.push(chain[0].account_label.clone().unwrap());
        }
        // Round-robin over 2 accounts → strict a, b, a, b.
        assert_eq!(primaries, vec!["a", "b", "a", "b"], "round-robin rotation");
    }

    #[tokio::test]
    async fn account_api_base_override_wins_over_provider_base() {
        // A per-account `api_base` covers multi-region / multi-org
        // setups; an empty one inherits the provider base.
        let yaml = r#"
providers:
  acme:
    api_base: https://default.example/v1
    models: [{ id: m1 }]
    accounts:
      - { api_key: key-a, api_base: "https://us.example/v1" }
      - { api_key: key-b }
"#;
        let t = table(yaml);
        let chain = t
            .route_chain("acme:m1", &RoutingPrefs::default(), &CallerContext::local())
            .await
            .unwrap();
        assert_eq!(chain[0].api_base, "https://us.example/v1");
        assert_eq!(chain[1].api_base, "https://default.example/v1");
    }

    #[tokio::test]
    async fn auto_cascade_expands_accounts_in_place() {
        // `shared` is declared by a single-credential provider and a
        // 2-account provider — the cascade is provider-ordered, and the
        // multi-account provider contributes both of its accounts.
        let yaml = r#"
providers:
  alpha:
    api_base: https://alpha.example/v1
    api_key: k-alpha
    models: [{ id: shared }]
  zeta:
    api_base: https://zeta.example/v1
    models: [{ id: shared }]
    accounts:
      - { api_key: z1 }
      - { api_key: z2 }
"#;
        let t = table(yaml);
        let chain = t
            .route_chain("shared", &RoutingPrefs::default(), &CallerContext::local())
            .await
            .unwrap();
        assert_eq!(chain.len(), 3, "alpha + zeta×2");
        assert_eq!(chain[0].provider_name, "alpha");
        assert_eq!(chain[1].provider_name, "zeta");
        assert_eq!(chain[2].provider_name, "zeta");
        assert_eq!(chain[1].api_key, "z1");
        assert_eq!(chain[2].api_key, "z2");
    }

    // ===== protocol-native routing =====

    // Case 1: one provider serving its model over several protocols, with a
    // per-protocol endpoint for the Anthropic Messages path.
    const MULTI_PROTOCOL: &str = r#"
providers:
  minimax:
    api_base: https://api.minimax.io/v1
    api_key: k-mm
    api_protocol:
      - "*": [chat_completions, responses, messages]
    protocol_endpoints:
      messages: https://api.minimax.io/anthropic/v1
    models: [{ id: MiniMax-M2 }]
"#;

    /// Routing prefs carrying an inbound protocol, the rest default.
    fn prefs_inbound(p: ApiProtocol) -> RoutingPrefs {
        RoutingPrefs {
            inbound_protocol: Some(p),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn native_routing_picks_inbound_protocol_over_per_protocol_endpoint() {
        // Inbound Messages → the upstream supports it → route Messages
        // natively, over the per-protocol /anthropic endpoint.
        let t = table(MULTI_PROTOCOL);
        let chain = t
            .route_chain(
                "minimax:MiniMax-M2",
                &prefs_inbound(ApiProtocol::Messages),
                &CallerContext::local(),
            )
            .await
            .unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].api_protocol, ApiProtocol::Messages);
        assert_eq!(chain[0].api_base, "https://api.minimax.io/anthropic/v1");
    }

    #[tokio::test]
    async fn native_routing_picks_supported_non_head_protocol() {
        // Inbound Responses is supported but not the head; native preference
        // still selects it, at the provider's default base (no endpoint set).
        let t = table(MULTI_PROTOCOL);
        let chain = t
            .route_chain(
                "minimax:MiniMax-M2",
                &prefs_inbound(ApiProtocol::Responses),
                &CallerContext::local(),
            )
            .await
            .unwrap();
        assert_eq!(chain[0].api_protocol, ApiProtocol::Responses);
        assert_eq!(chain[0].api_base, "https://api.minimax.io/v1");
    }

    #[tokio::test]
    async fn falls_back_to_default_head_when_inbound_unsupported() {
        // GenerateContent isn't in the set → fall back to the preferred head
        // (chat_completions), at the default base.
        let t = table(MULTI_PROTOCOL);
        let chain = t
            .route_chain(
                "minimax:MiniMax-M2",
                &prefs_inbound(ApiProtocol::GenerateContent),
                &CallerContext::local(),
            )
            .await
            .unwrap();
        assert_eq!(chain[0].api_protocol, ApiProtocol::ChatCompletions);
        assert_eq!(chain[0].api_base, "https://api.minimax.io/v1");
    }

    #[tokio::test]
    async fn no_inbound_protocol_uses_default_head() {
        // Today's default path: no inbound protocol → the preferred head at the
        // provider base. Guards the backward-compatible behaviour.
        let t = table(MULTI_PROTOCOL);
        let chain = t
            .route_chain(
                "minimax:MiniMax-M2",
                &RoutingPrefs::default(),
                &CallerContext::local(),
            )
            .await
            .unwrap();
        assert_eq!(chain[0].api_protocol, ApiProtocol::ChatCompletions);
        assert_eq!(chain[0].api_base, "https://api.minimax.io/v1");
    }

    #[tokio::test]
    async fn native_preference_does_not_reorder_the_cascade() {
        // `shared-model` is served by anthropic (messages-native, by host
        // inference) and openai (chat). Even with inbound Messages, the primary
        // alphabetical order is authoritative — anthropic, then openai — proving
        // native preference is a tie-break that never changes which upstream is
        // chosen. Each target still gets its own per-target protocol.
        let t = table(PROVIDERS);
        let chain = t
            .route_chain(
                "shared-model",
                &prefs_inbound(ApiProtocol::Messages),
                &CallerContext::local(),
            )
            .await
            .unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].provider_name, "anthropic");
        assert_eq!(chain[1].provider_name, "openai");
        // anthropic serves Messages natively; openai (host-inferred chat) does
        // not support Messages → its configured head.
        assert_eq!(chain[0].api_protocol, ApiProtocol::Messages);
        assert_eq!(chain[1].api_protocol, ApiProtocol::ChatCompletions);
    }

    #[tokio::test]
    async fn native_routing_applies_through_a_virtual_model() {
        // Strategy 2: a `models:` virtual model whose endpoint is a
        // multi-protocol provider still routes each target natively.
        let yaml = r#"
providers:
  minimax:
    api_base: https://api.minimax.io/v1
    api_key: k-mm
    api_protocol:
      - "*": [chat_completions, messages]
    models: [{ id: MiniMax-M2 }]
models:
  combo:
    endpoints:
      - { provider: minimax, service_id: MiniMax-M2 }
"#;
        let t = table(yaml);
        let chain = t
            .route_chain(
                "combo",
                &prefs_inbound(ApiProtocol::Messages),
                &CallerContext::local(),
            )
            .await
            .unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].provider_name, "minimax");
        assert_eq!(chain[0].api_protocol, ApiProtocol::Messages);
    }

    #[tokio::test]
    async fn inbound_protocol_survives_preset_resolution() {
        // The inbound protocol (a caller pref) must survive the merge over the
        // preset-derived prefs base, so native routing still applies when a
        // request goes through an `@preset`.
        let yaml = r#"
providers:
  minimax:
    api_base: https://api.minimax.io/v1
    api_key: k-mm
    api_protocol:
      - "*": [chat_completions, messages]
    models: [{ id: MiniMax-M2 }]
presets:
  pick:
    model: MiniMax-M2
"#;
        let t = table(yaml);
        let chain = t
            .route_chain(
                "@pick",
                &prefs_inbound(ApiProtocol::Messages),
                &CallerContext::local(),
            )
            .await
            .unwrap();
        assert_eq!(chain[0].provider_name, "minimax");
        assert_eq!(chain[0].api_protocol, ApiProtocol::Messages);
    }

    #[tokio::test]
    async fn account_base_wins_over_per_protocol_endpoint() {
        // Per-target precedence in the multi-account branch: an account that
        // pins its own base keeps it (multi-region); an account without one
        // gets the per-protocol endpoint for the natively-selected protocol.
        let yaml = r#"
providers:
  mm:
    api_base: https://api.minimax.io/v1
    api_protocol:
      - "*": [chat_completions, messages]
    protocol_endpoints:
      messages: https://api.minimax.io/anthropic/v1
    models: [{ id: m }]
    accounts:
      - { api_key: k-pinned, label: pinned, api_base: "https://eu.minimax.io/v1" }
      - { api_key: k-default, label: default }
"#;
        let t = table(yaml);
        let chain = t
            .route_chain(
                "mm:m",
                &prefs_inbound(ApiProtocol::Messages),
                &CallerContext::local(),
            )
            .await
            .unwrap();
        assert_eq!(chain.len(), 2);
        // Both serve the inbound protocol natively.
        assert!(
            chain
                .iter()
                .all(|h| h.api_protocol == ApiProtocol::Messages)
        );
        let pinned = chain
            .iter()
            .find(|h| h.account_label.as_deref() == Some("pinned"))
            .unwrap();
        let default = chain
            .iter()
            .find(|h| h.account_label.as_deref() == Some("default"))
            .unwrap();
        // Pinned account keeps its own base, ignoring the per-protocol endpoint.
        assert_eq!(pinned.api_base, "https://eu.minimax.io/v1");
        // Account without a base gets the per-protocol /anthropic endpoint.
        assert_eq!(default.api_base, "https://api.minimax.io/anthropic/v1");
    }
}
