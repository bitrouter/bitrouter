use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use bitrouter_core::{
    errors::{BitrouterError, Result},
    routers::registry::{ModelEntry, ModelRegistry},
    routers::routing_table::{
        InputTokenPricing as CoreInputTokenPricing, ModelPricing as CoreModelPricing,
        OutputTokenPricing as CoreOutputTokenPricing, RouteEntry, RoutingTable, RoutingTarget,
    },
};

use crate::config::{
    ApiProtocol, ModelConfig, ModelInfo, ModelPricing, ProviderConfig, RoutingStrategy,
};

/// The provider name used as fallback when the user has no explicit `models:`
/// section configured.
const DEFAULT_PROVIDER: &str = "bitrouter";

/// A routing target with full resolution context including any per-endpoint overrides.
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub provider_name: String,
    pub model_id: String,
    /// Per-endpoint API key override (from the model endpoint config).
    pub api_key_override: Option<String>,
    /// Per-endpoint API base override.
    pub api_base_override: Option<String>,
}

/// Configuration-driven routing table.
///
/// Routes incoming model names to concrete provider targets using three strategies:
///
/// 1. **Direct routing**: `"provider:model_id"` routes directly to the named provider.
/// 2. **Model lookup**: Names are looked up in the `models` map, which supports
///    prioritised failover and round-robin load balancing.
/// 3. **Default provider fallback**: When no explicit `models` section is
///    configured and the default provider (`bitrouter`) exists, bare model
///    names are forwarded to that provider.
pub struct ConfigRoutingTable {
    providers: HashMap<String, ProviderConfig>,
    models: HashMap<String, ModelConfig>,
    /// Per-model round-robin counters for load balancing.
    counters: HashMap<String, AtomicUsize>,
}

impl ConfigRoutingTable {
    pub fn new(
        providers: HashMap<String, ProviderConfig>,
        models: HashMap<String, ModelConfig>,
    ) -> Self {
        let counters = models
            .keys()
            .map(|k| (k.clone(), AtomicUsize::new(0)))
            .collect();
        Self {
            providers,
            models,
            counters,
        }
    }

    /// Returns a reference to the resolved provider configurations.
    pub fn providers(&self) -> &HashMap<String, ProviderConfig> {
        &self.providers
    }

    /// Returns the model metadata for a given provider and model ID.
    ///
    /// Falls back to [`ModelInfo::default()`] for unknown providers or
    /// unconfigured models.
    pub fn model_info(&self, provider_name: &str, model_id: &str) -> ModelInfo {
        self.providers
            .get(provider_name)
            .and_then(|p| p.models.as_ref())
            .and_then(|models| models.get(model_id))
            .cloned()
            .unwrap_or_default()
    }

    /// Returns the token pricing for a given provider and model ID.
    ///
    /// Convenience wrapper around [`model_info`](Self::model_info) that
    /// returns only the pricing component. Falls back to
    /// [`ModelPricing::default()`] (all zeros) for unknown providers or
    /// unconfigured models.
    pub fn model_pricing(&self, provider_name: &str, model_id: &str) -> ModelPricing {
        self.model_info(provider_name, model_id).pricing
    }

    /// Resolves an incoming model name to a full target with any per-endpoint overrides.
    pub fn resolve(&self, incoming: &str) -> Result<ResolvedTarget> {
        // Strategy 1: "provider:model_id" → direct route if provider is known
        if let Some((prefix, suffix)) = incoming.split_once(':')
            && self.providers.contains_key(prefix)
        {
            return Ok(ResolvedTarget {
                provider_name: prefix.to_owned(),
                model_id: suffix.to_owned(),
                api_key_override: None,
                api_base_override: None,
            });
        }

        // Strategy 2: lookup in models section
        if let Some(model_config) = self.models.get(incoming) {
            return self.select_endpoint(incoming, model_config);
        }

        // Strategy 3: when no explicit models section is configured, fall back
        // to the default provider (if it exists in the provider set).
        if self.models.is_empty() && self.providers.contains_key(DEFAULT_PROVIDER) {
            return Ok(ResolvedTarget {
                provider_name: DEFAULT_PROVIDER.to_owned(),
                model_id: incoming.to_owned(),
                api_key_override: None,
                api_base_override: None,
            });
        }

        Err(BitrouterError::invalid_request(
            None,
            format!("no route found for model: {incoming}"),
            None,
        ))
    }

