//! A2A client — upstream agent registry and gateway route construction.

use std::sync::Arc;

use warp::Filter;

type RouteFilter = warp::filters::BoxedFilter<(Box<dyn warp::Reply>,)>;

/// Warp route filters produced by A2A client initialization.
pub struct A2aRoutes {
    /// A2A JSON-RPC gateway endpoint.
    pub gateway: RouteFilter,
    /// Type-erased per-agent tool providers for [`ToolRouterImpl`].
    pub tool_providers: Vec<(
        String,
        Arc<bitrouter_core::tools::provider::DynToolProvider<'static>>,
    )>,
    /// Background task guards — dropped when routes are dropped.
    _guards: Vec<Box<dyn std::any::Any + Send>>,
}

impl A2aRoutes {
    /// Noop routes for when the `a2a` feature is disabled.
    #[cfg(not(feature = "a2a"))]
    pub fn noop() -> Self {
        let noop = warp::path!("a2a" / ..)
            .and_then(|| async { Err::<String, _>(warp::reject::not_found()) })
            .map(|r: String| Box::new(r) as Box<dyn warp::Reply>)
            .boxed();
        Self {
            gateway: noop,
            tool_providers: Vec::new(),
            _guards: Vec::new(),
        }
    }
}

// ── Feature-gated builder ────────────────────────────────────────

#[cfg(feature = "a2a")]
use std::collections::HashMap;
#[cfg(feature = "a2a")]
use std::net::SocketAddr;

#[cfg(feature = "a2a")]
use bitrouter_config::{ApiProtocol, ProviderConfig};
#[cfg(feature = "a2a")]
use bitrouter_core::observe::{CallerContext, ToolObserveCallback};
#[cfg(feature = "a2a")]
use bitrouter_providers::a2a::client::registry::UpstreamAgentRegistry;

/// Builder for A2A upstream agent registry and gateway routes.
#[cfg(feature = "a2a")]
pub struct A2aClient {
    providers: Vec<(String, ProviderConfig)>,
    listen_addr: SocketAddr,
    observer: Option<Arc<dyn ToolObserveCallback>>,
    account_filter: Option<warp::filters::BoxedFilter<(CallerContext,)>>,
}

#[cfg(feature = "a2a")]
impl A2aClient {
    pub fn new(
        providers_by_protocol: &HashMap<ApiProtocol, Vec<(String, ProviderConfig)>>,
        listen_addr: SocketAddr,
    ) -> Self {
        let providers = providers_by_protocol
            .get(&ApiProtocol::A2a)
            .cloned()
            .unwrap_or_default();
        Self {
            providers,
            listen_addr,
            observer: None,
            account_filter: None,
        }
    }

    pub fn with_observe(mut self, observer: Arc<dyn ToolObserveCallback>) -> Self {
        self.observer = Some(observer);
        self
    }

    pub fn with_account_filter(
        mut self,
        filter: impl Filter<Extract = (CallerContext,), Error = warp::Rejection>
        + Clone
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.account_filter = Some(filter.boxed());
        self
    }

    pub async fn build(self) -> A2aRoutes {
        let external_base_url = format!("http://{}/a2a", self.listen_addr);

        // Build A2A agent configs from provider configs.
        use bitrouter_providers::a2a::client::config::A2aAgentConfig;

        let a2a_configs: Vec<A2aAgentConfig> = self
            .providers
            .iter()
            .filter_map(|(name, provider)| {
                let url = match provider.api_base.as_deref() {
                    Some(url) => url.to_owned(),
                    None => {
                        tracing::warn!(provider = %name, "A2A provider requires api_base, skipping");
                        return None;
                    }
                };

                let mut headers = provider.default_headers.clone().unwrap_or_default();
                if let Some(ref key) = provider.api_key {
                    headers
                        .entry("Authorization".to_owned())
                        .or_insert_with(|| format!("Bearer {key}"));
                }

                Some(A2aAgentConfig {
                    name: name.clone(),
                    url,
                    headers,
                })
            })
            .collect();

        let reg = UpstreamAgentRegistry::from_configs(a2a_configs, external_base_url).await;

        let mut guards: Vec<Box<dyn std::any::Any + Send>> = Vec::new();

        let (gateway_reg, tool_providers) = if reg.has_agents() {
            tracing::info!("A2A gateway started");
            let inner = Arc::new(reg);
            let guard = inner.spawn_refresh_listeners();
            guards.push(Box::new(guard));

            // Build type-erased tool providers.
            let providers: Vec<(
                String,
                Arc<bitrouter_core::tools::provider::DynToolProvider<'static>>,
            )> = inner
                .agent_names()
                .into_iter()
                .map(|name| {
                    let provider: Arc<bitrouter_core::tools::provider::DynToolProvider<'static>> =
                        Arc::from(bitrouter_core::tools::provider::DynToolProvider::new_box(
                            A2aToolProviderAdapter {
                                registry: Arc::clone(&inner),
                                agent_name: name.clone(),
                            },
                        ));
                    (name, provider)
                })
                .collect();

            (Some(inner), providers)
        } else {
            (None, Vec::new())
        };

        // Build A2A gateway with optional observation.
        let account_filter = self
            .account_filter
            .unwrap_or_else(|| warp::any().map(CallerContext::default).boxed());
        let gateway = bitrouter_api::router::a2a::a2a_gateway_filter(
            gateway_reg,
            self.observer,
            account_filter,
        )
        .map(|r| Box::new(r) as Box<dyn warp::Reply>)
        .boxed();

        A2aRoutes {
            gateway,
            tool_providers,
            _guards: guards,
        }
    }
}

/// Thin wrapper that delegates [`ToolProvider`] to a specific agent in the registry.
#[cfg(feature = "a2a")]
struct A2aToolProviderAdapter {
    registry: Arc<UpstreamAgentRegistry>,
    agent_name: String,
}

#[cfg(feature = "a2a")]
impl bitrouter_core::tools::provider::ToolProvider for A2aToolProviderAdapter {
    fn provider_name(&self) -> &str {
        &self.agent_name
    }

    async fn call_tool(
        &self,
        tool_id: &str,
        arguments: serde_json::Value,
    ) -> bitrouter_core::errors::Result<bitrouter_core::tools::result::ToolCallResult> {
        let agent = self.registry.get_agent(&self.agent_name).ok_or_else(|| {
            bitrouter_core::errors::BitrouterError::invalid_request(
                Some(&self.agent_name),
                format!("A2A agent '{}' not found in registry", self.agent_name),
                None,
            )
        })?;
        agent.call_tool(tool_id, arguments).await
    }
}
