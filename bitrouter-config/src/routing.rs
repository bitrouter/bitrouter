use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use bitrouter_core::{
    errors::{BitrouterError, Result},
    routers::content::RouteContext,
    routers::registry::{
        AgentCapabilityFlags, AgentEntry, AgentEntryStatus, AgentRegistry, ModelEntry,
        ModelRegistry, ToolEntry, ToolRegistry,
    },
    routers::routing_table::{
        ApiProtocol, ModelPricing, RouteEntry, RoutingTable, RoutingTarget, strip_ansi_escapes,
    },
    tools::definition::ToolDefinition,
};

use crate::config::{
    AgentConfig, ModelConfig, ModelInfo, ProviderConfig, RoutingRuleConfig, RoutingStrategy,
    ToolConfig,
};
use crate::content_routing::ContentRoutingRules;

/// The provider name used as fallback when the user has no explicit `models:`
/// section configured.
const DEFAULT_PROVIDER: &str = "bitrouter";

/// A routing target with full resolution context including any per-endpoint overrides.
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub provider_name: String,
    /// Upstream service identifier: model ID for language models, tool ID for tools.
    pub service_id: String,
    /// The resolved API protocol for this endpoint.
    ///
    /// Resolution order: endpoint override > provider default.
    pub api_protocol: ApiProtocol,
    /// Per-endpoint API key override.
    pub api_key_override: Option<String>,
    /// Per-endpoint API base override.
    pub api_base_override: Option<String>,
}

/// Resolves the API protocol for a given provider, with an optional
/// per-endpoint override.
fn resolve_protocol(
    providers: &HashMap<String, ProviderConfig>,
    provider_name: &str,
    endpoint_override: Option<ApiProtocol>,
) -> Result<ApiProtocol> {
    if let Some(proto) = endpoint_override {
        return Ok(proto);
    }
    providers
        .get(provider_name)
        .and_then(|p| p.api_protocol)
        .ok_or_else(|| {
            BitrouterError::invalid_request(
                Some(provider_name),
                format!("provider '{provider_name}' has no api_protocol configured"),
                None,
            )
        })
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
    /// Compiled content-based routing rules (empty when no `routing:` config).
    content_rules: ContentRoutingRules,
}

impl ConfigRoutingTable {
    pub fn new(
        providers: HashMap<String, ProviderConfig>,
        models: HashMap<String, ModelConfig>,
    ) -> Self {
        Self::with_routing(providers, models, &HashMap::new())
    }