    fn select_endpoint(&self, model_name: &str, config: &ModelConfig) -> Result<ResolvedTarget> {
        if config.endpoints.is_empty() {
            return Err(BitrouterError::invalid_request(
                None,
                format!("model '{model_name}' has no configured endpoints"),
                None,
            ));
        }

        let endpoint = match config.strategy {
            RoutingStrategy::Priority => &config.endpoints[0],
            RoutingStrategy::LoadBalance => {
                let Some(counter) = self.counters.get(model_name) else {
                    return Err(BitrouterError::invalid_request(
                        None,
                        format!("load-balance counter missing for model '{model_name}'"),
                        None,
                    ));
                };
                let idx = counter.fetch_add(1, Ordering::Relaxed) % config.endpoints.len();
                &config.endpoints[idx]
            }
        };

        Ok(ResolvedTarget {
            provider_name: endpoint.provider.clone(),
            model_id: endpoint.model_id.clone(),
            api_key_override: endpoint.api_key.clone(),
            api_base_override: endpoint.api_base.clone(),
        })
    }
}

fn convert_pricing(pricing: &ModelPricing) -> CoreModelPricing {
    CoreModelPricing {
        input_tokens: CoreInputTokenPricing {
            no_cache: pricing.input_tokens.no_cache,
            cache_read: pricing.input_tokens.cache_read,
            cache_write: pricing.input_tokens.cache_write,
        },
        output_tokens: CoreOutputTokenPricing {
            text: pricing.output_tokens.text,
            reasoning: pricing.output_tokens.reasoning,
        },
    }
}

impl RoutingTable for ConfigRoutingTable {
    async fn route(&self, incoming_model_name: &str) -> Result<RoutingTarget> {
        let resolved = self.resolve(incoming_model_name)?;
        Ok(RoutingTarget {
            provider_name: resolved.provider_name,
            model_id: resolved.model_id,
        })
    }

    fn list_routes(&self) -> Vec<RouteEntry> {
        let mut entries = Vec::new();

        if self.models.is_empty() {
            // Fallback mode: surface the default provider's model catalog.
            if let Some(provider) = self.providers.get(DEFAULT_PROVIDER) {
                let protocol = provider
                    .api_protocol
                    .as_ref()
                    .map(|p| match p {
                        ApiProtocol::Openai => "openai",
                        ApiProtocol::Anthropic => "anthropic",
                        ApiProtocol::Google => "google",
                    })
                    .unwrap_or("openai")
                    .to_owned();
                if let Some(models) = &provider.models {
                    for model_id in models.keys() {
                        entries.push(RouteEntry {
                            model: model_id.clone(),
                            provider: DEFAULT_PROVIDER.to_owned(),
                            protocol: protocol.clone(),
                        });
                    }
                }
            }
        } else {
            for (model_name, model_config) in &self.models {
                if let Some(endpoint) = model_config.endpoints.first() {
                    let protocol = self
                        .providers
                        .get(&endpoint.provider)
                        .and_then(|p| p.api_protocol.as_ref())
                        .map(|p| match p {
                            ApiProtocol::Openai => "openai",
                            ApiProtocol::Anthropic => "anthropic",
                            ApiProtocol::Google => "google",
                        })
                        .unwrap_or("openai")
                        .to_owned();
                    entries.push(RouteEntry {
                        model: model_name.clone(),
                        provider: endpoint.provider.clone(),
                        protocol,
                    });
                }
            }
        }

        entries.sort_by(|a, b| a.model.cmp(&b.model));
        entries
    }
}

