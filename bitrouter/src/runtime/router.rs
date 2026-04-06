use std::collections::HashMap;
use std::sync::Arc;

use bitrouter_config::{ApiProtocol, ProviderConfig};
use bitrouter_core::{
    errors::{BitrouterError, Result},
    models::language::language_model::DynLanguageModel,
    routers::{content::RouteContext, router::LanguageModelRouter, routing_table::RoutingTarget},
    tools::provider::DynToolProvider,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest_middleware::ClientWithMiddleware;

#[cfg(feature = "mcp")]
use bitrouter_providers::mcp::client::upstream::UpstreamConnection;

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
            ApiProtocol::Mcp | ApiProtocol::Rest | ApiProtocol::Acp => {
                Err(BitrouterError::invalid_request(
                    Some(&target.provider_name),
                    format!(
                        "provider '{}' uses protocol '{}' which cannot serve models",
                        target.provider_name, target.api_protocol
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

/// A lazy tool router that constructs providers on-demand from config,
/// mirroring how [`Router`] constructs language models on-demand.
///
/// REST providers are stateless and constructed per-call.
/// MCP providers are looked up from a pre-built connection pool.
pub struct LazyToolRouter {
    providers: HashMap<String, ProviderConfig>,
    #[cfg(feature = "mcp")]
    mcp_pool: Arc<HashMap<String, Arc<UpstreamConnection>>>,
    #[cfg(feature = "rest")]
    client: Arc<reqwest::Client>,
}

impl LazyToolRouter {
    pub fn new(
        providers: HashMap<String, ProviderConfig>,
        #[cfg(feature = "mcp")] mcp_pool: Arc<HashMap<String, Arc<UpstreamConnection>>>,
        #[cfg(feature = "rest")] client: Arc<reqwest::Client>,
        #[cfg(not(feature = "rest"))] _client: Arc<reqwest::Client>,
    ) -> Self {
        Self {
            providers,
            #[cfg(feature = "mcp")]
            mcp_pool,
            #[cfg(feature = "rest")]
            client,
        }
    }

    /// Returns `true` if there are any tool-capable providers.
    pub fn has_providers(&self) -> bool {
        #[cfg(feature = "mcp")]
        let has_mcp = !self.mcp_pool.is_empty();
        #[cfg(not(feature = "mcp"))]
        let has_mcp = false;

        has_mcp
            || self
                .providers
                .values()
                .any(|p| p.api_protocol == Some(ApiProtocol::Rest))
    }
}

impl bitrouter_core::routers::router::ToolRouter for LazyToolRouter {
    async fn route_tool(&self, target: RoutingTarget) -> Result<Box<DynToolProvider<'static>>> {
        match target.api_protocol {
            #[cfg(feature = "rest")]
            ApiProtocol::Rest => {
                let provider = self.providers.get(&target.provider_name).ok_or_else(|| {
                    BitrouterError::invalid_request(
                        None,
                        format!("unknown REST provider: {}", target.provider_name),
                        None,
                    )
                })?;
                let api_base = provider.api_base.clone().ok_or_else(|| {
                    BitrouterError::invalid_request(
                        None,
                        format!("REST provider '{}' has no api_base", target.provider_name),
                        None,
                    )
                })?;
                let auth_header = resolve_auth_header(provider);
                let p = bitrouter_providers::rest::provider::RestToolProvider::new(
                    target.provider_name,
                    api_base,
                    auth_header,
                    self.client.clone(),
                );
                Ok(DynToolProvider::new_box(p))
            }
            #[cfg(not(feature = "rest"))]
            ApiProtocol::Rest => Err(BitrouterError::invalid_request(
                None,
                format!(
                    "REST protocol not available (feature disabled) for provider '{}'",
                    target.provider_name
                ),
                None,
            )),
            #[cfg(feature = "mcp")]
            ApiProtocol::Mcp => {
                let conn = self.mcp_pool.get(&target.provider_name).ok_or_else(|| {
                    BitrouterError::invalid_request(
                        None,
                        format!("no MCP connection for provider '{}'", target.provider_name),
                        None,
                    )
                })?;
                Ok(DynToolProvider::new_box(McpToolProviderAdapter(
                    Arc::clone(conn),
                )))
            }
            #[cfg(not(feature = "mcp"))]
            ApiProtocol::Mcp => Err(BitrouterError::invalid_request(
                None,
                format!(
                    "MCP protocol not available (feature disabled) for provider '{}'",
                    target.provider_name
                ),
                None,
            )),
            other => Err(BitrouterError::invalid_request(
                None,
                format!(
                    "protocol '{}' cannot serve tools for provider '{}'",
                    other, target.provider_name
                ),
                None,
            )),
        }
    }
}

// ── MCP → ToolProvider adapter ───────────────────────────────────

/// Thin wrapper that delegates [`ToolProvider`] to an `Arc<UpstreamConnection>`.
#[cfg(feature = "mcp")]
struct McpToolProviderAdapter(Arc<UpstreamConnection>);

#[cfg(feature = "mcp")]
impl bitrouter_core::tools::provider::ToolProvider for McpToolProviderAdapter {
    fn provider_name(&self) -> &str {
        bitrouter_core::tools::provider::ToolProvider::provider_name(self.0.as_ref())
    }

    async fn call_tool(
        &self,
        tool_id: &str,
        arguments: serde_json::Value,
    ) -> bitrouter_core::errors::Result<bitrouter_core::tools::result::ToolCallResult> {
        bitrouter_core::tools::provider::ToolProvider::call_tool(
            self.0.as_ref(),
            tool_id,
            arguments,
        )
        .await
    }
}

// ── REST auth helpers ────────────────────────────────────────────

/// Resolve the auth header from a provider config.
///
/// Priority: `auth` config (with resolved api_key) > `api_key` field (default
/// to Bearer). When the `auth.api_key` is an unsubstituted env var placeholder,
/// falls back to the provider-level `api_key` (which is resolved by
/// `env_prefix` during config loading).
#[cfg(feature = "rest")]
pub(crate) fn resolve_auth_header(config: &ProviderConfig) -> Option<(String, String)> {
    use bitrouter_config::AuthConfig;
    match config.auth.as_ref() {
        Some(AuthConfig::Bearer { api_key }) => {
            let key = resolve_key(api_key, config);
            Some(("Authorization".to_owned(), format!("Bearer {key}")))
        }
        Some(AuthConfig::Header {
            header_name,
            api_key,
        }) => {
            let key = resolve_key(api_key, config);
            Some((header_name.clone(), key))
        }
        Some(AuthConfig::X402 | AuthConfig::Mpp | AuthConfig::Custom { .. }) => None,
        None => {
            // Fall back to api_key field as Bearer token.
            config
                .api_key
                .as_ref()
                .map(|key| ("Authorization".to_owned(), format!("Bearer {key}")))
        }
    }
}

/// If the key is an unsubstituted env var placeholder (e.g. `"${EXA_API_KEY}"`),
/// fall back to the provider-level resolved `api_key`.
#[cfg(feature = "rest")]
fn resolve_key(auth_key: &str, config: &ProviderConfig) -> String {
    if auth_key.starts_with("${") && auth_key.ends_with('}') {
        config
            .api_key
            .clone()
            .unwrap_or_else(|| auth_key.to_owned())
    } else {
        auth_key.to_owned()
    }
}

// ── Tool call handler ────────────────────────────────────────────

/// [`ToolCallHandler`] implementation that dispatches `tools/call` through
/// a [`ToolRouter`] dispatch chain.
///
/// Routes tool calls using the config-authoritative routing table for
/// name resolution, then dispatches through the tool router (which may
/// wrap providers with guardrail enforcement via [`GuardedToolRouter`]).
#[cfg(feature = "mcp")]
pub struct RouterToolCallHandler<R, T> {
    tool_router: Arc<R>,
    tool_table: Arc<T>,
}

#[cfg(feature = "mcp")]
impl<R, T> RouterToolCallHandler<R, T> {
    pub fn new(tool_router: Arc<R>, tool_table: Arc<T>) -> Self {
        Self {
            tool_router,
            tool_table,
        }
    }
}

#[cfg(feature = "mcp")]
impl<R, T> bitrouter_core::api::mcp::gateway::ToolCallHandler for RouterToolCallHandler<R, T>
where
    R: bitrouter_core::routers::router::ToolRouter + Send + Sync + 'static,
    T: bitrouter_core::routers::routing_table::RoutingTable + Send + Sync + 'static,
{
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
            use bitrouter_core::api::mcp::types::McpGatewayError;
            use bitrouter_core::tools::provider::ToolProvider;

            let (_provider_name, tool_id) = name
                .split_once('/')
                .map(|(p, t)| (p.to_owned(), t.to_owned()))
                .unwrap_or_else(|| (name.clone(), name.clone()));

            // Route through config-authoritative table.
            let target = self
                .tool_table
                .route(&name, &RouteContext::default())
                .await
                .map_err(|e| McpGatewayError::ToolNotFound {
                    name: format!("{name}: {e}"),
                })?;

            // Dispatch through the tool router (GuardedToolRouter enforces
            // parameter restrictions on the returned provider).
            let provider_impl = self.tool_router.route_tool(target).await.map_err(|e| {
                McpGatewayError::UpstreamCall {
                    name: name.clone(),
                    reason: e.to_string(),
                }
            })?;

            let args_value = bitrouter_core::api::mcp::convert::args_to_value(arguments);
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

    fn test_providers() -> HashMap<String, ProviderConfig> {
        let mut p = HashMap::new();
        p.insert(
            "test-rest".into(),
            ProviderConfig {
                api_protocol: Some(ApiProtocol::Rest),
                api_base: Some("https://example.com/api".into()),
                api_key: Some("test-key".into()),
                ..Default::default()
            },
        );
        p
    }

    #[tokio::test]
    async fn route_to_rest_provider() {
        let router = LazyToolRouter::new(
            test_providers(),
            #[cfg(feature = "mcp")]
            Arc::new(HashMap::new()),
            Arc::new(reqwest::Client::new()),
        );

        let target = RoutingTarget {
            provider_name: "test-rest".into(),
            service_id: "search".into(),
            api_protocol: ApiProtocol::Rest,
        };

        let provider = router.route_tool(target).await;
        assert!(provider.is_ok());
        let p = provider.as_ref().ok();
        assert_eq!(p.map(|p| p.provider_name()), Some("test-rest"));
    }

    #[tokio::test]
    async fn route_to_unknown_provider_errors() {
        let router = LazyToolRouter::new(
            HashMap::new(),
            #[cfg(feature = "mcp")]
            Arc::new(HashMap::new()),
            Arc::new(reqwest::Client::new()),
        );

        let target = RoutingTarget {
            provider_name: "missing".into(),
            service_id: "foo".into(),
            api_protocol: ApiProtocol::Rest,
        };
        assert!(router.route_tool(target).await.is_err());
    }

    #[tokio::test]
    async fn has_providers_rest() {
        let router = LazyToolRouter::new(
            test_providers(),
            #[cfg(feature = "mcp")]
            Arc::new(HashMap::new()),
            Arc::new(reqwest::Client::new()),
        );
        assert!(router.has_providers());
    }

    #[tokio::test]
    async fn has_providers_empty() {
        let router = LazyToolRouter::new(
            HashMap::new(),
            #[cfg(feature = "mcp")]
            Arc::new(HashMap::new()),
            Arc::new(reqwest::Client::new()),
        );
        assert!(!router.has_providers());
    }
}
