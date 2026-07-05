//! Merge public registry data into a parsed [`Config`].
//!
//! [`apply_registry`] is the bridge from "what the registry says exists" to
//! "what this bitrouter instance will route to". Its job is the subsystem's
//! whole purpose: make a canonical model id routable to a provider that serves
//! it. The rules (from the feature's principles):
//!
//! 1. Route a *canonical* model id to a provider that provides it.
//! 2. Merge every **public** registry provider — never `private` ones. Public
//!    `local_oauth` / `local_pkce` providers ARE merged (the OSS authenticates
//!    them with a local login); only the activation credential differs.
//! 3. The public `bitrouter` provider is the hosted BitRouter Cloud gateway and
//!    discovers the cloud-owned model list from `/models`; OSS does not infer
//!    that it serves every registry canonical model.
//! 4. Providers carry a [`ProviderClass`]; the auto-cascade orders by it.
//! 5. A provider is activated **only if its credentials are present**, except
//!    BitRouter Cloud which may authenticate through the local OAuth flow.
//!
//! Precedence is conservative: the merge never overwrites a field the user set
//! in `bitrouter.yaml`. The providers it configures are no longer compiled-in
//! built-ins (only the `bitrouter` cloud gateway is — see [`crate::builtin`]);
//! the merge applies their full transport (base URL, protocol map,
//! per-protocol endpoints), class, and env-resolved credential directly from
//! the fetched-or-cached registry data.

use bitrouter_sdk::config::{
    Config, Pattern, PatternMap, PricingConfig, PricingTierConfig, ProviderClass, ProviderConfig,
    ProviderModel, RateLimit, RegistryConfig, env_lookup,
};
use bitrouter_sdk::language_model::types::ProtocolList;

use crate::catalog::types::{Catalog, CatalogCost};
use crate::registry::cache::DiskCache;
use crate::registry::fetch::fetch_registry;
use crate::registry::types::{
    AutoSyncFeed, Billing, RegistryData, RegistryKind, RegistryPricing, RegistryProvider,
    RegistryRateLimits,
};

/// The provider id of the hosted BitRouter Cloud gateway.
const BITROUTER_CLOUD_ID: &str = "bitrouter";

/// Fetch the registry (honouring [`RegistryConfig`]) and return it, or `None`.
///
/// Best-effort and cache-backed, mirroring the runtime catalog policy: a fresh
/// on-disk cache short-circuits the network; otherwise a fetch refreshes the
/// cache; and if the fetch fails, stale cache data is served so a network
/// outage doesn't blank the routable provider set. Returns `None` when the
/// registry is disabled, or unreachable with no cache to fall back on.
pub async fn load_or_cached(registry: &RegistryConfig) -> Option<RegistryData> {
    if !registry.enabled {
        return None;
    }
    let cache = match DiskCache::default_path() {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(error = %e, "registry cache dir unresolved; fetching without cache");
            None
        }
    };
    // A fresh cache hit short-circuits the network entirely.
    if let Some(cache) = &cache
        && let Ok(Some(data)) = cache.read_fresh()
    {
        return Some(data);
    }
    match fetch_registry(&registry.url).await {
        Ok(data) => {
            if let Some(cache) = &cache
                && let Err(e) = cache.write(&data)
            {
                tracing::warn!(error = %e, "failed to write registry cache");
            }
            Some(data)
        }
        Err(e) => {
            tracing::warn!(error = %e, "registry fetch failed; using cached data if any");
            cache.and_then(|c| c.read_any().ok().flatten())
        }
    }
}

/// Read the disk-cached registry **without** touching the network. Returns
/// `None` when no readable cache exists. Used by the synchronous CLI paths (the
/// onboarding env-var hint, `bitrouter reload --env`) that need the provider
/// list but cannot await a fetch — on a never-fetched host they simply fall
/// back to the compiled-in cloud gateway only.
pub fn cached_registry() -> Option<RegistryData> {
    DiskCache::default_path()
        .ok()
        .and_then(|c| c.read_any().ok().flatten())
}