impl ModelRegistry for ConfigRoutingTable {
    fn list_models(&self) -> Vec<ModelEntry> {
        let mut entries: Vec<ModelEntry> = if self.models.is_empty() {
            // Fallback mode: surface the default provider's model catalog.
            self.providers
                .get(DEFAULT_PROVIDER)
                .and_then(|p| p.models.as_ref())
                .into_iter()
                .flat_map(|models| {
                    models.iter().map(|(model_id, info)| {
                        let pricing = convert_pricing(&info.pricing);
                        let pricing = if pricing.is_empty() {
                            None
                        } else {
                            Some(pricing)
                        };
                        ModelEntry {
                            id: model_id.clone(),
                            providers: vec![DEFAULT_PROVIDER.to_owned()],
                            name: info.name.clone(),
                            description: info.description.clone(),
                            max_input_tokens: info.max_input_tokens,
                            max_output_tokens: info.max_output_tokens,
                            input_modalities: info
                                .input_modalities
                                .iter()
                                .map(|m| m.to_string())
                                .collect(),
                            output_modalities: info
                                .output_modalities
                                .iter()
                                .map(|m| m.to_string())
                                .collect(),
                            pricing,
                        }
                    })
                })
                .collect()
        } else {
            self.models
                .iter()
                .map(|(model_name, model_config)| {
                    let providers: Vec<String> = model_config
                        .endpoints
                        .iter()
                        .map(|ep| ep.provider.clone())
                        .collect();
                    let pricing = convert_pricing(&model_config.pricing);
                    let pricing = if pricing.is_empty() {
                        None
                    } else {
                        Some(pricing)
                    };
                    ModelEntry {
                        id: model_name.clone(),
                        providers,
                        name: model_config.name.clone(),
                        description: None,
                        max_input_tokens: model_config.max_input_tokens,
                        max_output_tokens: model_config.max_output_tokens,
                        input_modalities: model_config
                            .input_modalities
                            .iter()
                            .map(|m| m.to_string())
                            .collect(),
                        output_modalities: model_config
                            .output_modalities
                            .iter()
                            .map(|m| m.to_string())
                            .collect(),
                        pricing,
                    }
                })
                .collect()
        };
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ApiProtocol, InputTokenPricing, Modality, ModelEndpoint, OutputTokenPricing,
    };

    fn test_providers() -> HashMap<String, ProviderConfig> {
        let mut p = HashMap::new();
        p.insert(
            "openai".into(),
            ProviderConfig {
                api_protocol: Some(ApiProtocol::Openai),
                api_base: Some("https://api.openai.com/v1".into()),
                ..Default::default()
            },
        );
        p.insert(
            "anthropic".into(),
            ProviderConfig {
                api_protocol: Some(ApiProtocol::Anthropic),
                api_base: Some("https://api.anthropic.com".into()),
                ..Default::default()
            },
        );
        p
    }

    #[test]
    fn direct_provider_routing() {
        let table = ConfigRoutingTable::new(test_providers(), HashMap::new());
        let target = table.resolve("openai:gpt-4o").unwrap();
        assert_eq!(target.provider_name, "openai");
        assert_eq!(target.model_id, "gpt-4o");
    }

    #[test]
    fn direct_provider_routing_with_slash_in_model() {
        let table = ConfigRoutingTable::new(test_providers(), HashMap::new());
        let target = table.resolve("openai:deepseek/deepseek-v3").unwrap();
        assert_eq!(target.provider_name, "openai");
        assert_eq!(target.model_id, "deepseek/deepseek-v3");
    }

    #[test]
    fn anthropic_direct_routing() {
        let table = ConfigRoutingTable::new(test_providers(), HashMap::new());
        let target = table.resolve("anthropic:claude-opus-4-6").unwrap();
        assert_eq!(target.provider_name, "anthropic");
        assert_eq!(target.model_id, "claude-opus-4-6");
    }

