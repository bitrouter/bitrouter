//! Merge provider-registry data into a parsed [`Config`].
//!
//! [`apply_registry`] is the bridge from "what the registry says exists" to
//! "what this bitrouter instance will route to". Its job is the subsystem's
//! whole purpose: make a canonical model id routable to a provider that serves
//! it. The rules (from the feature's principles):
//!
//! 1. Route a *canonical* model id to a provider that provides it.
//! 2. Only merge **BYOK** registry providers — never private (`byok: false`)
//!    ones (the pooled `bitrouter` provider among them).
//! 3. The built-in `bitrouter` provider is the hosted gateway and serves
//!    **every** canonical model.
//! 4. Providers carry a [`ProviderClass`]; the auto-cascade orders by it.
//! 5. A provider is activated **only if its credentials are present**.
//!
//! Precedence is conservative: the merge never overwrites a field the user set
//! in `bitrouter.yaml`, and for a provider that also has a compiled-in
//! [`ProviderEntry`](crate::ProviderEntry) it defers the endpoint/auth shape to
//! that built-in (filled by a follow-up [`apply_builtin_defaults`]).

use bitrouter_sdk::config::{
    Config, Pattern, PatternMap, PricingConfig, PricingTierConfig, ProviderClass, ProviderConfig,
    ProviderModel, RateLimit, RegistryConfig, env_lookup,
};
use bitrouter_sdk::language_model::types::{ApiProtocol, ProtocolList};

use crate::builtin;
use crate::registry::cache::DiskCache;
use crate::registry::fetch::fetch_registry;
use crate::registry::types::{
    Billing, RegistryData, RegistryPricing, RegistryProtocol, RegistryProvider, RegistryRateLimits,
};

/// The provider id of the hosted bitrouter gateway. The pooled registry entry
/// of the same name is `byok: false` and so is filtered out of the merge — the
/// gateway is served by the compiled-in [`ProviderEntry`](crate::ProviderEntry)
/// of this id instead, and serves the whole canonical list.
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
            tracing::warn!(error = %e, "provider-registry cache dir unresolved; fetching without cache");
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
                tracing::warn!(error = %e, "failed to write provider-registry cache");
            }
            Some(data)
        }
        Err(e) => {
            tracing::warn!(error = %e, "provider-registry fetch failed; using cached data if any");
            cache.and_then(|c| c.read_any().ok().flatten())
        }
    }
}

/// Merge `data` into `config`. No-op when `inherit_defaults` or
/// `registry.enabled` is false. Idempotent: re-running over an already-merged
/// config changes nothing (every write is guarded by an "is it empty / unset"
/// check).
pub fn apply_registry(config: &mut Config, data: &RegistryData) {
    if !config.inherit_defaults || !config.registry.enabled {
        return;
    }
    apply_cloud_all_canonical(config, data);
    for provider in &data.providers {
        // Principle #2: only BYOK, active registry providers. The pooled
        // `bitrouter` entry is `byok: false`, so it's filtered here even before
        // the id guard below.
        if !provider.is_active() || !provider.byok || provider.name == BITROUTER_CLOUD_ID {
            continue;
        }
        merge_provider(config, provider);
    }
}

/// Principle #3: the hosted gateway serves every canonical model. When the
/// built-in `bitrouter` provider is present (added by the env-var / sign-in
/// zero-config path or written by the user), give it one entry per canonical id
/// and stop it auto-discovering — the canonical list is authoritative. Presence
/// is the credential signal (the zero-config paths only add it when
/// credentialed); routing still gates on the provider's own `active` flag.
fn apply_cloud_all_canonical(config: &mut Config, data: &RegistryData) {
    let Some(cloud) = config.providers.get_mut(BITROUTER_CLOUD_ID) else {
        return;
    };
    if cloud.class.is_none() {
        cloud.class = Some(ProviderClass::BitrouterCloud);
    }
    if cloud.models.is_empty() && !data.canonical.is_empty() {
        cloud.models = data
            .canonical
            .iter()
            .map(|m| ProviderModel {
                id: m.id.clone(),
                // The gateway accepts canonical ids directly — no translation.
                provider_model_id: None,
                api_protocol: None,
                rate_limits: None,
                pricing: None,
            })
            .collect();
        // We filled the catalog from the canonical list; don't probe `/models`.
        cloud.auto_discover = false;
    }
}

/// Merge one BYOK registry provider into the config.
fn merge_provider(config: &mut Config, provider: &RegistryProvider) {
    let id = provider.name.as_str();
    let class = classify(provider);
    let has_builtin = builtin::find(id).is_some();

    if let Some(existing) = config.providers.get_mut(id) {
        // Already configured (user-written, zero-config, or a built-in fill).
        // Respect the user's fields; only fill what's unset. Never flip `active`
        // — an uncredentialed entry stays inactive (principle #5).
        if existing.class.is_none() {
            existing.class = Some(class);
        }
        if existing.models.is_empty() {
            existing.models = build_models(provider);
        }
        // For a registry-only provider the user listed bare, supply the
        // endpoint shape; a built-in's shape is left to `apply_builtin_defaults`.
        if !has_builtin {
            if existing.api_base.is_empty() {
                existing.api_base = provider.api_base.clone();
            }
            if existing.api_protocol.is_empty() {
                existing.api_protocol = build_protocol_map(provider);
            }
        }
        return;
    }

    // Not configured — activate only if a credential is present (principle #5).
    let Some(api_key) = env_lookup(&env_var_for(provider)).filter(|v| !v.is_empty()) else {
        return;
    };
    let mut entry = ProviderConfig {
        api_key,
        models: build_models(provider),
        class: Some(class),
        active: true,
        ..ProviderConfig::default()
    };
    if has_builtin {
        // A built-in exists: leave `api_base` / `api_protocol` empty so the
        // follow-up `apply_builtin_defaults` fills the authoritative shape
        // (and any OAuth/header auth applier keys off the provider's presence).
    } else {
        entry.api_base = provider.api_base.clone();
        entry.api_protocol = build_protocol_map(provider);
    }
    config.providers.insert(id.to_string(), entry);
}