/// Merge `data` into `config`. No-op when `inherit_defaults` or
/// `registry.enabled` is false. Idempotent: re-running over an already-merged
/// config changes nothing (every write is guarded by an "is it empty / unset"
/// check).
pub fn apply_registry(config: &mut Config, data: &RegistryData) {
    if !config.inherit_defaults || !config.registry.enabled {
        return;
    }
    for provider in &data.providers {
        // Principle #2: merge every public, active registry provider.
        if !provider.is_active() || !provider.is_mergeable() {
            continue;
        }
        merge_provider(config, provider);
    }
}

/// Merge one public (non-private) registry provider into the config. Fully
/// configures it from the registry data — these providers are no longer
/// compiled-in built-ins (only the `bitrouter` cloud gateway is), so the merge
/// is the single place their transport, protocol map, and credential are
/// applied.
fn merge_provider(config: &mut Config, provider: &RegistryProvider) {
    let id = provider.name.as_str();
    let class = classify(provider);
    let protocol_map = provider_protocol_map(provider);

    if let Some(existing) = config.providers.get_mut(id) {
        // Already in the config (user-written, zero-config, or a prior pass):
        // only fill what is unset; never overwrite a user's field.
        if existing.class.is_none() {
            existing.class = Some(class);
        }
        if existing.models.is_empty() {
            existing.models = build_models(provider);
        }
        if existing.api_base.is_empty() {
            existing.api_base = provider.api_base.clone();
        }
        // Provider-level protocol globs (the gateways) — used by discovered
        // models, which carry no per-model protocol. Curated providers have no
        // provider-level globs (resolved per-model), so this is skipped for them.
        if existing.api_protocol.is_empty()
            && let Some(map) = &protocol_map
        {
            existing.api_protocol = map.clone();
        }
        if existing.protocol_endpoints.is_empty() {
            existing.protocol_endpoints = protocol_endpoints(provider);
        }
        // Runtime-discovered gateway (a `v1_models` feed, no curated models):
        // probe `/models` so an explicitly-listed gateway populates its catalog
        // the same way a zero-config one does. Never flip it off if the user set
        // it, and never when curated models are present.
        if provider.probes_v1_models() && existing.models.is_empty() && !existing.auto_discover {
            existing.auto_discover = true;
        }
        // Credential: an env-keyed provider resolves its key from the env var,
        // and drops out of routing if the key is absent (so it doesn't emit
        // broken upstream requests). OAuth / native providers authenticate via a
        // local login + a request-time `AuthApplier` (no env var), so they are
        // exempt — a listed-but-not-logged-in entry stays active and surfaces
        // its own error.
        if existing.accounts.is_empty()
            && existing.api_key.is_empty()
            && let Some(var) = provider.env_credential_var()
        {
            match env_lookup(&var).filter(|v| !v.is_empty()) {
                Some(key) => existing.api_key = key,
                None if id != BITROUTER_CLOUD_ID => existing.active = false,
                None => {}
            }
        }
        return;
    }

    // Not in the config. OAuth / native providers are never auto-added (they
    // need a local `bitrouter login`); an env-keyed provider is auto-added only
    // when its credential is present (principle #5).
    let Some(var) = provider.env_credential_var() else {
        return;
    };
    let Some(api_key) = env_lookup(&var).filter(|v| !v.is_empty()) else {
        return;
    };
    let models = build_models(provider);
    // A `v1_models` gateway has no curated models — probe `/models` at startup.
    let auto_discover = models.is_empty() && provider.probes_v1_models();
    let entry = ProviderConfig {
        api_key,
        api_base: provider.api_base.clone(),
        api_protocol: protocol_map.unwrap_or_default(),
        protocol_endpoints: protocol_endpoints(provider),
        models,
        class: Some(class),
        active: true,
        auto_discover,
        ..ProviderConfig::default()
    };
    config.providers.insert(id.to_string(), entry);
}