    #[test]
    fn unknown_provider_prefix_falls_through_to_models() {
        let mut models = HashMap::new();
        models.insert(
            "unknown:custom-model".into(),
            ModelConfig {
                strategy: RoutingStrategy::Priority,
                endpoints: vec![ModelEndpoint {
                    provider: "openai".into(),
                    model_id: "custom-model".into(),
                    api_key: None,
                    api_base: None,
                }],
                ..Default::default()
            },
        );
        let table = ConfigRoutingTable::new(test_providers(), models);
        let target = table.resolve("unknown:custom-model").unwrap();
        assert_eq!(target.provider_name, "openai");
        assert_eq!(target.model_id, "custom-model");
    }

    #[test]
    fn model_lookup_without_colon() {
        let mut models = HashMap::new();
        models.insert(
            "my-gpt4".into(),
            ModelConfig {
                strategy: RoutingStrategy::Priority,
                endpoints: vec![ModelEndpoint {
                    provider: "openai".into(),
                    model_id: "gpt-4o".into(),
                    api_key: Some("sk-override".into()),
                    api_base: None,
                }],
                ..Default::default()
            },
        );
        let table = ConfigRoutingTable::new(test_providers(), models);
        let target = table.resolve("my-gpt4").unwrap();
        assert_eq!(target.provider_name, "openai");
        assert_eq!(target.model_id, "gpt-4o");
        assert_eq!(target.api_key_override.as_deref(), Some("sk-override"));
    }

    #[test]
    fn slash_separator_does_not_match_provider() {
        let table = ConfigRoutingTable::new(test_providers(), HashMap::new());
        // "openai/gpt-4o" uses slash, not colon — should NOT match openai provider
        let result = table.resolve("openai/gpt-4o");
        assert!(result.is_err());
    }

    #[test]
    fn load_balance_round_robin() {
        let mut models = HashMap::new();
        models.insert(
            "balanced".into(),
            ModelConfig {
                strategy: RoutingStrategy::LoadBalance,
                endpoints: vec![
                    ModelEndpoint {
                        provider: "openai".into(),
                        model_id: "gpt-4o".into(),
                        api_key: Some("key-a".into()),
                        api_base: None,
                    },
                    ModelEndpoint {
                        provider: "openai".into(),
                        model_id: "gpt-4o".into(),
                        api_key: Some("key-b".into()),
                        api_base: None,
                    },
                ],
                ..Default::default()
            },
        );
        let table = ConfigRoutingTable::new(test_providers(), models);

        let t1 = table.resolve("balanced").unwrap();
        let t2 = table.resolve("balanced").unwrap();
        let t3 = table.resolve("balanced").unwrap();

        assert_eq!(t1.api_key_override.as_deref(), Some("key-a"));
        assert_eq!(t2.api_key_override.as_deref(), Some("key-b"));
        assert_eq!(t3.api_key_override.as_deref(), Some("key-a")); // wraps around
    }

    #[test]
    fn priority_always_picks_first() {
        let mut models = HashMap::new();
        models.insert(
            "primary".into(),
            ModelConfig {
                strategy: RoutingStrategy::Priority,
                endpoints: vec![
                    ModelEndpoint {
                        provider: "openai".into(),
                        model_id: "gpt-4o".into(),
                        api_key: Some("primary-key".into()),
                        api_base: None,
                    },
                    ModelEndpoint {
                        provider: "openai".into(),
                        model_id: "gpt-4o".into(),
                        api_key: Some("fallback-key".into()),
                        api_base: None,
                    },
                ],
                ..Default::default()
            },
        );
        let table = ConfigRoutingTable::new(test_providers(), models);

        for _ in 0..5 {
            let t = table.resolve("primary").unwrap();
            assert_eq!(t.api_key_override.as_deref(), Some("primary-key"));
        }
    }

    #[test]
    fn no_route_found() {
        let table = ConfigRoutingTable::new(test_providers(), HashMap::new());
        let result = table.resolve("nonexistent-model");
        assert!(result.is_err());
    }

    #[test]
    fn empty_endpoints_is_error() {
        let mut models = HashMap::new();
        models.insert(
            "empty".into(),
            ModelConfig {
                strategy: RoutingStrategy::Priority,
                endpoints: vec![],
                ..Default::default()
            },
        );
        let table = ConfigRoutingTable::new(test_providers(), models);
        let result = table.resolve("empty");
        assert!(result.is_err());
    }