    /// Creates a routing table with content-based auto-routing rules.
    pub fn with_routing(
        providers: HashMap<String, ProviderConfig>,
        models: HashMap<String, ModelConfig>,
        routing: &HashMap<String, RoutingRuleConfig>,
    ) -> Self {
        let counters = models
            .keys()
            .map(|k| (k.clone(), AtomicUsize::new(0)))
            .collect();
        let content_rules = ContentRoutingRules::compile(routing);
        Self {
            providers,
            models,
            counters,
            content_rules,
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
            let api_protocol = resolve_protocol(&self.providers, prefix, None)?;
            return Ok(ResolvedTarget {
                provider_name: prefix.to_owned(),
                service_id: strip_ansi_escapes(suffix),
                api_protocol,
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
            let api_protocol = resolve_protocol(&self.providers, DEFAULT_PROVIDER, None)?;
            return Ok(ResolvedTarget {
                provider_name: DEFAULT_PROVIDER.to_owned(),
                service_id: strip_ansi_escapes(incoming),
                api_protocol,
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

        let api_protocol =
            resolve_protocol(&self.providers, &endpoint.provider, endpoint.api_protocol)?;

        Ok(ResolvedTarget {
            provider_name: endpoint.provider.clone(),
            service_id: strip_ansi_escapes(&endpoint.service_id),
            api_protocol,
            api_key_override: endpoint.api_key.clone(),
            api_base_override: endpoint.api_base.clone(),
        })
    }
}

impl RoutingTable for ConfigRoutingTable {
    async fn route(&self, incoming_name: &str, context: &RouteContext) -> Result<RoutingTarget> {
        // Content-based auto-routing: if the requested model name is a trigger
        // and the caller supplied non-empty context, classify and resolve.
        if !context.is_empty()
            && self.content_rules.is_trigger(incoming_name)
            && let Some(resolved_name) = self.content_rules.resolve(incoming_name, context)
        {
            // Delegate the resolved name through normal routing with empty
            // context to prevent recursive auto-routing.
            let resolved = self.resolve(&resolved_name)?;
            return Ok(RoutingTarget {
                provider_name: resolved.provider_name,
                service_id: resolved.service_id,
                api_protocol: resolved.api_protocol,
            });
        }

        let resolved = self.resolve(incoming_name)?;
        Ok(RoutingTarget {
            provider_name: resolved.provider_name,
            service_id: resolved.service_id,
            api_protocol: resolved.api_protocol,
        })
    }

    fn list_routes(&self) -> Vec<RouteEntry> {
        let mut entries = Vec::new();

        if self.models.is_empty() {
            // Fallback mode: surface the default provider's model catalog.
            if let Some(provider) = self.providers.get(DEFAULT_PROVIDER) {
                let protocol = provider.api_protocol.unwrap_or(ApiProtocol::Openai);
                if let Some(models) = &provider.models {
                    for model_id in models.keys() {
                        entries.push(RouteEntry {
                            name: model_id.clone(),
                            provider: DEFAULT_PROVIDER.to_owned(),
                            protocol,
                        });
                    }
                }
            }
        } else {
            for (model_name, model_config) in &self.models {
                if let Some(endpoint) = model_config.endpoints.first() {
                    let protocol = endpoint
                        .api_protocol
                        .or_else(|| {
                            self.providers
                                .get(&endpoint.provider)
                                .and_then(|p| p.api_protocol)
                        })
                        .unwrap_or(ApiProtocol::Openai);
                    entries.push(RouteEntry {
                        name: model_name.clone(),
                        provider: endpoint.provider.clone(),
                        protocol,
                    });
                }
            }
        }

        entries.sort_by(|a, b| a.name.cmp(&b.name));
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
                        let pricing = info.pricing.clone();
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
                    let pricing = model_config.pricing.clone();
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

// ── Tool routing table ──────────────────────────────────────────────

/// Configuration-driven tool routing table.
///
/// Routes incoming tool names to concrete provider targets using two strategies:
///
/// 1. **Direct routing**: `"provider:tool_id"` routes directly to the named provider.
/// 2. **Tool lookup**: Names are looked up in the `tools` map, which supports
///    prioritised failover and round-robin load balancing.
///
/// Unlike model routing, there is no default-provider fallback for tools.
pub struct ConfigToolRoutingTable {
    providers: HashMap<String, ProviderConfig>,
    tools: HashMap<String, ToolConfig>,
    /// Per-tool round-robin counters for load balancing.
    counters: HashMap<String, AtomicUsize>,
}

impl ConfigToolRoutingTable {
    pub fn new(
        providers: HashMap<String, ProviderConfig>,
        tools: HashMap<String, ToolConfig>,
    ) -> Self {
        let counters = tools
            .keys()
            .map(|k| (k.clone(), AtomicUsize::new(0)))
            .collect();
        Self {
            providers,
            tools,
            counters,
        }
    }

    /// Returns a reference to the resolved provider configurations.
    pub fn providers(&self) -> &HashMap<String, ProviderConfig> {
        &self.providers
    }

    /// Returns a reference to the tool configurations.
    pub fn tools(&self) -> &HashMap<String, ToolConfig> {
        &self.tools
    }

    /// Groups providers by their [`ApiProtocol`], considering only providers
    /// that are actually referenced by at least one tool endpoint.
    ///
    /// Providers without a resolvable `api_protocol` are skipped with a
    /// warning log.
    pub fn providers_by_protocol(&self) -> HashMap<ApiProtocol, Vec<(String, ProviderConfig)>> {
        // Track which (provider_name, protocol) pairs have already been added
        // so we don't insert duplicates.
        let mut seen: std::collections::HashSet<(String, ApiProtocol)> =
            std::collections::HashSet::new();
        let mut map: HashMap<ApiProtocol, Vec<(String, ProviderConfig)>> = HashMap::new();

        for tool_config in self.tools.values() {
            for endpoint in &tool_config.endpoints {
                let Some(provider) = self.providers.get(&endpoint.provider) else {
                    eprintln!(
                        "warning: tool endpoint references unknown provider '{}' — skipping",
                        endpoint.provider
                    );
                    continue;
                };

                // Resolve the effective protocol: per-endpoint override or provider default.
                let protocol = endpoint.api_protocol.or(provider.api_protocol);
                let Some(protocol) = protocol else {
                    eprintln!(
                        "warning: provider '{}' has no api_protocol configured — skipping",
                        endpoint.provider
                    );
                    continue;
                };

                if !seen.insert((endpoint.provider.clone(), protocol)) {
                    continue;
                }

                // Build a provider config with per-endpoint overrides applied.
                let mut config = provider.clone();
                if let Some(ref base) = endpoint.api_base {
                    config.api_base = Some(base.clone());
                }
                if let Some(ref key) = endpoint.api_key {
                    config.api_key = Some(key.clone());
                }
                config.api_protocol = Some(protocol);

                map.entry(protocol)
                    .or_default()
                    .push((endpoint.provider.clone(), config));
            }
        }
        map
    }

    /// Returns the pricing configuration for a tool, if any.
    pub fn tool_pricing(&self, tool_name: &str) -> Option<&bitrouter_core::pricing::FlatPricing> {
        self.tools.get(tool_name)?.pricing.as_ref()
    }

    /// Resolves an incoming tool name to a full target with any per-endpoint overrides.
    pub fn resolve(&self, incoming: &str) -> Result<ResolvedTarget> {
        // Strategy 1: "provider:tool_id" → direct route if provider is known
        if let Some((prefix, suffix)) = incoming.split_once(':')
            && self.providers.contains_key(prefix)
        {
            let api_protocol = resolve_protocol(&self.providers, prefix, None)?;
            return Ok(ResolvedTarget {
                provider_name: prefix.to_owned(),
                service_id: suffix.to_owned(),
                api_protocol,
                api_key_override: None,
                api_base_override: None,
            });
        }

        // Strategy 2: lookup in tools section by bare name
        if let Some(tool_config) = self.tools.get(incoming) {
            return self.select_endpoint(incoming, tool_config);
        }

        // Strategy 3: "provider/service_id" → namespaced format from MCP wire.
        // Searches config tools for a matching endpoint.
        if let Some((provider, service_id)) = incoming.split_once('/') {
            for tool_config in self.tools.values() {
                if let Some(ep) = tool_config
                    .endpoints
                    .iter()
                    .find(|ep| ep.provider == provider && ep.service_id == service_id)
                {
                    let api_protocol =
                        resolve_protocol(&self.providers, &ep.provider, ep.api_protocol)?;
                    return Ok(ResolvedTarget {
                        provider_name: ep.provider.clone(),
                        service_id: strip_ansi_escapes(&ep.service_id),
                        api_protocol,
                        api_key_override: ep.api_key.clone(),
                        api_base_override: ep.api_base.clone(),
                    });
                }
            }
        }

        Err(BitrouterError::invalid_request(
            None,
            format!("no route found for tool: {incoming}"),
            None,
        ))
    }

    fn select_endpoint(&self, tool_name: &str, config: &ToolConfig) -> Result<ResolvedTarget> {
        if config.endpoints.is_empty() {
            return Err(BitrouterError::invalid_request(
                None,
                format!("tool '{tool_name}' has no configured endpoints"),
                None,
            ));
        }

        let endpoint = match config.strategy {
            RoutingStrategy::Priority => &config.endpoints[0],
            RoutingStrategy::LoadBalance => {
                let Some(counter) = self.counters.get(tool_name) else {
                    return Err(BitrouterError::invalid_request(
                        None,
                        format!("load-balance counter missing for tool '{tool_name}'"),
                        None,
                    ));
                };
                let idx = counter.fetch_add(1, Ordering::Relaxed) % config.endpoints.len();
                &config.endpoints[idx]
            }
        };

        let api_protocol =
            resolve_protocol(&self.providers, &endpoint.provider, endpoint.api_protocol)?;

        Ok(ResolvedTarget {
            provider_name: endpoint.provider.clone(),
            service_id: strip_ansi_escapes(&endpoint.service_id),
            api_protocol,
            api_key_override: endpoint.api_key.clone(),
            api_base_override: endpoint.api_base.clone(),
        })
    }
}

impl RoutingTable for ConfigToolRoutingTable {
    async fn route(&self, incoming_name: &str, _context: &RouteContext) -> Result<RoutingTarget> {
        let resolved = self.resolve(incoming_name)?;
        Ok(RoutingTarget {
            provider_name: resolved.provider_name,
            service_id: resolved.service_id,
            api_protocol: resolved.api_protocol,
        })
    }

    fn list_routes(&self) -> Vec<RouteEntry> {
        let mut entries: Vec<RouteEntry> = self
            .tools
            .iter()
            .filter_map(|(tool_name, tool_config)| {
                let endpoint = tool_config.endpoints.first()?;
                let protocol = endpoint
                    .api_protocol
                    .or_else(|| {
                        self.providers
                            .get(&endpoint.provider)
                            .and_then(|p| p.api_protocol)
                    })
                    .unwrap_or(ApiProtocol::Mcp);
                Some(RouteEntry {
                    name: tool_name.clone(),
                    provider: endpoint.provider.clone(),
                    protocol,
                })
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }
}

impl ToolRegistry for ConfigToolRoutingTable {
    async fn list_tools(&self) -> Vec<ToolEntry> {
        let mut entries: Vec<ToolEntry> = self
            .tools
            .iter()
            .filter_map(|(tool_name, config)| {
                let ep = config.endpoints.first()?;
                let input_schema = config
                    .input_schema
                    .as_ref()
                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                Some(ToolEntry {
                    id: format!("{}/{}", ep.provider, ep.service_id),
                    provider: ep.provider.clone(),
                    definition: ToolDefinition {
                        name: tool_name.clone(),
                        description: config.description.clone(),
                        input_schema,
                        annotations: None,
                        input_examples: Vec::new(),
                    },
                })
            })
            .collect();
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        entries
    }
}

// ── Agent registry ──────────────────────────────────────────────

/// Config-driven agent registry that implements [`AgentRegistry`] from
/// the `agents:` configuration section.
///
/// Parallel to [`ConfigRoutingTable`] for models and
/// [`ConfigToolRoutingTable`] for tools. This is a read-only discovery
/// registry — agent sessions are managed by the runtime, not the config layer.
pub struct ConfigAgentRegistry {
    agents: HashMap<String, AgentConfig>,
}

impl ConfigAgentRegistry {
    /// Create a new registry from the agents configuration map.
    pub fn new(agents: HashMap<String, AgentConfig>) -> Self {
        Self { agents }
    }
}

impl AgentRegistry for ConfigAgentRegistry {
    async fn list_agents(&self) -> Vec<AgentEntry> {
        let mut entries: Vec<AgentEntry> = self
            .agents
            .iter()
            .map(|(name, config)| {
                let status = if config.enabled {
                    AgentEntryStatus::Idle
                } else {
                    AgentEntryStatus::Unavailable
                };
                AgentEntry {
                    name: name.clone(),
                    protocol: config.protocol.to_string(),
                    description: None,
                    capabilities: AgentCapabilityFlags::default(),
                    status,
                }
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_core::routers::routing_table::ApiProtocol;

    use crate::config::{Endpoint, InputTokenPricing, Modality, OutputTokenPricing};

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
        assert_eq!(target.service_id, "gpt-4o");
    }

    #[test]
    fn direct_provider_routing_with_slash_in_model() {
        let table = ConfigRoutingTable::new(test_providers(), HashMap::new());
        let target = table.resolve("openai:deepseek/deepseek-v3").unwrap();
        assert_eq!(target.provider_name, "openai");
        assert_eq!(target.service_id, "deepseek/deepseek-v3");
    }

    #[test]
    fn anthropic_direct_routing() {
        let table = ConfigRoutingTable::new(test_providers(), HashMap::new());
        let target = table.resolve("anthropic:claude-opus-4-6").unwrap();
        assert_eq!(target.provider_name, "anthropic");
        assert_eq!(target.service_id, "claude-opus-4-6");
    }

    #[test]
    fn unknown_provider_prefix_falls_through_to_models() {
        let mut models = HashMap::new();
        models.insert(
            "unknown:custom-model".into(),
            ModelConfig {
                strategy: RoutingStrategy::Priority,
                endpoints: vec![Endpoint {
                    provider: "openai".into(),
                    service_id: "custom-model".into(),
                    api_protocol: None,
                    api_key: None,
                    api_base: None,
                }],
                ..Default::default()
            },
        );
        let table = ConfigRoutingTable::new(test_providers(), models);
        let target = table.resolve("unknown:custom-model").unwrap();
        assert_eq!(target.provider_name, "openai");
        assert_eq!(target.service_id, "custom-model");
    }

    #[test]
    fn model_lookup_without_colon() {
        let mut models = HashMap::new();
        models.insert(
            "my-gpt4".into(),
            ModelConfig {
                strategy: RoutingStrategy::Priority,
                endpoints: vec![Endpoint {
                    provider: "openai".into(),
                    service_id: "gpt-4o".into(),
                    api_protocol: None,
                    api_key: Some("sk-override".into()),
                    api_base: None,
                }],
                ..Default::default()
            },
        );
        let table = ConfigRoutingTable::new(test_providers(), models);
        let target = table.resolve("my-gpt4").unwrap();
        assert_eq!(target.provider_name, "openai");
        assert_eq!(target.service_id, "gpt-4o");
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
                    Endpoint {
                        provider: "openai".into(),
                        service_id: "gpt-4o".into(),
                        api_protocol: None,
                        api_key: Some("key-a".into()),
                        api_base: None,
                    },
                    Endpoint {
                        provider: "openai".into(),
                        service_id: "gpt-4o".into(),
                        api_protocol: None,
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
                    Endpoint {
                        provider: "openai".into(),
                        service_id: "gpt-4o".into(),
                        api_protocol: None,
                        api_key: Some("primary-key".into()),
                        api_base: None,
                    },
                    Endpoint {
                        provider: "openai".into(),
                        service_id: "gpt-4o".into(),
                        api_protocol: None,
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
        assert_eq!(target.service_id, "openai/gpt-4o");
    }

    #[test]
    fn fallback_routes_arbitrary_bare_name() {
        let table = ConfigRoutingTable::new(providers_with_bitrouter(), HashMap::new());
        let target = table.resolve("some-unknown-model").unwrap();
        assert_eq!(target.provider_name, "bitrouter");
        assert_eq!(target.service_id, "some-unknown-model");
    }

    #[test]
    fn fallback_does_not_fire_when_models_configured() {
        let mut models = HashMap::new();
        models.insert(
            "my-gpt4".into(),
            ModelConfig {
                strategy: RoutingStrategy::Priority,
                endpoints: vec![Endpoint {
                    provider: "openai".into(),
                    service_id: "gpt-4o".into(),
                    api_protocol: None,
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
        assert_eq!(target.service_id, "gpt-4o");
    }

    #[test]
    fn explicit_bitrouter_prefix_routes_directly() {
        let table = ConfigRoutingTable::new(providers_with_bitrouter(), HashMap::new());
        let target = table.resolve("bitrouter:openai/gpt-4o").unwrap();
        assert_eq!(target.provider_name, "bitrouter");
        assert_eq!(target.service_id, "openai/gpt-4o");
    }

    #[test]
    fn fallback_list_routes_surfaces_default_catalog() {
        let table = ConfigRoutingTable::new(providers_with_bitrouter(), HashMap::new());
        let routes = table.list_routes();
        assert_eq!(routes.len(), 2);
        assert!(routes.iter().all(|r| r.provider == "bitrouter"));
        assert!(routes.iter().any(|r| r.name == "openai/gpt-4o"));
        assert!(routes.iter().any(|r| r.name == "anthropic/claude-sonnet-4"));
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

    // ── Auto-routing integration tests ─────────────────────────────

    #[tokio::test]
    async fn auto_route_coding_signal_resolves() {
        use crate::config::RoutingRuleConfig;
        let mut models = HashMap::new();
        models.insert(
            "code-model".to_owned(),
            ModelConfig {
                endpoints: vec![crate::config::Endpoint {
                    provider: "openai".into(),
                    service_id: "gpt-4o".into(),
                    api_protocol: None,
                    api_key: None,
                    api_base: None,
                }],
                ..Default::default()
            },
        );
        models.insert(
            "general".to_owned(),
            ModelConfig {
                endpoints: vec![crate::config::Endpoint {
                    provider: "anthropic".into(),
                    service_id: "claude-sonnet".into(),
                    api_protocol: None,
                    api_key: None,
                    api_base: None,
                }],
                ..Default::default()
            },
        );

        let mut routing = HashMap::new();
        routing.insert(
            "auto".to_owned(),
            RoutingRuleConfig {
                inherit_defaults: true,
                models: HashMap::from([
                    ("coding".into(), "code-model".into()),
                    ("default".into(), "general".into()),
                ]),
                ..Default::default()
            },
        );

        let table = ConfigRoutingTable::with_routing(test_providers(), models, &routing);

        // Request with coding content → coding model
        let ctx = RouteContext {
            text: "help me debug this function and fix the compile error".into(),
            char_count: 52,
            turn_count: 1,
            ..Default::default()
        };
        let target = table.route("auto", &ctx).await.unwrap();
        assert_eq!(target.provider_name, "openai");
        assert_eq!(target.service_id, "gpt-4o");
    }

    #[tokio::test]
    async fn auto_route_empty_context_falls_through() {
        use crate::config::RoutingRuleConfig;
        let mut models = HashMap::new();
        models.insert(
            "auto".to_owned(),
            ModelConfig {
                endpoints: vec![crate::config::Endpoint {
                    provider: "openai".into(),
                    service_id: "gpt-4o".into(),
                    api_protocol: None,
                    api_key: None,
                    api_base: None,
                }],
                ..Default::default()
            },
        );

        let mut routing = HashMap::new();
        routing.insert(
            "auto".to_owned(),
            RoutingRuleConfig {
                inherit_defaults: true,
                models: HashMap::from([("default".into(), "general".into())]),
                ..Default::default()
            },
        );

        let table = ConfigRoutingTable::with_routing(test_providers(), models, &routing);

        // Empty context → skip auto-routing, use normal model lookup for "auto"
        let target = table.route("auto", &RouteContext::default()).await.unwrap();
        assert_eq!(target.provider_name, "openai");
        assert_eq!(target.service_id, "gpt-4o");
    }

    #[tokio::test]
    async fn auto_route_non_trigger_passes_through() {
        use crate::config::RoutingRuleConfig;
        let mut models = HashMap::new();
        models.insert(
            "my-model".to_owned(),
            ModelConfig {
                endpoints: vec![crate::config::Endpoint {
                    provider: "anthropic".into(),
                    service_id: "claude-sonnet".into(),
                    api_protocol: None,
                    api_key: None,
                    api_base: None,
                }],
                ..Default::default()
            },
        );

        let mut routing = HashMap::new();
        routing.insert(
            "auto".to_owned(),
            RoutingRuleConfig {
                inherit_defaults: true,
                models: HashMap::from([("default".into(), "my-model".into())]),
                ..Default::default()
            },
        );

        let table = ConfigRoutingTable::with_routing(test_providers(), models, &routing);

        // "my-model" is not a trigger → normal routing
        let ctx = RouteContext {
            text: "help me code".into(),
            char_count: 12,
            turn_count: 1,
            ..Default::default()
        };
        let target = table.route("my-model", &ctx).await.unwrap();
        assert_eq!(target.provider_name, "anthropic");
        assert_eq!(target.service_id, "claude-sonnet");
    }

    // ── ConfigToolRoutingTable tests ────────────────────────────────

    fn tool_providers() -> HashMap<String, ProviderConfig> {
        let mut p = HashMap::new();
        p.insert(
            "github-mcp".into(),
            ProviderConfig {
                api_protocol: Some(ApiProtocol::Mcp),
                api_base: Some("https://api.githubcopilot.com/mcp".into()),
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
    fn tool_direct_provider_routing() {
        let table = ConfigToolRoutingTable::new(tool_providers(), HashMap::new());
        let target = table.resolve("github-mcp:create_issue").unwrap();
        assert_eq!(target.provider_name, "github-mcp");
        assert_eq!(target.service_id, "create_issue");
        assert_eq!(target.api_protocol, ApiProtocol::Mcp);
    }

    #[test]
    fn tool_slash_namespaced_routing() {
        let mut tools = HashMap::new();
        tools.insert(
            "create_issue".into(),
            ToolConfig {
                strategy: RoutingStrategy::Priority,
                endpoints: vec![Endpoint {
                    provider: "github-mcp".into(),
                    service_id: "create_issue".into(),
                    api_protocol: None,
                    api_key: None,
                    api_base: None,
                }],
                ..Default::default()
            },
        );
        let table = ConfigToolRoutingTable::new(tool_providers(), tools);
        // Slash-namespaced format used by MCP wire protocol.
        let target = table.resolve("github-mcp/create_issue").ok();
        assert!(target.is_some());
        let target = target.as_ref();
        assert_eq!(target.map(|t| t.provider_name.as_str()), Some("github-mcp"));
        assert_eq!(target.map(|t| t.service_id.as_str()), Some("create_issue"));
        assert_eq!(target.map(|t| t.api_protocol), Some(ApiProtocol::Mcp));
    }

    #[test]
    fn tool_slash_no_match_when_no_config() {
        // "github-mcp/unknown" should fail when no matching endpoint exists.
        let table = ConfigToolRoutingTable::new(tool_providers(), HashMap::new());
        assert!(table.resolve("github-mcp/unknown").is_err());
    }

    #[test]
    fn tool_lookup() {
        let mut tools = HashMap::new();
        tools.insert(
            "create_issue".into(),
            ToolConfig {
                strategy: RoutingStrategy::Priority,
                endpoints: vec![Endpoint {
                    provider: "github-mcp".into(),
                    service_id: "create_issue".into(),
                    api_protocol: None,
                    api_key: None,
                    api_base: None,
                }],
                ..Default::default()
            },
        );
        let table = ConfigToolRoutingTable::new(tool_providers(), tools);
        let target = table.resolve("create_issue").unwrap();
        assert_eq!(target.provider_name, "github-mcp");
        assert_eq!(target.service_id, "create_issue");
        assert_eq!(target.api_protocol, ApiProtocol::Mcp);
    }

    #[test]
    fn tool_endpoint_protocol_override() {
        let mut tools = HashMap::new();
        tools.insert(
            "web_search".into(),
            ToolConfig {
                endpoints: vec![Endpoint {
                    provider: "anthropic".into(),
                    service_id: "web_search".into(),
                    api_protocol: Some(ApiProtocol::Mcp),
                    api_key: None,
                    api_base: Some("https://mcp.anthropic.com".into()),
                }],
                ..Default::default()
            },
        );
        let table = ConfigToolRoutingTable::new(tool_providers(), tools);
        let target = table.resolve("web_search").unwrap();
        assert_eq!(target.provider_name, "anthropic");
        assert_eq!(target.api_protocol, ApiProtocol::Mcp);
        assert_eq!(
            target.api_base_override.as_deref(),
            Some("https://mcp.anthropic.com")
        );
    }

    #[test]
    fn tool_no_route_found() {
        let table = ConfigToolRoutingTable::new(tool_providers(), HashMap::new());
        let result = table.resolve("nonexistent-tool");
        assert!(result.is_err());
    }

    #[test]
    fn tool_no_fallback_for_bare_names() {
        // Unlike models, tools don't have a default provider fallback
        let table = ConfigToolRoutingTable::new(tool_providers(), HashMap::new());
        let result = table.resolve("some-tool");
        assert!(result.is_err());
    }

    #[test]
    fn tool_load_balance_round_robin() {
        let mut tools = HashMap::new();
        tools.insert(
            "search".into(),
            ToolConfig {
                strategy: RoutingStrategy::LoadBalance,
                endpoints: vec![
                    Endpoint {
                        provider: "github-mcp".into(),
                        service_id: "search".into(),
                        api_protocol: None,
                        api_key: Some("key-a".into()),
                        api_base: None,
                    },
                    Endpoint {
                        provider: "github-mcp".into(),
                        service_id: "search".into(),
                        api_protocol: None,
                        api_key: Some("key-b".into()),
                        api_base: None,
                    },
                ],
                ..Default::default()
            },
        );
        let table = ConfigToolRoutingTable::new(tool_providers(), tools);

        let t1 = table.resolve("search").unwrap();
        let t2 = table.resolve("search").unwrap();
        let t3 = table.resolve("search").unwrap();

        assert_eq!(t1.api_key_override.as_deref(), Some("key-a"));
        assert_eq!(t2.api_key_override.as_deref(), Some("key-b"));
        assert_eq!(t3.api_key_override.as_deref(), Some("key-a"));
    }

    #[tokio::test]
    async fn tool_list_tools_from_config() {
        let mut tools = HashMap::new();
        tools.insert(
            "search".into(),
            ToolConfig {
                endpoints: vec![Endpoint {
                    provider: "github-mcp".into(),
                    service_id: "search_code".into(),
                    api_protocol: None,
                    api_key: None,
                    api_base: None,
                }],
                description: Some("Search GitHub code".into()),
                ..Default::default()
            },
        );
        tools.insert(
            "web_search".into(),
            ToolConfig {
                endpoints: vec![Endpoint {
                    provider: "github-mcp".into(),
                    service_id: "web_search".into(),
                    api_protocol: None,
                    api_key: None,
                    api_base: None,
                }],
                ..Default::default()
            },
        );
        let table = ConfigToolRoutingTable::new(tool_providers(), tools);
        let entries = table.list_tools().await;
        assert_eq!(entries.len(), 2);

        // Sorted by id
        assert_eq!(entries[0].id, "github-mcp/search_code");
        assert_eq!(entries[0].provider, "github-mcp");
        assert_eq!(entries[0].definition.name, "search");
        assert_eq!(
            entries[0].definition.description.as_deref(),
            Some("Search GitHub code")
        );

        assert_eq!(entries[1].id, "github-mcp/web_search");
        assert!(entries[1].definition.description.is_none());
    }

    #[tokio::test]
    async fn tool_list_tools_empty_endpoints_skipped() {
        let mut tools = HashMap::new();
        tools.insert(
            "empty".into(),
            ToolConfig {
                endpoints: vec![],
                ..Default::default()
            },
        );
        let table = ConfigToolRoutingTable::new(tool_providers(), tools);
        let entries = table.list_tools().await;
        assert!(entries.is_empty());
    }

    #[test]
    fn tool_list_routes() {
        let mut tools = HashMap::new();
        tools.insert(
            "create_issue".into(),
            ToolConfig {
                endpoints: vec![Endpoint {
                    provider: "github-mcp".into(),
                    service_id: "create_issue".into(),
                    api_protocol: None,
                    api_key: None,
                    api_base: None,
                }],
                ..Default::default()
            },
        );
        let table = ConfigToolRoutingTable::new(tool_providers(), tools);
        let routes = table.list_routes();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].name, "create_issue");
        assert_eq!(routes[0].provider, "github-mcp");
        assert_eq!(routes[0].protocol, ApiProtocol::Mcp);
    }

    // ── ConfigAgentRegistry ──────────────────────────────────────

    fn test_agent_config(enabled: bool) -> AgentConfig {
        AgentConfig {
            protocol: crate::config::AgentProtocol::Acp,
            binary: "test-agent".to_owned(),
            args: Vec::new(),
            enabled,
            distribution: Vec::new(),
            session: None,
            a2a: None,
        }
    }

    #[tokio::test]
    async fn agent_registry_lists_enabled_as_idle() {
        let mut agents = HashMap::new();
        agents.insert("claude".to_owned(), test_agent_config(true));
        let registry = ConfigAgentRegistry::new(agents);

        let entries = registry.list_agents().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "claude");
        assert_eq!(entries[0].protocol, "acp");
        assert_eq!(entries[0].status, AgentEntryStatus::Idle);
    }

    #[tokio::test]
    async fn agent_registry_lists_disabled_as_unavailable() {
        let mut agents = HashMap::new();
        agents.insert("disabled-agent".to_owned(), test_agent_config(false));
        let registry = ConfigAgentRegistry::new(agents);

        let entries = registry.list_agents().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, AgentEntryStatus::Unavailable);
    }

    #[tokio::test]
    async fn agent_registry_sorted_by_name() {
        let mut agents = HashMap::new();
        agents.insert("zeta".to_owned(), test_agent_config(true));
        agents.insert("alpha".to_owned(), test_agent_config(true));
        let registry = ConfigAgentRegistry::new(agents);

        let entries = registry.list_agents().await;
        assert_eq!(entries[0].name, "alpha");
        assert_eq!(entries[1].name, "zeta");
    }

    #[tokio::test]
    async fn agent_registry_empty() {
        let registry = ConfigAgentRegistry::new(HashMap::new());
        assert!(registry.list_agents().await.is_empty());
    }

    #[test]
    fn agent_config_backward_compat_no_session_no_a2a() {
        let yaml = "binary: claude\nargs: [\"--agent\"]\n";
        let config: AgentConfig = serde_saphyr::from_str(yaml).expect("should parse");
        assert_eq!(config.binary, "claude");
        assert!(config.session.is_none());
        assert!(config.a2a.is_none());
    }

    #[test]
    fn agent_config_with_session_and_a2a() {
        let yaml = r#"
binary: claude
session:
  idle_timeout_secs: 300
  max_concurrent: 5
a2a:
  enabled: true
  skills:
    - coding
    - review
"#;
        let config: AgentConfig = serde_saphyr::from_str(yaml).expect("should parse");
        let session = config.session.expect("session should be present");
        assert_eq!(session.idle_timeout_secs, 300);
        assert_eq!(session.max_concurrent, 5);

        let a2a = config.a2a.expect("a2a should be present");
        assert!(a2a.enabled);
        assert_eq!(a2a.skills, vec!["coding", "review"]);
    }

    #[test]
    fn agent_session_config_defaults() {
        let config = crate::config::AgentSessionConfig::default();
        assert_eq!(config.idle_timeout_secs, 600);
        assert_eq!(config.max_concurrent, 1);
    }

    #[test]
    fn agent_session_config_partial_defaults() {
        let yaml = "idle_timeout_secs: 120\n";
        let config: crate::config::AgentSessionConfig =
            serde_saphyr::from_str(yaml).expect("should parse");
        assert_eq!(config.idle_timeout_secs, 120);
        assert_eq!(config.max_concurrent, 1); // default
    }

    #[test]
    fn agent_a2a_config_defaults() {
        let config = crate::config::AgentA2aConfig::default();
        assert!(!config.enabled);
        assert!(config.skills.is_empty());
    }

    // ── ANSI escape code sanitization ────────────────────────────────

    #[test]
    fn ansi_escape_stripped_from_fallback_routing() {
        // Reproduce: model name with ANSI bold code reaches fallback routing
        // (Strategy 3). The service_id in the resolved target must be clean.
        let mut providers = test_providers();
        providers.insert(
            "bitrouter".into(),
            ProviderConfig {
                api_protocol: Some(ApiProtocol::Openai),
                api_base: Some("https://api.bitrouter.ai/v1".into()),
                ..Default::default()
            },
        );
        let table = ConfigRoutingTable::new(providers, HashMap::new());

        // Incoming model name with ANSI bold suffix: \x1b[1m
        let target = table.resolve("claude-opus-4-6\x1b[1m").unwrap();
        assert_eq!(target.service_id, "claude-opus-4-6");
    }

    #[test]
    fn ansi_escape_stripped_from_direct_routing() {
        // Strategy 1: "provider:model_id" with ANSI in the model_id suffix.
        let table = ConfigRoutingTable::new(test_providers(), HashMap::new());
        let target = table
            .resolve("anthropic:\x1b[1mclaude-opus-4-6\x1b[0m")
            .unwrap();
        assert_eq!(target.provider_name, "anthropic");
        assert_eq!(target.service_id, "claude-opus-4-6");
    }

    #[test]
    fn ansi_escape_stripped_from_endpoint_service_id() {
        // Strategy 2: model lookup where the endpoint service_id contains ANSI.
        let mut models = HashMap::new();
        models.insert(
            "fast".into(),
            ModelConfig {
                strategy: RoutingStrategy::Priority,
                endpoints: vec![Endpoint {
                    provider: "anthropic".into(),
                    service_id: "claude-opus-4-6\x1b[1m".into(),
                    api_protocol: None,
                    api_key: None,
                    api_base: None,
                }],
                ..Default::default()
            },
        );
        let table = ConfigRoutingTable::new(test_providers(), models);
        let target = table.resolve("fast").unwrap();
        assert_eq!(target.service_id, "claude-opus-4-6");
    }

    #[tokio::test]
    async fn ansi_escape_stripped_in_route_trait() {
        // End-to-end: the RoutingTable::route method also strips ANSI.
        let mut providers = test_providers();
        providers.insert(
            "bitrouter".into(),
            ProviderConfig {
                api_protocol: Some(ApiProtocol::Openai),
                api_base: Some("https://api.bitrouter.ai/v1".into()),
                ..Default::default()
            },
        );
        let table = ConfigRoutingTable::new(providers, HashMap::new());

        let target = table
            .route("claude-opus-4-6\x1b[1m", &RouteContext::default())
            .await
            .unwrap();
        assert_eq!(target.service_id, "claude-opus-4-6");
    }
}
