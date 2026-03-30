use std::collections::HashMap;
use std::sync::Arc;

use bitrouter_config::{ApiProtocol, ProviderConfig};
use bitrouter_core::{
    errors::{BitrouterError, Result},
    models::language::language_model::DynLanguageModel,
    routers::{router::LanguageModelRouter, routing_table::RoutingTarget},
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

        match target.api_protocol {
            ApiProtocol::Openai => {
                let config = self.build_openai_config(provider)?;
                let model = OpenAiChatCompletionsModel::with_client(
                    target.service_id,
                    self.client.clone(),
                    config,
                );
                Ok(DynLanguageModel::new_box(model))
            }
            ApiProtocol::Anthropic => {
                let config = self.build_anthropic_config(provider)?;
                let model = AnthropicMessagesModel::with_client(
                    target.service_id,
                    self.client.clone(),
                    config,
                );
                Ok(DynLanguageModel::new_box(model))
            }
            ApiProtocol::Google => {
                let config = self.build_google_config(provider)?;
                let model = GoogleGenerativeAiModel::with_client(
                    target.service_id,
                    self.client.clone(),
                    config,
                );
                Ok(DynLanguageModel::new_box(model))
            }
            ApiProtocol::Mcp | ApiProtocol::Rest => Err(BitrouterError::invalid_request(
                Some(&target.provider_name),
                format!(
                    "provider '{}' uses tool protocol '{}' which cannot serve models",
                    target.provider_name, target.api_protocol
                ),
                None,
            )),
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
    providers: tokio::sync::RwLock<HashMap<String, Arc<DynToolProvider<'static>>>>,
}

impl ToolRouterImpl {
    pub fn new() -> Self {
        Self {
            providers: tokio::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Register a tool provider under the given name and protocol.
    ///
    /// Uses interior mutability (`RwLock`) so that providers can be added
    /// after the router is wrapped in an `Arc` and shared with the gateway.
    ///
    /// Providers are keyed by `"{name}:{protocol}"` to support multiple
    /// protocols for the same logical provider (e.g. `exa:rest`, `exa:mcp`).
    pub async fn register(
        &self,
        name: String,
        protocol: bitrouter_config::ApiProtocol,
        provider: Arc<DynToolProvider<'static>>,
    ) {
        let key = format!("{name}:{protocol}");
        self.providers.write().await.insert(key, provider);
    }

    /// Returns `true` if any providers are registered.
    pub async fn has_providers(&self) -> bool {
        !self.providers.read().await.is_empty()
    }

    /// Returns the number of registered providers.
    pub async fn provider_count(&self) -> usize {
        self.providers.read().await.len()
    }
}

impl bitrouter_core::routers::router::ToolRouter for ToolRouterImpl {
    async fn route_tool(&self, target: RoutingTarget) -> Result<Box<DynToolProvider<'static>>> {
        let key = format!("{}:{}", target.provider_name, target.api_protocol);
        let providers = self.providers.read().await;
        let provider = providers.get(&key).ok_or_else(|| {
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

// ── Tool call handler ────────────────────────────────────────────

/// [`ToolCallHandler`] implementation that dispatches `tools/call` through
/// the [`ToolRouterImpl`] dispatch chain.
///
/// Extracts provider name and protocol from the namespaced tool name
/// (e.g. `"github/search"` → provider `"github"`, tool `"search"`),
/// resolves the provider via `ToolRouterImpl`, and forwards the call.
///
/// Parameter restrictions are enforced before dispatch.
pub struct RouterToolCallHandler {
    tool_router: Arc<ToolRouterImpl>,
    /// Maps namespaced tool ID ("provider/tool") → API protocol.
    ///
    /// Keyed per-tool (not per-provider) so that a single provider can
    /// serve tools across multiple protocols (e.g. `exa/search` → REST,
    /// `exa/web_search_exa` → MCP).
    tool_protocols: HashMap<String, bitrouter_config::ApiProtocol>,
    /// Per-server parameter restrictions (read from `DynamicToolRegistry`).
    restrictions: std::sync::Arc<
        std::sync::RwLock<HashMap<String, bitrouter_core::routers::admin::ParamRestrictions>>,
    >,
}

impl RouterToolCallHandler {
    pub fn new(
        tool_router: Arc<ToolRouterImpl>,
        tool_protocols: HashMap<String, bitrouter_config::ApiProtocol>,
        restrictions: std::sync::Arc<
            std::sync::RwLock<HashMap<String, bitrouter_core::routers::admin::ParamRestrictions>>,
        >,
    ) -> Self {
        Self {
            tool_router,
            tool_protocols,
            restrictions,
        }
    }
}

impl bitrouter_core::api::mcp::gateway::ToolCallHandler for RouterToolCallHandler {
    fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = std::result::Result<
                        bitrouter_core::api::mcp::types::McpToolCallResult,
                        bitrouter_core::api::mcp::types::McpGatewayError,
                    >,
                > + Send
                + '_,
        >,
    > {
        let name = name.to_owned();
        Box::pin(async move {
            let (provider_name, tool_id) = name
                .split_once('/')
                .map(|(p, t)| (p.to_owned(), t.to_owned()))
                .unwrap_or_else(|| (name.clone(), name.clone()));

            use bitrouter_core::api::mcp::types::McpGatewayError;
            use bitrouter_core::routers::router::ToolRouter;
            use bitrouter_core::tools::provider::ToolProvider;

            // Enforce parameter restrictions before dispatch.
            let mut args_map = arguments;
            if let Ok(restrictions) = self.restrictions.read()
                && let Some(restriction) = restrictions.get(&provider_name)
            {
                restriction.check(&tool_id, &mut args_map).map_err(|e| {
                    McpGatewayError::UpstreamCall {
                        name: name.clone(),
                        reason: e.to_string(),
                    }
                })?;
            }

            // Look up protocol by full tool ID (e.g. "exa/search"),
            // supporting providers that serve tools across multiple protocols.
            let protocol = self
                .tool_protocols
                .get(&name)
                .ok_or_else(|| McpGatewayError::ToolNotFound { name: name.clone() })?;

            let target = RoutingTarget {
                provider_name,
                service_id: tool_id.clone(),
                api_protocol: *protocol,
            };

            let provider_impl = self.tool_router.route_tool(target).await.map_err(|e| {
                McpGatewayError::UpstreamCall {
                    name: name.clone(),
                    reason: e.to_string(),
                }
            })?;

            let args_value = bitrouter_core::api::mcp::convert::args_to_value(args_map);
            let result = provider_impl
                .call_tool(&tool_id, args_value)
                .await
                .map_err(|e| McpGatewayError::UpstreamCall {
                    name,
                    reason: e.to_string(),
                })?;

            Ok(bitrouter_core::api::mcp::types::McpToolCallResult::from(
                result,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_core::routers::router::ToolRouter;
    use bitrouter_core::tools::provider::ToolProvider;

    #[tokio::test]
    async fn route_to_registered_provider() {
        let router = ToolRouterImpl::new();

        // Register a dummy MCP provider.
        let dummy = DynToolProvider::new_box(DummyProvider("test-mcp".into()));
        router
            .register("test-mcp".into(), ApiProtocol::Mcp, Arc::from(dummy))
            .await;

        let target = RoutingTarget {
            provider_name: "test-mcp".into(),
            service_id: "search".into(),
            api_protocol: ApiProtocol::Mcp,
        };

        let provider = router.route_tool(target).await;
        assert!(provider.is_ok());
        assert_eq!(provider.unwrap().provider_name(), "test-mcp");
    }

    #[tokio::test]
    async fn route_to_unknown_provider_errors() {
        let router = ToolRouterImpl::new();
        let target = RoutingTarget {
            provider_name: "missing".into(),
            service_id: "foo".into(),
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