/// Classify a registry provider into a routing-preference [`ProviderClass`].
/// Community resellers are third-party; first-party providers split by billing.
fn classify(provider: &RegistryProvider) -> ProviderClass {
    if provider.community {
        ProviderClass::ThirdPartyApi
    } else if provider.billing == Billing::Subscription {
        ProviderClass::FirstPartySubscription
    } else {
        ProviderClass::FirstPartyApi
    }
}

/// The env var a registry-only provider's BYOK key is read from: the built-in
/// entry's advertised var when one exists, else the convention
/// `{NAME}_API_KEY` (uppercased, hyphens → underscores).
fn env_var_for(provider: &RegistryProvider) -> String {
    builtin::find(&provider.name)
        .and_then(|e| e.auth.env_var())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{}_API_KEY", provider.name.to_uppercase().replace('-', "_")))
}

/// Map a registry protocol onto bitrouter's wire-protocol enum.
fn map_protocol(p: RegistryProtocol) -> ApiProtocol {
    match p {
        RegistryProtocol::Openai => ApiProtocol::ChatCompletions,
        RegistryProtocol::Anthropic => ApiProtocol::Messages,
        RegistryProtocol::Google => ApiProtocol::GenerateContent,
        RegistryProtocol::Responses => ApiProtocol::Responses,
    }
}

/// Build the provider-level `api_protocol` pattern map from the registry's
/// pattern entries. Each registry entry maps one glob to a single protocol.
fn build_protocol_map(provider: &RegistryProvider) -> PatternMap<ProtocolList> {
    let mut map = PatternMap::new();
    for entry in &provider.api_protocol {
        for (pattern, protocol) in entry {
            map.push(
                Pattern::parse(pattern),
                ProtocolList(vec![map_protocol(*protocol)]),
            );
        }
    }
    map
}

/// Translate the registry's per-model entries into `ProviderModel`s — the
/// canonical id is the match key, `provider_model_id` the upstream dispatch id.
fn build_models(provider: &RegistryProvider) -> Vec<ProviderModel> {
    provider
        .models
        .iter()
        .map(|m| ProviderModel {
            id: m.id.clone(),
            provider_model_id: Some(m.provider_model_id.clone()),
            api_protocol: m.api_protocol.map(|p| ProtocolList(vec![map_protocol(p)])),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::types::{
        CanonicalModel, InputTokenPricing, OutputTokenPricing, RegistryModel,
    };

    fn provider(name: &str) -> RegistryProvider {
        RegistryProvider {
            name: name.to_string(),
            api_base: format!("https://{name}.example/v1"),
            api_protocol: vec![
                [("*".to_string(), RegistryProtocol::Openai)]
                    .into_iter()
                    .collect(),
            ],
            models: vec![RegistryModel {
                id: "deepseek/deepseek-v3.2".to_string(),
                provider_model_id: "deepseek-v3.2".to_string(),
                api_protocol: None,
                pricing: Some(RegistryPricing {
                    input_tokens: Some(InputTokenPricing {
                        no_cache: Some(0.27),
                    }),
                    output_tokens: Some(OutputTokenPricing { text: Some(0.41) }),
                    context_tiers: Vec::new(),
                }),
                rate_limits: None,
            }],
            rate_limits: Vec::new(),
            status: "active".to_string(),
            community: false,
            byok: true,
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
        let _g = LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
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
            let pricing = model.pricing.as_ref().expect("pricing mapped");
            assert_eq!(pricing.input_micro_usd_per_token, 0.27);
            assert_eq!(pricing.output_micro_usd_per_token, 0.41);
        });
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
    fn byok_false_provider_is_never_merged() {
        with_env("PRIVATEPROV_API_KEY", Some("sk-test"), || {
            let mut config = Config::default();
            let mut p = provider("privateprov");
            p.byok = false;
            apply_registry(&mut config, &data_with(vec![p], vec![]));
            assert!(
                !config.providers.contains_key("privateprov"),
                "byok=false ⇒ never merged, even with a credential set"
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
    fn bitrouter_cloud_serves_every_canonical_model() {
        let mut config = Config::default();
        // The hosted gateway is present (as the env/sign-in path would add it).
        config
            .providers
            .insert(BITROUTER_CLOUD_ID.to_string(), ProviderConfig::default());
        let data = data_with(
            vec![],
            vec!["anthropic/claude-sonnet-4.6", "deepseek/deepseek-v3.2"],
        );
        apply_registry(&mut config, &data);
        let cloud = &config.providers[BITROUTER_CLOUD_ID];
        assert_eq!(cloud.class, Some(ProviderClass::BitrouterCloud));
        assert!(
            !cloud.auto_discover,
            "filled from canonical, no /models probe"
        );
        let ids: Vec<&str> = cloud.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["anthropic/claude-sonnet-4.6", "deepseek/deepseek-v3.2"]
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
            let mut config = Config::default();
            config.inherit_defaults = false;
            apply_registry(
                &mut config,
                &data_with(vec![provider("regtestprov")], vec![]),
            );
            assert!(config.providers.is_empty());
        });
    }
}
