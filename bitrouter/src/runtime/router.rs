use std::collections::HashMap;
use std::sync::Arc;

use bitrouter_config::{ApiProtocol, ProviderConfig};
use bitrouter_core::{
    errors::{BitrouterError, Result},
    models::language::language_model::DynLanguageModel,
    routers::{
        router::LanguageModelRouter,
        routing_table::{RoutingTarget, ToolRoutingTarget},
    },
    tools::provider::DynToolProvider,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest_middleware::ClientWithMiddleware;

/// A model router backed by `reqwest` that instantiates concrete provider
/// model objects on demand from [`ProviderConfig`] entries.
pub struct Router {
    client: ClientWithMiddleware,
    providers: HashMap<String, ProviderConfig>,
}

impl Router {
    pub fn new(client: ClientWithMiddleware, providers: HashMap<String, ProviderConfig>) -> Self {
        Self { client, providers }
    }

    fn build_openai_config(&self, provider: &ProviderConfig) -> Result<OpenAiConfig> {
        let api_key = provider.api_key.clone().unwrap_or_default();
        let base_url = provider
            .api_base
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".into());

        let default_headers = parse_headers(provider.default_headers.as_ref())?;

        Ok(OpenAiConfig {
            api_key,
            base_url,
            organization: None,
            project: None,
            default_headers,
        })
    }

    fn build_anthropic_config(&self, provider: &ProviderConfig) -> Result<AnthropicConfig> {
        let api_key = provider.api_key.clone().unwrap_or_default();
        let base_url = provider
            .api_base
            .clone()
            .unwrap_or_else(|| "https://api.anthropic.com".into());

        let default_headers = parse_headers(provider.default_headers.as_ref())?;

        Ok(AnthropicConfig {
            api_key,
            base_url,
            api_version: "2023-06-01".into(),
            default_headers,
        })
    }

    fn build_google_config(&self, provider: &ProviderConfig) -> Result<GoogleConfig> {
        let api_key = provider.api_key.clone().unwrap_or_default();
        let base_url = provider
            .api_base
            .clone()
            .unwrap_or_else(|| "https://generativelanguage.googleapis.com".into());

        let default_headers = parse_headers(provider.default_headers.as_ref())?;

        Ok(GoogleConfig {
            api_key,
            base_url,
            default_headers,
        })
    }
}

impl LanguageModelRouter for Router {
    async fn route_model(&self, target: RoutingTarget) -> Result<Box<DynLanguageModel<'static>>> {
        let provider = self.providers.get(&target.provider_name).ok_or_else(|| {
            BitrouterError::invalid_request(
                None,
                format!("unknown provider: {}", target.provider_name),
                None,
            )
        })?;

        let protocol = provider.api_protocol.as_ref().ok_or_else(|| {
            BitrouterError::invalid_request(
                Some(&target.provider_name),
                format!(
                    "provider '{}' has no api_protocol configured",
                    target.provider_name
                ),
                None,
            )
        })?;

        match protocol {
            ApiProtocol::Openai => {
                let config = self.build_openai_config(provider)?;
                let model = OpenAiChatCompletionsModel::with_client(
                    target.model_id,
                    self.client.clone(),
                    config,
                );
                Ok(DynLanguageModel::new_box(model))
            }
            ApiProtocol::Anthropic => {
                let config = self.build_anthropic_config(provider)?;
                let model = AnthropicMessagesModel::with_client(
                    target.model_id,
                    self.client.clone(),
                    config,
                );
                Ok(DynLanguageModel::new_box(model))
            }
            ApiProtocol::Google => {
                let config = self.build_google_config(provider)?;
                let model = GoogleGenerativeAiModel::with_client(
                    target.model_id,
                    self.client.clone(),
                    config,
                );
                Ok(DynLanguageModel::new_box(model))
            }
            ApiProtocol::Mcp | ApiProtocol::A2a | ApiProtocol::Rest | ApiProtocol::Skill => {
                Err(BitrouterError::invalid_request(
                    Some(&target.provider_name),
                    format!(
                        "provider '{}' uses tool protocol '{}' which cannot serve models",
                        target.provider_name, protocol
                    ),
                    None,
                ))
            }
        }
    }
}

fn parse_headers(headers: Option<&HashMap<String, String>>) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    if let Some(h) = headers {
        for (k, v) in h {
            let name = HeaderName::from_bytes(k.as_bytes()).map_err(|e| {
                BitrouterError::invalid_request(
                    None,
                    format!("invalid header name '{k}': {e}"),
                    None,
                )
            })?;
            let value = HeaderValue::from_str(v).map_err(|e| {
                BitrouterError::invalid_request(
                    None,
                    format!("invalid header value for '{k}': {e}"),
                    None,
                )
            })?;
            map.insert(name, value);
        }
    }
    Ok(map)
}