/// Classify a registry provider into a routing-preference [`ProviderClass`].
/// Community resellers are third-party; first-party providers split by billing.
fn classify(provider: &RegistryProvider) -> ProviderClass {
    match provider.kind.unwrap_or(if provider.community {
        RegistryKind::ThirdParty
    } else {
        RegistryKind::FirstParty
    }) {
        RegistryKind::Cloud => ProviderClass::BitrouterCloud,
        RegistryKind::Gateway => ProviderClass::GatewaySubscription,
        RegistryKind::ThirdParty => ProviderClass::ThirdPartyApi,
        RegistryKind::FirstParty => {
            if provider.billing == Billing::Subscription {
                ProviderClass::FirstPartySubscription
            } else {
                ProviderClass::FirstPartyApi
            }
        }
    }
}

/// The provider-level wire-protocol globs as the SDK's [`PatternMap`], or `None`
/// when the provider declares none (a curated provider, whose protocol is
/// resolved onto each model instead). Longest-match precedence is the SDK's.
fn provider_protocol_map(provider: &RegistryProvider) -> Option<PatternMap<ProtocolList>> {
    if provider.api_protocol.is_empty() {
        return None;
    }
    let mut map = PatternMap::new();
    for entry in &provider.api_protocol {
        for (pattern, set) in entry {
            map.push(Pattern::parse(pattern), set.to_protocol_list());
        }
    }
    Some(map)
}