    #[test]
    fn model_pricing_returns_configured_values() {
        let mut providers = test_providers();
        providers.get_mut("openai").unwrap().models = Some(HashMap::from([(
            "gpt-4o".into(),
            ModelInfo {
                pricing: ModelPricing {
                    input_tokens: InputTokenPricing {
                        no_cache: Some(2.50),
                        cache_read: Some(1.25),
                        cache_write: Some(2.50),
                    },
                    output_tokens: OutputTokenPricing {
                        text: Some(10.00),
                        reasoning: Some(10.00),
                    },
                },
                ..Default::default()
            },
        )]));
        let table = ConfigRoutingTable::new(providers, HashMap::new());

        let pricing = table.model_pricing("openai", "gpt-4o");
        assert_eq!(pricing.input_tokens.no_cache, Some(2.50));
        assert_eq!(pricing.input_tokens.cache_read, Some(1.25));
        assert_eq!(pricing.output_tokens.text, Some(10.00));
    }

    #[test]
    fn model_pricing_unknown_model_returns_defaults() {
        let table = ConfigRoutingTable::new(test_providers(), HashMap::new());
        let pricing = table.model_pricing("openai", "nonexistent");
        assert_eq!(pricing.input_tokens.no_cache, None);
        assert_eq!(pricing.output_tokens.text, None);
    }

    #[test]
    fn model_pricing_unknown_provider_returns_defaults() {
        let table = ConfigRoutingTable::new(test_providers(), HashMap::new());
        let pricing = table.model_pricing("unknown-provider", "gpt-4o");
        assert_eq!(pricing.input_tokens.no_cache, None);
    }

    #[test]
    fn model_info_returns_full_metadata() {
        let mut providers = test_providers();
        providers.get_mut("openai").unwrap().models = Some(HashMap::from([(
            "gpt-4o".into(),
            ModelInfo {
                name: Some("GPT-4o".into()),
                description: Some("Multimodal model".into()),
                max_input_tokens: Some(128000),
                max_output_tokens: Some(16384),
                input_modalities: vec![Modality::Text, Modality::Image],
                output_modalities: vec![Modality::Text],
                ..Default::default()
            },
        )]));
        let table = ConfigRoutingTable::new(providers, HashMap::new());

        let info = table.model_info("openai", "gpt-4o");
        assert_eq!(info.name.as_deref(), Some("GPT-4o"));
        assert_eq!(info.max_input_tokens, Some(128000));
        assert_eq!(info.max_output_tokens, Some(16384));
        assert_eq!(info.input_modalities, vec![Modality::Text, Modality::Image]);
    }

    #[test]
    fn model_info_unknown_returns_defaults() {
        let table = ConfigRoutingTable::new(test_providers(), HashMap::new());
        let info = table.model_info("openai", "nonexistent");
        assert!(info.name.is_none());
        assert!(info.max_input_tokens.is_none());
        assert!(info.input_modalities.is_empty());
    }

    // ── Default provider fallback tests ──────────────────────────────

    fn providers_with_bitrouter() -> HashMap<String, ProviderConfig> {
        let mut p = test_providers();
        p.insert(
            "bitrouter".into(),
            ProviderConfig {
                api_protocol: Some(ApiProtocol::Openai),
                api_base: Some("https://api.bitrouter.ai/v1".into()),
                models: Some(HashMap::from([
                    (
                        "openai/gpt-4o".into(),
                        ModelInfo {
                            name: Some("GPT-4o".into()),
                            max_input_tokens: Some(128000),
                            ..Default::default()
                        },
                    ),
                    (
                        "anthropic/claude-sonnet-4".into(),
                        ModelInfo {
                            name: Some("Claude Sonnet 4".into()),
                            max_input_tokens: Some(200000),
                            ..Default::default()
                        },
                    ),
                ])),
                ..Default::default()
            },
        );
        p
    }