// Re-export provider types under short aliases for readability.
use bitrouter_providers::anthropic::messages::provider::{AnthropicConfig, AnthropicMessagesModel};
use bitrouter_providers::google::generate_content::provider::{
    GoogleConfig, GoogleGenerativeAiModel,
};
use bitrouter_providers::openai::chat::provider::{OpenAiChatCompletionsModel, OpenAiConfig};

// ── Tool router ──────────────────────────────────────────────────

/// A tool router backed by a pre-built pool of tool providers.
///
/// Each provider is an `Arc`-wrapped [`DynToolProvider`] keyed by provider
/// name, constructed at server startup from upstream MCP connections and
/// A2A agent connections.
pub struct ToolRouterImpl {
    providers: HashMap<String, Arc<DynToolProvider<'static>>>,
}

impl ToolRouterImpl {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    /// Register a tool provider under the given name.
    pub fn register(&mut self, name: String, provider: Arc<DynToolProvider<'static>>) {
        self.providers.insert(name, provider);
    }

    /// Returns `true` if any providers are registered.
    pub fn has_providers(&self) -> bool {
        !self.providers.is_empty()
    }

    /// Returns the number of registered providers.
    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }
}

impl bitrouter_core::routers::router::ToolRouter for ToolRouterImpl {
    async fn route_tool(&self, target: ToolRoutingTarget) -> Result<Box<DynToolProvider<'static>>> {
        let provider = self.providers.get(&target.provider_name).ok_or_else(|| {
            BitrouterError::invalid_request(
                None,
                format!(
                    "no tool provider '{}' registered (protocol: {})",
                    target.provider_name, target.api_protocol
                ),
                None,
            )
        })?;

        // Return a boxed clone of the Arc-backed provider.
        // DynToolProvider is created from Arc, so this is cheap.
        Ok(DynToolProvider::new_box(ArcToolProvider {
            inner: Arc::clone(provider),
            name: target.provider_name,
        }))
    }
}

/// Thin wrapper that implements `ToolProvider` by delegating to an
/// `Arc<DynToolProvider>`, allowing `route_tool` to return a fresh
/// `Box<DynToolProvider>` without cloning the underlying connection.
struct ArcToolProvider {
    inner: Arc<DynToolProvider<'static>>,
    name: String,
}

impl bitrouter_core::tools::provider::ToolProvider for ArcToolProvider {
    fn provider_name(&self) -> &str {
        &self.name
    }

    async fn call_tool(
        &self,
        tool_id: &str,
        arguments: serde_json::Value,
    ) -> Result<bitrouter_core::tools::result::ToolCallResult> {
        self.inner.call_tool(tool_id, arguments).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_core::routers::router::ToolRouter;
    use bitrouter_core::tools::provider::ToolProvider;

    #[tokio::test]
    async fn route_to_registered_provider() {
        let mut router = ToolRouterImpl::new();

        // Register a dummy MCP provider.
        let dummy = DynToolProvider::new_box(DummyProvider("test-mcp".into()));
        router.register("test-mcp".into(), Arc::from(dummy));

        let target = ToolRoutingTarget {
            provider_name: "test-mcp".into(),
            tool_id: "search".into(),
            api_protocol: ApiProtocol::Mcp,
        };

        let provider = router.route_tool(target).await;
        assert!(provider.is_ok());
        assert_eq!(provider.unwrap().provider_name(), "test-mcp");
    }

    #[tokio::test]
    async fn route_to_unknown_provider_errors() {
        let router = ToolRouterImpl::new();
        let target = ToolRoutingTarget {
            provider_name: "missing".into(),
            tool_id: "foo".into(),
            api_protocol: ApiProtocol::Mcp,
        };
        assert!(router.route_tool(target).await.is_err());
    }

    struct DummyProvider(String);

    impl ToolProvider for DummyProvider {
        fn provider_name(&self) -> &str {
            &self.0
        }

        async fn call_tool(
            &self,
            _tool_id: &str,
            _arguments: serde_json::Value,
        ) -> Result<bitrouter_core::tools::result::ToolCallResult> {
            Ok(bitrouter_core::tools::result::ToolCallResult {
                content: vec![],
                is_error: false,
                metadata: None,
            })
        }
    }
}