/// The provider's per-protocol base-URL overrides as the SDK config map.
fn protocol_endpoints(provider: &RegistryProvider) -> std::collections::HashMap<String, String> {
    provider
        .protocol_endpoints
        .as_ref()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

/// Translate the registry's per-model entries into `ProviderModel`s — the
/// canonical id is the match key, `provider_model_id` the upstream dispatch id,
/// and the dist-resolved protocol set rides on each model.
fn build_models(provider: &RegistryProvider) -> Vec<ProviderModel> {
    provider
        .models
        .iter()
        .map(|m| ProviderModel {
            id: m.id.clone(),
            provider_model_id: Some(m.provider_model_id.clone()),
            api_protocol: Some(m.api_protocol.to_protocol_list()),
            rate_limits: m.rate_limits.as_ref().map(map_rate_limits),
            pricing: m.pricing.as_ref().and_then(map_pricing),
        })
        .collect()
}

fn map_rate_limits(rl: &RegistryRateLimits) -> RateLimit {
    RateLimit {
        requests_per_minute: rl.requests_per_minute,
        tokens_per_minute: rl.tokens_per_minute,
    }
}

/// Map registry pricing onto config pricing. OSS metering tracks the base
/// no-cache input rate, the text output rate, and context tiers (USD per 1M
/// tokens == µUSD per token). Returns `None` when no usable rate is present.
fn map_pricing(p: &RegistryPricing) -> Option<PricingConfig> {
    let input = p.input_tokens.as_ref().and_then(|i| i.no_cache);
    let output = p.output_tokens.as_ref().and_then(|o| o.text);
    let context_tiers: Vec<PricingTierConfig> = p
        .context_tiers
        .iter()
        .map(|t| PricingTierConfig {
            above_input_tokens: t.above_input_tokens,
            input_micro_usd_per_token: t
                .input_tokens
                .as_ref()
                .and_then(|i| i.no_cache)
                .unwrap_or(0.0),
            output_micro_usd_per_token: t
                .output_tokens
                .as_ref()
                .and_then(|o| o.text)
                .unwrap_or(0.0),
        })
        .collect();
    if input.is_none() && output.is_none() && context_tiers.is_empty() {
        return None;
    }
    Some(PricingConfig {
        input_micro_usd_per_token: input.unwrap_or(0.0),
        output_micro_usd_per_token: output.unwrap_or(0.0),
        context_tiers,
    })
}

/// Enrich `models_dev` auto-sync providers with their FULL models.dev catalog.
///
/// Improvement-1 "full catalog beyond canonical" for the `models_dev` feed: the
/// registry curates these providers FROM models.dev, and the OSS reads the same
/// channel at runtime to pull the rest of the catalog. For each public,
/// `models_dev`-feed provider that the registry merge placed into `config`, add
/// every models.dev model whose native id is not already represented — neither
/// as one of the provider's existing OSS model ids nor as a curated model's
/// `provider_model_id`. That keeps the curated canonical entries at the highest
/// route priority and never duplicates an upstream model the registry already
/// curates. (`v1_models` feeds discover via the SDK's `/models` probe instead.)
///
/// No-op when `inherit_defaults` / `registry.enabled` is false. Idempotent (the
/// "already represented" set guards re-runs) and best-effort (an absent catalog
/// or provider key simply leaves the curated models in place).
pub fn apply_catalog(config: &mut Config, data: &RegistryData, catalog: &Catalog) {
    if !config.inherit_defaults || !config.registry.enabled {
        return;
    }
    for provider in &data.providers {
        let Some(sync) = provider.discovery_feed() else {
            continue;
        };
        if sync.feed != AutoSyncFeed::ModelsDev || !provider.is_mergeable() {
            continue;
        }
        // Only enrich a provider the merge actually placed into the config.
        let Some(entry) = config.providers.get_mut(&provider.name) else {
            continue;
        };
        // models.dev provider key: the explicit override, else the provider name.
        let key = sync.key.as_deref().unwrap_or(provider.name.as_str());
        let Some(cat) = catalog.get(key) else {
            continue;
        };
        // Canonical priority: never add an id already present as an OSS model id
        // or as a curated model's upstream id.
        let mut represented: std::collections::HashSet<String> = entry
            .models
            .iter()
            .flat_map(|m| std::iter::once(m.id.clone()).chain(m.provider_model_id.clone()))
            .collect();
        let mut added = 0usize;
        for (model_id, meta) in &cat.models {
            if !represented.insert(model_id.clone()) {
                continue;
            }
            entry.models.push(ProviderModel {
                id: model_id.clone(),
                // Native id == OSS id; no canonical translation.
                provider_model_id: None,
                // The provider-level mapping governs (a built-in's set, filled by
                // `apply_builtin_defaults`, or the openai-compatible default).
                api_protocol: None,
                rate_limits: None,
                pricing: meta.cost.as_ref().and_then(map_catalog_cost),
            });
            added += 1;
        }
        if added > 0 {
            tracing::debug!(
                provider = %provider.name,
                added,
                "enriched provider catalog from models.dev"
            );
        }
    }
}

/// Map a models.dev per-1M-token cost onto the SDK pricing config (USD per 1M
/// tokens == µUSD per token). Returns `None` when no rate is published.
fn map_catalog_cost(cost: &CatalogCost) -> Option<PricingConfig> {
    if cost.input.is_none() && cost.output.is_none() {
        return None;
    }
    Some(PricingConfig {
        input_micro_usd_per_token: cost.input.unwrap_or(0.0),
        output_micro_usd_per_token: cost.output.unwrap_or(0.0),
        context_tiers: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::types::{CatalogModel, CatalogProvider};
    use crate::registry::types::{
        AutoSync, CanonicalModel, InputTokenPricing, OutputTokenPricing, ProtocolSet,
        RegistryAccess, RegistryAuth, RegistryAuthKind, RegistryModel, RegistryProtocol,
    };
    use bitrouter_sdk::language_model::types::{ApiProtocol, ProtocolList};

    fn provider(name: &str) -> RegistryProvider {
        RegistryProvider {
            name: name.to_string(),
            display_name: None,
            api_base: format!("https://{name}.example/v1"),
            api_protocol: Vec::new(),
            protocol_endpoints: None,
            models: vec![RegistryModel {
                id: "deepseek/deepseek-v3.2".to_string(),
                provider_model_id: "deepseek-v3.2".to_string(),
                api_protocol: ProtocolSet::One(RegistryProtocol::Openai),
                pricing: Some(RegistryPricing {
                    input_tokens: Some(InputTokenPricing {
                        no_cache: Some(0.27),
                    }),
                    output_tokens: Some(OutputTokenPricing { text: Some(0.41) }),
                    context_tiers: Vec::new(),
                }),
                rate_limits: None,
            }],
            status: "active".to_string(),
            kind: None,
            auth: None,
            doc_url: None,
            community: false,
            access: Some(RegistryAccess::ApiKey),
            byok: Some(true),
            auto_sync: None,
            billing: Billing::Token,
        }
    }

    fn data_with(providers: Vec<RegistryProvider>, canonical: Vec<&str>) -> RegistryData {
        RegistryData {
            providers,
            canonical: canonical
                .into_iter()
                .map(|id| CanonicalModel { id: id.to_string() })
                .collect(),
        }
    }

    /// Mutating `std::env` is process-global; serialize env-touching tests.
    fn with_env<R>(key: &str, value: Option<&str>, f: impl FnOnce() -> R) -> R {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _g = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let prev = std::env::var(key).ok();
        // SAFETY: the test process owns its env; the mutex serialises access.
        unsafe {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        let out = f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        out
    }

    #[test]
    fn classifies_by_community_and_billing() {
        let mut first_party = provider("zai");
        first_party.community = false;
        first_party.billing = Billing::Token;
        assert_eq!(classify(&first_party), ProviderClass::FirstPartyApi);

        let mut sub = provider("zai-coding-plan");
        sub.billing = Billing::Subscription;
        assert_eq!(classify(&sub), ProviderClass::FirstPartySubscription);

        let mut reseller = provider("chutes");
        reseller.community = true;
        // community wins even if (oddly) flagged subscription.
        reseller.billing = Billing::Subscription;
        assert_eq!(classify(&reseller), ProviderClass::ThirdPartyApi);
    }

    #[test]
    fn credential_present_inserts_active_provider_with_models() {
        // Unique env var name avoids racing other modules' env tests.
        with_env("REGTESTPROV_API_KEY", Some("sk-test"), || {
            let mut config = Config::default();
            let p = provider("regtestprov");
            apply_registry(
                &mut config,
                &data_with(vec![p], vec!["deepseek/deepseek-v3.2"]),
            );
            let merged = config
                .providers
                .get("regtestprov")
                .expect("credentialed provider must be inserted");
            assert!(merged.active);
            assert_eq!(merged.api_key, "sk-test");
            assert_eq!(merged.api_base, "https://regtestprov.example/v1");
            assert_eq!(merged.class, Some(ProviderClass::FirstPartyApi));
            assert_eq!(merged.models.len(), 1);
            let model = &merged.models[0];
            assert_eq!(model.id, "deepseek/deepseek-v3.2");
            assert_eq!(model.provider_model_id.as_deref(), Some("deepseek-v3.2"));
            // Registry-only provider (no built-in): the dist-resolved protocol
            // is the model's only source, set as the per-model override.
            assert_eq!(
                model.api_protocol,
                Some(ProtocolList(vec![ApiProtocol::ChatCompletions]))
            );
            let pricing = model.pricing.as_ref().expect("pricing mapped");
            assert_eq!(pricing.input_micro_usd_per_token, 0.27);
            assert_eq!(pricing.output_micro_usd_per_token, 0.41);
        });
    }

    #[test]
    fn merged_provider_carries_per_model_protocol() {
        // The providers are no longer compiled-in built-ins, so the merge is the
        // sole source of their protocol: each model carries its dist-resolved
        // protocol set (an ordered set like [chat, responses] is preserved).
        let mut config = Config::default();
        config.providers.insert(
            "openai".to_string(),
            ProviderConfig {
                active: true,
                ..ProviderConfig::default()
            },
        );
        // `provider("openai")`'s model resolves to `openai` (chat) in the dist.
        apply_registry(&mut config, &data_with(vec![provider("openai")], vec![]));
        let openai = &config.providers["openai"];
        assert!(!openai.models.is_empty(), "registry catalog is merged in");
        assert_eq!(
            openai.models[0].api_protocol,
            Some(ProtocolList(vec![ApiProtocol::ChatCompletions])),
            "the merge pins each model's dist-resolved protocol"
        );
    }

    #[test]
    fn credential_absent_skips_provider() {
        with_env("REGABSENTPROV_API_KEY", None, || {
            let mut config = Config::default();
            let p = provider("regabsentprov");
            apply_registry(&mut config, &data_with(vec![p], vec![]));
            assert!(
                !config.providers.contains_key("regabsentprov"),
                "no credential ⇒ provider must not be activated"
            );
        });
    }

    #[test]
    fn listed_env_provider_without_key_is_marked_inactive() {
        // A user lists an env-keyed provider bare (active by default) but has no
        // key: the merge must drop it out of routing (the behaviour that moved
        // here from `apply_builtin_defaults`), not leave a keyless active entry.
        with_env("LISTEDENVPROV_API_KEY", None, || {
            let mut config = Config::default();
            config.providers.insert(
                "listedenvprov".to_string(),
                ProviderConfig {
                    active: true,
                    ..ProviderConfig::default()
                },
            );
            apply_registry(
                &mut config,
                &data_with(vec![provider("listedenvprov")], vec![]),
            );
            assert!(
                !config.providers["listedenvprov"].active,
                "an env-keyed provider with no key must be marked inactive"
            );
        });
    }

    #[test]
    fn listed_oauth_provider_without_key_stays_active() {
        // An OAuth provider listed bare has no env key — it authenticates via a
        // local login + request-time applier, so the merge must NOT mark it
        // inactive (exempt from the keyless-inactive rule).
        with_env("OAUTHPROV_API_KEY", None, || {
            let mut config = Config::default();
            config.providers.insert(
                "oauthprov".to_string(),
                ProviderConfig {
                    active: true,
                    ..ProviderConfig::default()
                },
            );
            let mut p = provider("oauthprov");
            p.auth = Some(RegistryAuth {
                kind: RegistryAuthKind::Oauth,
                env: None,
                header: None,
                extra_headers: None,
                handler: Some("oauthprov".to_string()),
                params: None,
            });
            apply_registry(&mut config, &data_with(vec![p], vec![]));
            assert!(
                config.providers["oauthprov"].active,
                "an OAuth provider authenticates via login, so it stays active"
            );
        });
    }

    #[test]
    fn private_provider_is_never_merged() {
        with_env("PRIVATEPROV_API_KEY", Some("sk-test"), || {
            let mut config = Config::default();
            let mut p = provider("privateprov");
            p.access = Some(RegistryAccess::Private);
            apply_registry(&mut config, &data_with(vec![p], vec![]));
            assert!(
                !config.providers.contains_key("privateprov"),
                "access=private ⇒ never merged, even with a credential set"
            );
        });
    }

    #[test]
    fn legacy_byok_false_still_resolves_to_private() {
        // An older dist / cache carries the derived `byok` alias with no
        // explicit `access`. `byok: false` must still be read as `private`.
        let mut p = provider("legacyprivate");
        p.access = None;
        p.byok = Some(false);
        assert_eq!(p.access(), RegistryAccess::Private);
        assert!(!p.is_mergeable());
    }

    #[test]
    fn v1_models_gateway_gets_auto_discover_on_merge() {
        with_env("GATEWAYPROV_API_KEY", Some("sk-test"), || {
            let mut config = Config::default();
            let mut p = provider("gatewayprov");
            p.models = Vec::new(); // a gateway curates no models
            p.auto_sync = Some(AutoSync {
                feed: AutoSyncFeed::V1Models,
                key: None,
                url: None,
            });
            apply_registry(&mut config, &data_with(vec![p], vec![]));
            let merged = config
                .providers
                .get("gatewayprov")
                .expect("v1_models gateway with a credential is merged");
            assert!(
                merged.auto_discover,
                "a v1_models gateway with no curated models must probe /models"
            );
        });
    }

    #[test]
    fn inactive_status_provider_is_skipped() {
        with_env("STAGINGPROV_API_KEY", Some("sk-test"), || {
            let mut config = Config::default();
            let mut p = provider("stagingprov");
            p.status = "staging".to_string();
            apply_registry(&mut config, &data_with(vec![p], vec![]));
            assert!(!config.providers.contains_key("stagingprov"));
        });
    }

    fn catalog_with(provider: &str, models: Vec<(&str, Option<(f64, f64)>)>) -> Catalog {
        let models = models
            .into_iter()
            .map(|(id, cost)| {
                let cost = cost.map(|(input, output)| CatalogCost {
                    input: Some(input),
                    output: Some(output),
                });
                (id.to_owned(), CatalogModel { cost })
            })
            .collect();
        [(provider.to_owned(), CatalogProvider { models })]
            .into_iter()
            .collect()
    }

    #[test]
    fn models_dev_catalog_enriches_beyond_canonical() {
        with_env("DEEPSEEK_API_KEY", Some("sk-test"), || {
            let mut config = Config::default();
            let mut p = provider("deepseek"); // curated: deepseek/deepseek-v3.2 → deepseek-v3.2
            p.auto_sync = Some(AutoSync {
                feed: AutoSyncFeed::ModelsDev,
                key: None,
                url: None,
            });
            let data = data_with(vec![p], vec!["deepseek/deepseek-v3.2"]);
            apply_registry(&mut config, &data);
            // The catalog re-lists the curated upstream id (must NOT duplicate)
            // plus a genuinely new model (must be added, with pricing).
            let catalog = catalog_with(
                "deepseek",
                vec![
                    ("deepseek-v3.2", None),
                    ("deepseek-coder", Some((0.14, 0.28))),
                ],
            );
            apply_catalog(&mut config, &data, &catalog);

            let entry = config.providers.get("deepseek").expect("merged");
            let ids: Vec<&str> = entry.models.iter().map(|m| m.id.as_str()).collect();
            assert!(
                ids.contains(&"deepseek/deepseek-v3.2"),
                "curated canonical model is kept (highest priority)"
            );
            assert!(
                ids.contains(&"deepseek-coder"),
                "a non-curated models.dev model is added (full catalog)"
            );
            assert!(
                !ids.contains(&"deepseek-v3.2"),
                "the curated model's upstream id is not re-added as a native duplicate"
            );
            let coder = entry
                .models
                .iter()
                .find(|m| m.id == "deepseek-coder")
                .unwrap();
            let pricing = coder.pricing.as_ref().expect("priced from models.dev");
            assert_eq!(pricing.input_micro_usd_per_token, 0.14);
            assert_eq!(pricing.output_micro_usd_per_token, 0.28);

            // Idempotent: a second pass adds nothing.
            let before = entry.models.len();
            apply_catalog(&mut config, &data, &catalog);
            assert_eq!(config.providers["deepseek"].models.len(), before);
        });
    }

    #[test]
    fn v1_models_provider_is_not_models_dev_enriched() {
        // A `v1_models` feed discovers via `/models`, never models.dev — even if
        // a catalog entry happens to exist under the provider name.
        with_env("GW2_API_KEY", Some("sk-test"), || {
            let mut config = Config::default();
            let mut p = provider("gw2");
            p.models = Vec::new();
            p.auto_sync = Some(AutoSync {
                feed: AutoSyncFeed::V1Models,
                key: None,
                url: None,
            });
            let data = data_with(vec![p], vec![]);
            apply_registry(&mut config, &data);
            let catalog = catalog_with("gw2", vec![("some-model", Some((1.0, 2.0)))]);
            apply_catalog(&mut config, &data, &catalog);
            assert!(
                config.providers["gw2"].models.is_empty(),
                "a v1_models provider must not be enriched from models.dev"
            );
        });
    }

    #[test]
    fn user_declared_fields_are_not_overwritten() {
        // A user listed the provider with an explicit api_base + one model. The
        // merge must keep both and only fill the missing class.
        let mut config = Config::default();
        let mut user = ProviderConfig {
            api_base: "https://gateway.internal/v1".to_string(),
            api_key: "sk-user".to_string(),
            active: true,
            ..ProviderConfig::default()
        };
        user.models = vec![ProviderModel {
            id: "custom/model".to_string(),
            provider_model_id: None,
            api_protocol: None,
            rate_limits: None,
            pricing: None,
        }];
        config.providers.insert("regtestprov".to_string(), user);

        apply_registry(
            &mut config,
            &data_with(vec![provider("regtestprov")], vec![]),
        );
        let merged = &config.providers["regtestprov"];
        assert_eq!(merged.api_base, "https://gateway.internal/v1");
        assert_eq!(merged.models.len(), 1, "user's model list is preserved");
        assert_eq!(merged.models[0].id, "custom/model");
        assert_eq!(merged.class, Some(ProviderClass::FirstPartyApi));
    }

    #[test]
    fn bitrouter_cloud_is_merged_from_public_registry_and_discovers_models() {
        let mut config = Config::default();
        // The hosted gateway is present (as the env/sign-in path would add it).
        config
            .providers
            .insert(BITROUTER_CLOUD_ID.to_string(), ProviderConfig::default());
        let mut bitrouter = provider(BITROUTER_CLOUD_ID);
        bitrouter.display_name = Some("BitRouter Cloud".to_string());
        bitrouter.kind = Some(crate::registry::types::RegistryKind::Cloud);
        bitrouter.api_base = "https://api.bitrouter.ai/v1".to_string();
        bitrouter.models = Vec::new();
        bitrouter.auto_sync = Some(AutoSync {
            feed: AutoSyncFeed::V1Models,
            key: None,
            url: Some("https://api.bitrouter.ai/v1".to_string()),
        });
        bitrouter.auth = Some(RegistryAuth {
            kind: RegistryAuthKind::Bearer,
            env: Some("BITROUTER_API_KEY".to_string()),
            header: None,
            extra_headers: None,
            handler: None,
            params: None,
        });
        let data = data_with(
            vec![bitrouter],
            vec!["anthropic/claude-sonnet-4.6", "deepseek/deepseek-v3.2"],
        );
        apply_registry(&mut config, &data);
        let cloud = &config.providers[BITROUTER_CLOUD_ID];
        assert_eq!(cloud.class, Some(ProviderClass::BitrouterCloud));
        assert_eq!(cloud.api_base, "https://api.bitrouter.ai/v1");
        assert!(
            cloud.auto_discover,
            "public BitRouter Cloud provider should discover its cloud-owned catalog"
        );
        assert!(
            cloud.models.is_empty(),
            "OSS must not fill BitRouter Cloud from the registry canonical list"
        );
        assert!(
            cloud.active,
            "a configured BitRouter Cloud provider may authenticate via OAuth, not only BITROUTER_API_KEY"
        );
    }

    #[test]
    fn disabled_registry_is_a_noop() {
        with_env("REGTESTPROV_API_KEY", Some("sk-test"), || {
            let mut config = Config::default();
            config.registry.enabled = false;
            apply_registry(
                &mut config,
                &data_with(vec![provider("regtestprov")], vec![]),
            );
            assert!(config.providers.is_empty());
        });
    }

    #[test]
    fn inherit_defaults_false_is_a_noop() {
        with_env("REGTESTPROV_API_KEY", Some("sk-test"), || {
            let mut config = Config {
                inherit_defaults: false,
                ..Config::default()
            };
            apply_registry(
                &mut config,
                &data_with(vec![provider("regtestprov")], vec![]),
            );
            assert!(config.providers.is_empty());
        });
    }

    #[test]
    fn apply_registry_is_idempotent() {
        // The merge re-runs on every reload, so a second pass over the same
        // data must not duplicate models or change the result.
        with_env("REGTESTPROV_API_KEY", Some("sk-test"), || {
            let data = data_with(
                vec![provider("regtestprov")],
                vec!["anthropic/claude-sonnet-4.6", "deepseek/deepseek-v3.2"],
            );
            let mut config = Config::default();
            // Include hosted cloud so a reload does not synthesize canonical
            // models for it; cloud discovers its own catalog from /models.
            config
                .providers
                .insert(BITROUTER_CLOUD_ID.to_string(), ProviderConfig::default());

            apply_registry(&mut config, &data);
            let merged_models = config.providers["regtestprov"].models.len();
            let cloud_models = config.providers[BITROUTER_CLOUD_ID].models.len();

            apply_registry(&mut config, &data);
            assert_eq!(config.providers["regtestprov"].models.len(), merged_models);
            assert_eq!(
                config.providers[BITROUTER_CLOUD_ID].models.len(),
                cloud_models,
                "re-running must not mutate the cloud catalog"
            );
            assert_eq!(cloud_models, 0);
        });
    }
}
