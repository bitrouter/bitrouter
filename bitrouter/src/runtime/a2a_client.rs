//! A2A client — upstream agent registry and gateway route construction.

use std::sync::Arc;

use warp::Filter;

type RouteFilter = warp::filters::BoxedFilter<(Box<dyn warp::Reply>,)>;

/// Warp route filters produced by A2A client initialization.
pub struct A2aRoutes {
    /// A2A JSON-RPC gateway endpoint.
    pub gateway: RouteFilter,
    /// Admin agent management endpoints (gated by management auth).
    pub admin_agent_routes: RouteFilter,
    /// `GET /v1/agents` discovery endpoint.
    pub agent_list: RouteFilter,
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
            gateway: noop.clone(),
            admin_agent_routes: noop.clone(),
            agent_list: noop,
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
use bitrouter_api::router::{admin_agents, agents};
#[cfg(feature = "a2a")]
use bitrouter_config::{ApiProtocol, ProviderConfig};
#[cfg(feature = "a2a")]
use bitrouter_core::observe::{AgentObserveCallback, CallerContext};
#[cfg(feature = "a2a")]
use bitrouter_core::routers::dynamic_agent::DynamicAgentRegistry;
#[cfg(feature = "a2a")]
use bitrouter_providers::a2a::client::registry::UpstreamAgentRegistry;

#[cfg(feature = "a2a")]
use crate::runtime::auth::{self, JwtAuthContext};

/// Builder for A2A upstream agent registry and gateway routes.
#[cfg(feature = "a2a")]
pub struct A2aClient {
    providers: Vec<(String, ProviderConfig)>,
    listen_addr: SocketAddr,
    auth_ctx: Option<Arc<JwtAuthContext>>,
    observer: Option<Arc<dyn AgentObserveCallback>>,
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
            auth_ctx: None,
            observer: None,
            account_filter: None,
        }
    }

    pub fn with_auth(mut self, auth_ctx: Arc<JwtAuthContext>) -> Self {
        self.auth_ctx = Some(auth_ctx);
        self
    }

    pub fn with_observe(mut self, observer: Arc<dyn AgentObserveCallback>) -> Self {
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

        // Convert A2A providers into AgentConfigs.
        let a2a_configs: Vec<bitrouter_core::routers::upstream::AgentConfig> = self
            .providers
            .iter()
            .filter_map(|(name, p)| {
                bitrouter_config::compat::provider_to_agent_config(name, p)
                    .map_err(|e| {
                        tracing::warn!(provider = %name, error = %e, "skipping A2A provider");
                    })
                    .ok()
            })
            .collect();

        let reg = UpstreamAgentRegistry::from_configs(a2a_configs, external_base_url).await;

        let mut guards: Vec<Box<dyn std::any::Any + Send>> = Vec::new();

        let (gateway_reg, discovery_reg) = if reg.has_agents() {
            tracing::info!("A2A gateway started");
            let inner = Arc::new(reg);
            let guard = inner.spawn_refresh_listeners();
            guards.push(Box::new(guard));
            let wrapped = Arc::new(DynamicAgentRegistry::new(Arc::clone(&inner)));
            (Some(inner), Some(wrapped))
        } else {
            (None, None)
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

        // Build admin agent routes (gated by management auth when configured).
        let admin_agent_routes = if let Some(ref auth_ctx) = self.auth_ctx {
            auth::auth_gate(auth::management_auth(auth_ctx.clone()))
                .and(admin_agents::admin_agents_filter(discovery_reg.clone()))
                .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                .boxed()
        } else {
            admin_agents::admin_agents_filter(discovery_reg.clone())
                .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                .boxed()
        };

        let agent_list = agents::agents_filter(discovery_reg)
            .map(|r| Box::new(r) as Box<dyn warp::Reply>)
            .boxed();

        A2aRoutes {
            gateway,
            admin_agent_routes,
            agent_list,
            _guards: guards,
        }
    }
}
