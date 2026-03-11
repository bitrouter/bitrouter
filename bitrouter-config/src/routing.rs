use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use bitrouter_core::{
    errors::{BitrouterError, Result},
    routers::routing_table::{RouteEntry, RoutingTable, RoutingTarget},
};

use crate::config::{ApiProtocol, ModelConfig, ProviderConfig, RoutingStrategy};

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
/// Routes incoming model names to concrete provider targets using two strategies:
///
/// 1. **Direct routing**: `"provider:model_id"` routes directly to the named provider.
/// 2. **Model lookup**: Other names are looked up in the `models` map, which supports
///    prioritized failover and round-robin load balancing.
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
                let counter = self
                    .counters
                    .get(model_name)
                    .expect("counter must exist for every model");
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
        entries.sort_by(|a, b| a.model.cmp(&b.model));
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ApiProtocol, ModelEndpoint};

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
            },
        );
        let table = ConfigRoutingTable::new(test_providers(), models);
        let result = table.resolve("empty");
        assert!(result.is_err());
    }
}