    #[test]
    fn fallback_routes_bare_name_to_default_provider() {
        let table = ConfigRoutingTable::new(providers_with_bitrouter(), HashMap::new());
        let target = table.resolve("openai/gpt-4o").unwrap();
        assert_eq!(target.provider_name, "bitrouter");
        assert_eq!(target.model_id, "openai/gpt-4o");
    }

    #[test]
    fn fallback_routes_arbitrary_bare_name() {
        let table = ConfigRoutingTable::new(providers_with_bitrouter(), HashMap::new());
        let target = table.resolve("some-unknown-model").unwrap();
        assert_eq!(target.provider_name, "bitrouter");
        assert_eq!(target.model_id, "some-unknown-model");
    }

    #[test]
    fn fallback_does_not_fire_when_models_configured() {
        let mut models = HashMap::new();
        models.insert(
            "my-gpt4".into(),
            ModelConfig {
                strategy: RoutingStrategy::Priority,
                endpoints: vec![ModelEndpoint {
                    provider: "openai".into(),
                    model_id: "gpt-4o".into(),
                    api_key: None,
                    api_base: None,
                }],
                ..Default::default()
            },
        );
        let table = ConfigRoutingTable::new(providers_with_bitrouter(), models);
        // A name NOT in the models map should error (no fallback).
        let result = table.resolve("openai/gpt-4o");
        assert!(result.is_err());
    }

    #[test]
    fn fallback_not_active_without_default_provider() {
        // test_providers() has no "bitrouter" provider
        let table = ConfigRoutingTable::new(test_providers(), HashMap::new());
        let result = table.resolve("openai/gpt-4o");
        assert!(result.is_err());
    }

    #[test]
    fn direct_routing_takes_precedence_over_fallback() {
        let table = ConfigRoutingTable::new(providers_with_bitrouter(), HashMap::new());
        // With colon syntax, should route to openai directly, not bitrouter
        let target = table.resolve("openai:gpt-4o").unwrap();
        assert_eq!(target.provider_name, "openai");
        assert_eq!(target.model_id, "gpt-4o");
    }

    #[test]
    fn explicit_bitrouter_prefix_routes_directly() {
        let table = ConfigRoutingTable::new(providers_with_bitrouter(), HashMap::new());
        let target = table.resolve("bitrouter:openai/gpt-4o").unwrap();
        assert_eq!(target.provider_name, "bitrouter");
        assert_eq!(target.model_id, "openai/gpt-4o");
    }

    #[test]
    fn fallback_list_routes_surfaces_default_catalog() {
        let table = ConfigRoutingTable::new(providers_with_bitrouter(), HashMap::new());
        let routes = table.list_routes();
        assert_eq!(routes.len(), 2);
        assert!(routes.iter().all(|r| r.provider == "bitrouter"));
        assert!(routes.iter().any(|r| r.model == "openai/gpt-4o"));
        assert!(
            routes
                .iter()
                .any(|r| r.model == "anthropic/claude-sonnet-4")
        );
    }

    #[test]
    fn fallback_list_models_surfaces_default_catalog() {
        let table = ConfigRoutingTable::new(providers_with_bitrouter(), HashMap::new());
        let models = table.list_models();
        assert_eq!(models.len(), 2);
        assert!(models.iter().any(|m| m.id == "openai/gpt-4o"));
        assert!(models.iter().any(|m| m.id == "anthropic/claude-sonnet-4"));
        // Verify metadata is surfaced
        let gpt = models.iter().find(|m| m.id == "openai/gpt-4o").unwrap();
        assert_eq!(gpt.name.as_deref(), Some("GPT-4o"));
        assert_eq!(gpt.max_input_tokens, Some(128000));
        assert_eq!(gpt.providers, vec!["bitrouter"]);
    }

    #[test]
    fn no_fallback_list_routes_empty_without_default_provider() {
        let table = ConfigRoutingTable::new(test_providers(), HashMap::new());
        assert!(table.list_routes().is_empty());
    }

    #[test]
    fn no_fallback_list_models_empty_without_default_provider() {
        let table = ConfigRoutingTable::new(test_providers(), HashMap::new());
        assert!(table.list_models().is_empty());
    }
}
