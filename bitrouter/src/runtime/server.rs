use std::sync::Arc;

use bitrouter_api::router::{admin, admin_agents, admin_tools, models, routes};
use bitrouter_api::router::{anthropic, google, openai};
use bitrouter_config::BitrouterConfig;
use bitrouter_core::observe::{
    AgentObserveCallback, CallerContext, ObserveCallback, ToolObserveCallback,
};
use bitrouter_core::routers::admin::AdminRoutingTable;
use bitrouter_core::routers::model_router::LanguageModelRouter;
use bitrouter_core::routers::registry::ModelRegistry;
use bitrouter_guardrails::{GuardedRouter, Guardrail};
use bitrouter_observe::builder::ObserveStack;
use sea_orm::DatabaseConnection;
use warp::Filter;

#[cfg(feature = "mcp")]
use bitrouter_api::router::mcp as mcp_admin;

use crate::runtime::auth::{self, JwtAuthContext, Unauthorized};
use crate::runtime::error::Result;

/// Conditional bound: when MPP features are enabled, the routing table must
/// also implement `PricingLookup` so that handlers can compute per-request costs.
#[cfg(any(feature = "mpp-tempo", feature = "mpp-solana"))]
pub(crate) trait ServerTableBound:
    AdminRoutingTable + ModelRegistry + bitrouter_api::mpp::PricingLookup
{
}

#[cfg(any(feature = "mpp-tempo", feature = "mpp-solana"))]
impl<T: AdminRoutingTable + ModelRegistry + bitrouter_api::mpp::PricingLookup> ServerTableBound
    for T
{
}

#[cfg(not(any(feature = "mpp-tempo", feature = "mpp-solana")))]
pub(crate) trait ServerTableBound: AdminRoutingTable + ModelRegistry {}

#[cfg(not(any(feature = "mpp-tempo", feature = "mpp-solana")))]
impl<T: AdminRoutingTable + ModelRegistry> ServerTableBound for T {}

pub struct ServerPlan<T, R> {
    config: BitrouterConfig,
    table: Arc<T>,
    router: Arc<R>,
    db: Option<Arc<DatabaseConnection>>,
    paths: Option<crate::runtime::paths::RuntimePaths>,
    reload_fn: Option<Arc<dyn Fn() -> std::result::Result<(), String> + Send + Sync>>,
}

impl<T, R> ServerPlan<T, R>
where
    T: ServerTableBound + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    pub fn new(config: BitrouterConfig, table: Arc<T>, router: Arc<R>) -> Self {
        Self {
            config,
            table,
            router,
            db: None,
            paths: None,
            reload_fn: None,
        }
    }

    pub fn with_db(mut self, db: Arc<DatabaseConnection>) -> Self {
        self.db = Some(db);
        self
    }

    pub fn with_paths(mut self, paths: crate::runtime::paths::RuntimePaths) -> Self {
        self.paths = Some(paths);
        self
    }

    /// Register a callback invoked when the server receives a reload signal.
    ///
    /// The callback should re-read the configuration from disk and swap the
    /// inner routing table.  It is called from a background task; errors are
    /// logged but do not stop the server.
    pub fn with_reload(
        mut self,
        f: impl Fn() -> std::result::Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        self.reload_fn = Some(Arc::new(f));
        self
    }
}

impl<T, R> ServerPlan<T, R>
where
    T: ServerTableBound + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    pub async fn serve(self) -> Result<()> {
        let addr = self.config.server.listen;

        // Build guardrail engine from config and wrap the model router.
        let raw_router = Arc::clone(&self.router);
        let guardrail = Arc::new(Guardrail::new(self.config.guardrails.clone()));
        let guarded_router = Arc::new(GuardedRouter::new(self.router, guardrail.clone()));

        if guardrail.is_disabled() {
            tracing::info!("guardrails disabled");
        } else {
            tracing::info!("guardrails enabled");
        }

        // Build JWT auth context.
        let auth_ctx = Arc::new(JwtAuthContext::new(
            self.db.as_ref().map(|db| db.as_ref().clone()),
        ));

        // Build the observation stack: spend tracking + metrics for all service types.
        let mut observe_builder = ObserveStack::builder();

        if let Some(ref db) = self.db {
            observe_builder = observe_builder.with_db(db.as_ref());
        }

        let providers = self.config.providers.clone();
        observe_builder = observe_builder.model_pricing(move |provider, model| {
            providers
                .get(provider)
                .and_then(|p| p.models.as_ref())
                .and_then(|models| models.get(model))
                .map(|info| info.pricing.clone())
                .unwrap_or_default()
        });

        let tool_pricing = self.config.mcp_server_pricing.clone();
        observe_builder = observe_builder.tool_cost(move |server, tool| {
            tool_pricing.get(server).map_or(0.0, |p| p.cost_for(tool))
        });

        let agent_pricing = self.config.a2a_agent_pricing.clone();
        observe_builder = observe_builder.agent_cost(move |agent, method| {
            agent_pricing.get(agent).map_or(0.0, |p| p.cost_for(method))
        });

        let observe = observe_builder.build();
        let observer: Arc<dyn ObserveCallback> = observe.observer.clone();
        let metrics_collector = observe.metrics.clone();

        let health = warp::path("health")
            .and(warp::get())
            .map(|| warp::reply::json(&serde_json::json!({ "status": "ok" })));

        // Metrics endpoint backed by the in-memory MetricsCollector.
        let metrics_route = {
            let mc = metrics_collector.clone();
            warp::path!("v1" / "metrics")
                .and(warp::get())
                .map(move || warp::reply::json(&mc.snapshot()))
        };

        // Route listing — no auth required.
        let route_list = routes::routes_filter(self.table.clone());

        // Model listing — no auth required.
        let model_list = models::models_filter(self.table.clone());

        // Admin route management — gated by management auth.
        let admin_routes = auth_gate(auth::management_auth(auth_ctx.clone()))
            .and(admin::admin_routes_filter(self.table.clone()));

        // Build account filter that extracts caller context when auth is enabled,
        // or returns a default (empty) caller context when no database is configured.
        let account_filter = if self.db.is_some() {
            let auth_filter = auth::openai_auth(auth_ctx.clone());
            warp::any()
                .and(auth_filter)
                .map(identity_to_caller_context)
                .boxed()
        } else {
            warp::any().map(CallerContext::default).boxed()
        };

        let anthropic_account_filter = if self.db.is_some() {
            let auth_filter = auth::anthropic_auth(auth_ctx.clone());
            warp::any()
                .and(auth_filter)
                .map(identity_to_caller_context)
                .boxed()
        } else {
            warp::any().map(CallerContext::default).boxed()
        };

        // Model API routes with observation.
        // When MPP is enabled, payment-gated filters replace the standard
        // auth-gated filters. We use .boxed() to unify the filter types.
        #[cfg(any(feature = "mpp-tempo", feature = "mpp-solana"))]
        let mpp_state: Option<Arc<bitrouter_api::mpp::MppState>> = {
            match self.config.mpp.as_ref().filter(|c| c.enabled) {
                Some(mpp_config) => {
                    let realm = mpp_config.realm.as_deref().unwrap_or("MPP Payment");
                    let secret_key = mpp_config.secret_key.as_deref();
                    let mut state = bitrouter_api::mpp::MppState::new(realm);

                    #[cfg(feature = "mpp-tempo")]
                    if let Some(tempo) = mpp_config.networks.tempo.as_ref() {
                        let close_signer: Option<Arc<dyn mpp::Signer + Send + Sync>> =
                            match tempo.close_signer.as_deref() {
                                Some(key_hex) => {
                                    let signer: mpp::PrivateKeySigner =
                                        key_hex.parse().map_err(|e| {
                                            bitrouter_config::ConfigError::ConfigParse(format!(
                                                "invalid close_signer: {e}"
                                            ))
                                        })?;
                                    Some(Arc::new(signer))
                                }
                                None => None,
                            };
                        state
                            .add_tempo(tempo, secret_key, close_signer)
                            .map_err(|e| {
                                bitrouter_config::ConfigError::ConfigParse(format!(
                                    "MPP Tempo initialization failed: {e}"
                                ))
                            })?;
                        tracing::info!("MPP Tempo backend enabled");
                    }

                    #[cfg(feature = "mpp-solana")]
                    if let Some(solana) = mpp_config.networks.solana.as_ref() {
                        state.add_solana(solana, secret_key).map_err(|e| {
                            bitrouter_config::ConfigError::ConfigParse(format!(
                                "MPP Solana initialization failed: {e}"
                            ))
                        })?;
                        tracing::info!("MPP Solana backend enabled");
                    }

                    if state.is_configured() {
                        tracing::info!(realm = state.realm(), "MPP enabled");
                        Some(Arc::new(state))
                    } else {
                        None
                    }
                }
                None => None,
            }
        };

        #[cfg(any(feature = "mpp-tempo", feature = "mpp-solana"))]
        let (chat, messages, responses, generate_content) = if let Some(ref mpp) = mpp_state {
            (
                openai::chat::filters::chat_completions_filter_with_mpp(
                    self.table.clone(),
                    guarded_router.clone(),
                    observer.clone(),
                    mpp.clone(),
                    account_filter.clone(),
                )
                .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                .boxed(),
                anthropic::messages::filters::messages_filter_with_mpp(
                    self.table.clone(),
                    guarded_router.clone(),
                    observer.clone(),
                    mpp.clone(),
                    anthropic_account_filter.clone(),
                )
                .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                .boxed(),
                openai::responses::filters::responses_filter_with_mpp(
                    self.table.clone(),
                    guarded_router.clone(),
                    observer.clone(),
                    mpp.clone(),
                    account_filter.clone(),
                )
                .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                .boxed(),
                google::generate_content::filters::generate_content_filter_with_mpp(
                    self.table.clone(),
                    guarded_router.clone(),
                    observer.clone(),
                    mpp.clone(),
                    account_filter.clone(),
                )
                .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                .boxed(),
            )
        } else {
            (
                openai::chat::filters::chat_completions_filter_with_observe(
                    self.table.clone(),
                    guarded_router.clone(),
                    observer.clone(),
                    account_filter.clone(),
                )
                .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                .boxed(),
                anthropic::messages::filters::messages_filter_with_observe(
                    self.table.clone(),
                    guarded_router.clone(),
                    observer.clone(),
                    anthropic_account_filter,
                )
                .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                .boxed(),
                openai::responses::filters::responses_filter_with_observe(
                    self.table.clone(),
                    guarded_router.clone(),
                    observer.clone(),
                    account_filter.clone(),
                )
                .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                .boxed(),
                google::generate_content::filters::generate_content_filter_with_observe(
                    self.table.clone(),
                    guarded_router.clone(),
                    observer.clone(),
                    account_filter.clone(),
                )
                .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                .boxed(),
            )
        };

        #[cfg(not(any(feature = "mpp-tempo", feature = "mpp-solana")))]
        let chat = openai::chat::filters::chat_completions_filter_with_observe(
            self.table.clone(),
            guarded_router.clone(),
            observer.clone(),
            account_filter.clone(),
        );
        #[cfg(not(any(feature = "mpp-tempo", feature = "mpp-solana")))]
        let messages = anthropic::messages::filters::messages_filter_with_observe(
            self.table.clone(),
            guarded_router.clone(),
            observer.clone(),
            anthropic_account_filter,
        );
        #[cfg(not(any(feature = "mpp-tempo", feature = "mpp-solana")))]
        let responses = openai::responses::filters::responses_filter_with_observe(
            self.table.clone(),
            guarded_router.clone(),
            observer.clone(),
            account_filter.clone(),
        );
        #[cfg(not(any(feature = "mpp-tempo", feature = "mpp-solana")))]
        let generate_content =
            google::generate_content::filters::generate_content_filter_with_observe(
                self.table.clone(),
                guarded_router.clone(),
                observer.clone(),
                account_filter.clone(),
            );

        // ── Skills registry (filesystem-backed, no DB) ──────────────
        let skills_dir = self
            .paths
            .as_ref()
            .map(|p| p.home_dir.join("skills"))
            .unwrap_or_else(|| std::path::PathBuf::from("skills"));

        let skill_registry = match bitrouter_providers::agentskills::registry::FilesystemSkillRegistry::from_config_and_dir(
            self.config.skills.clone(),
            skills_dir.clone(),
        )
        .await
        {
            Ok(reg) => Arc::new(reg),
            Err(e) => {
                tracing::warn!("failed to initialize skills from config: {e}");
                Arc::new(
                    bitrouter_providers::agentskills::registry::FilesystemSkillRegistry::from_dir(
                        skills_dir,
                    )
                    .await
                    .map_err(|e2| tracing::warn!("skills registry unavailable: {e2}"))
                    .unwrap_or_default(),
                )
            }
        };

        // ── MCP registry ─────────────────────────────────────────────
        #[cfg(feature = "mcp")]
        let (
            admin_tool_routes,
            mcp_server,
            tool_list,
            bridge_routes,
            _refresh_guard,
            _bridge_guards,
        ) = {
            use bitrouter_core::api::mcp::gateway::{
                McpClientRequestHandler, McpPromptServer, McpResourceServer, McpToolServer,
            };
            use bitrouter_core::routers::admin::{ParamRestrictions, ToolFilter};
            use bitrouter_core::routers::dynamic_tool::DynamicToolRegistry;
            use bitrouter_providers::mcp::client::bridge::SingleServerBridge;
            use bitrouter_providers::mcp::client::registry::ConfigMcpRegistry;
            use bitrouter_providers::mcp::client::upstream::UpstreamConnection;

            use crate::runtime::mcp_handler::McpSamplingHandler;

            let mcp_configs = self.config.mcp_servers.clone();
            let mcp_groups = self.config.mcp_groups.clone();

            // Extract initial filters and restrictions from config for the wrapper.
            let initial_filters: std::collections::HashMap<String, ToolFilter> = self
                .config
                .mcp_servers
                .iter()
                .filter_map(|cfg| cfg.tool_filter.clone().map(|f| (cfg.name.clone(), f)))
                .collect();
            let initial_restrictions: std::collections::HashMap<String, ParamRestrictions> = self
                .config
                .mcp_servers
                .iter()
                .filter(|cfg| !cfg.param_restrictions.rules.is_empty())
                .map(|cfg| (cfg.name.clone(), cfg.param_restrictions.clone()))
                .collect();
            let groups = self.config.mcp_groups.as_map().clone();

            // Build the sampling handler so upstream MCP servers can request
            // LLM generation via sampling/createMessage.
            let sampling_handler: Option<Arc<dyn McpClientRequestHandler>> = Some(Arc::new(
                McpSamplingHandler::new(self.table.clone(), raw_router.clone()),
            ));

            // Build all upstream connections upfront so bridges can share them.
            let mut connections: std::collections::HashMap<String, Arc<UpstreamConnection>> =
                std::collections::HashMap::with_capacity(mcp_configs.len());
            for config in &mcp_configs {
                let name = config.name.clone();
                match UpstreamConnection::connect(config.clone(), sampling_handler.clone()).await {
                    Ok(conn) => {
                        connections.insert(name, Arc::new(conn));
                    }
                    Err(e) => {
                        tracing::warn!(
                            upstream = %name,
                            error = %e,
                            "failed to connect to MCP upstream"
                        );
                    }
                }
            }

            let (inner, registry, refresh_guard) = if !connections.is_empty() {
                let reg = ConfigMcpRegistry::from_connections(connections.clone(), mcp_groups);
                tracing::info!("MCP registry started with {} upstreams", connections.len());
                let inner = Arc::new(reg);
                let guard = inner.spawn_refresh_listeners().await;
                let wrapped = Arc::new(DynamicToolRegistry::new(
                    Arc::clone(&inner),
                    initial_filters,
                    initial_restrictions,
                    groups,
                ));
                (Some(inner), Some(wrapped), Some(guard))
            } else {
                (None, None, None)
            };

            // Build bridge endpoints for servers with `bridge: true`.
            let mut bridge_map: std::collections::HashMap<String, Arc<SingleServerBridge>> =
                std::collections::HashMap::new();
            let mut bridge_guards: Vec<bitrouter_providers::mcp::client::registry::RefreshGuard> =
                Vec::new();
            if let Some(ref reg) = inner {
                for config in mcp_configs.iter().filter(|c| c.bridge) {
                    if let Some(conn) = connections.get(&config.name) {
                        let (bridge, guard) = SingleServerBridge::new(
                            Arc::clone(conn),
                            McpToolServer::subscribe_tool_changes(reg.as_ref()),
                            McpResourceServer::subscribe_resource_changes(reg.as_ref()),
                            McpPromptServer::subscribe_prompt_changes(reg.as_ref()),
                        );
                        tracing::info!(server = %config.name, "MCP bridge enabled");
                        bridge_map.insert(config.name.clone(), bridge);
                        bridge_guards.push(guard);
                    }
                }
            }
            let bridges = mcp_admin::mcp_bridge_filter(Arc::new(bridge_map));

            let admin = auth_gate(auth::management_auth(auth_ctx.clone()))
                .and(admin_tools::admin_tools_filter(registry.clone()));

            // Build MCP server filter with tool call observation.
            let tool_observer: Arc<dyn ToolObserveCallback> = observe.observer.clone();
            let server = mcp_admin::mcp_server_filter_with_observe(
                registry.clone(),
                tool_observer,
                account_filter.clone(),
            );
            // Compose MCP tools + agentskills into a single ToolRegistry
            // for the unified GET /v1/tools endpoint.
            let tools = if let Some(ref mcp_reg) = registry {
                let composite = Arc::new(
                    bitrouter_core::routers::registry::CompositeToolRegistry::new(
                        mcp_reg.clone(),
                        skill_registry.clone(),
                    ),
                );
                bitrouter_api::router::tools::tools_filter(Some(composite))
                    .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                    .boxed()
            } else if !self.config.skills.is_empty() {
                bitrouter_api::router::tools::tools_filter(Some(skill_registry.clone()))
                    .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                    .boxed()
            } else {
                bitrouter_api::router::tools::tools_filter(registry)
                    .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                    .boxed()
            };

            (admin, server, tools, bridges, refresh_guard, bridge_guards)
        };
        #[cfg(not(feature = "mcp"))]
        let (admin_tool_routes, mcp_server, tool_list, bridge_routes) = {
            let noop = warp::path!("mcp" / ..)
                .and_then(|| async { Err::<String, _>(warp::reject::not_found()) })
                .map(|r: String| Box::new(r) as Box<dyn warp::Reply>)
                .boxed();
            (noop.clone(), noop.clone(), noop.clone(), noop)
        };

        // ── Skills registry ───────────────────────────────────────────
        // The same FilesystemSkillRegistry serves both /v1/tools (ToolRegistry)
        // and /v1/skills CRUD (SkillService) — no database needed.
        let skills_list = bitrouter_api::router::skills::skills_filter(skill_registry)
            .map(|r| Box::new(r) as Box<dyn warp::Reply>)
            .boxed();

        // ── A2A protocol ─────────────────────────────────────────────
        #[cfg(feature = "a2a")]
        let (a2a_routes, admin_agent_routes, agent_list, _a2a_refresh_guard) = {
            use bitrouter_core::routers::dynamic_agent::DynamicAgentRegistry;
            use bitrouter_providers::a2a::client::registry::UpstreamAgentRegistry;

            let external_base_url = format!("http://{}/a2a", self.config.server.listen);
            let a2a_configs = self.config.a2a_agents.clone();

            let reg = UpstreamAgentRegistry::from_configs(a2a_configs, external_base_url).await;

            let (gateway_reg, discovery_reg, refresh_guard) = if reg.has_agents() {
                tracing::info!("A2A gateway started");
                let inner = Arc::new(reg);
                let guard = inner.spawn_refresh_listeners();
                let wrapped = Arc::new(DynamicAgentRegistry::new(Arc::clone(&inner)));
                (Some(inner), Some(wrapped), Some(guard))
            } else {
                (None, None, None)
            };

            let agent_observer: Option<Arc<dyn AgentObserveCallback>> =
                Some(observe.observer.clone() as Arc<dyn AgentObserveCallback>);

            let a2a_account_filter = if self.db.is_some() {
                let auth_filter = auth::openai_auth(auth_ctx.clone());
                warp::any()
                    .and(auth_filter)
                    .map(identity_to_caller_context)
                    .boxed()
            } else {
                warp::any().map(CallerContext::default).boxed()
            };

            let admin = auth_gate(auth::management_auth(auth_ctx.clone()))
                .and(admin_agents::admin_agents_filter(discovery_reg.clone()));
            let agents = bitrouter_api::router::agents::agents_filter(discovery_reg);

            (
                bitrouter_api::router::a2a::a2a_gateway_filter(
                    gateway_reg,
                    agent_observer,
                    a2a_account_filter,
                ),
                admin,
                agents,
                refresh_guard,
            )
        };
        #[cfg(not(feature = "a2a"))]
        let (a2a_routes, admin_agent_routes, agent_list) = {
            let noop = warp::path!("a2a" / ..)
                .and_then(|| async { Err::<String, _>(warp::reject::not_found()) })
                .map(|r: String| Box::new(r) as Box<dyn warp::Reply>)
                .boxed();
            (noop.clone(), noop.clone(), noop)
        };

        // ── Base route tree (always present) ─────────────────────────
        let base = health
            .or(metrics_route)
            .or(route_list)
            .or(model_list)
            .or(admin_routes)
            .or(chat)
            .or(messages)
            .or(responses)
            .or(generate_content);

        // ── Compose all routes ─────────────────────────────────────────
        let all_routes = base
            .or(a2a_routes)
            .or(admin_agent_routes)
            .or(agent_list)
            .or(admin_tool_routes)
            .or(tool_list)
            .or(skills_list)
            .or(mcp_server)
            // Bridge routes come after the aggregated MCP filter so that the static
            // paths POST /mcp and GET /mcp/sse are matched first.
            .or(bridge_routes);

        // ── Reload listener ────────────────────────────────────────
        let _reload_guard = if let Some(reload_fn) = self.reload_fn {
            let paths = self.paths.clone();
            Some(tokio::spawn(reload_listener(reload_fn, paths)))
        } else {
            None
        };

        // ── Serve ────────────────────────────────────────────────────
        if let Some(ref db) = self.db {
            let db_conn = db.as_ref().clone();
            let mgmt_auth = auth::management_auth(auth_ctx.clone());
            let acct =
                bitrouter_accounts::filters::account_routes(db_conn.clone(), mgmt_auth.clone());
            let sess = bitrouter_accounts::filters::session_routes(db_conn, mgmt_auth);

            let all = all_routes
                .or(acct)
                .or(sess)
                .recover(handle_auth_rejection)
                .with(warp::trace::request());

            tracing::info!(%addr, "server listening (JWT auth enabled)");
            let server = warp::serve(all)
                .bind(addr)
                .await
                .graceful(shutdown_signal());
            server.run().await;
        } else {
            let all = all_routes
                .recover(handle_auth_rejection)
                .with(warp::trace::request());

            tracing::info!(%addr, "server listening (auth disabled — no database configured)");
            let server = warp::serve(all)
                .bind(addr)
                .await
                .graceful(shutdown_signal());
            server.run().await;
        }

        tracing::info!("server stopped");
        Ok(())
    }
}

/// Map an authenticated [`Identity`] to the transport-neutral [`CallerContext`].
fn identity_to_caller_context(id: bitrouter_accounts::identity::Identity) -> CallerContext {
    CallerContext {
        account_id: Some(id.account_id.0.to_string()),
        models: id.models,
        tools: id.tools,
        budget: id.budget,
        budget_scope: id.budget_scope,
        budget_range: id.budget_range,
        chain: id.chain,
    }
}

/// Convert an auth filter into a gate that rejects unauthorized requests
/// but does not add anything to the extract tuple.
fn auth_gate(
    auth: impl Filter<Extract = (bitrouter_accounts::identity::Identity,), Error = warp::Rejection>
    + Clone,
) -> impl Filter<Extract = (), Error = warp::Rejection> + Clone {
    auth.map(|_| ()).untuple_one()
}

/// Rejection handler that turns [`Unauthorized`] into a JSON 401 response
/// and MPP rejections into 402 responses.
async fn handle_auth_rejection(
    rejection: warp::Rejection,
) -> std::result::Result<impl warp::Reply, warp::Rejection> {
    if let Some(e) = rejection.find::<Unauthorized>() {
        let json = warp::reply::json(&serde_json::json!({
            "error": {
                "message": e.to_string(),
                "type": "authentication_error",
            }
        }));
        return Ok(Box::new(warp::reply::with_status(
            json,
            warp::http::StatusCode::UNAUTHORIZED,
        )) as Box<dyn warp::Reply>);
    }

    #[cfg(any(feature = "mpp-tempo", feature = "mpp-solana"))]
    if let Some(challenge) = rejection.find::<bitrouter_api::mpp::MppChallenge>() {
        match warp::http::Response::builder()
            .status(warp::http::StatusCode::PAYMENT_REQUIRED)
            .header("WWW-Authenticate", &challenge.www_authenticate)
            .header("Content-Type", "application/json")
            .body(
                serde_json::json!({
                    "error": {
                        "message": "Payment required",
                        "type": "payment_required",
                    }
                })
                .to_string(),
            ) {
            Ok(resp) => return Ok(Box::new(resp) as Box<dyn warp::Reply>),
            Err(_) => return Err(rejection),
        }
    }

    #[cfg(any(feature = "mpp-tempo", feature = "mpp-solana"))]
    if let Some(err) = rejection.find::<bitrouter_api::mpp::MppVerificationFailed>() {
        let json = warp::reply::json(&serde_json::json!({
            "error": {
                "message": err.message,
                "type": "payment_verification_failed",
            }
        }));
        return Ok(Box::new(warp::reply::with_status(
            json,
            warp::http::StatusCode::PAYMENT_REQUIRED,
        )) as Box<dyn warp::Reply>);
    }

    if let Some(resp) = bitrouter_api::error::handle_bitrouter_rejection(&rejection) {
        return Ok(Box::new(resp) as Box<dyn warp::Reply>);
    }

    Err(rejection)
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            ctrl_c.await.ok();
            return;
        };
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await.ok();
    }

    tracing::info!("shutdown signal received");
}

/// Background task that listens for reload signals and invokes the callback.
///
/// On Unix this waits for `SIGHUP`; on Windows it polls for a reload flag
/// file in the runtime directory.
async fn reload_listener(
    reload_fn: Arc<dyn Fn() -> std::result::Result<(), String> + Send + Sync>,
    paths: Option<crate::runtime::paths::RuntimePaths>,
) {
    tracing::info!("configuration reload listener started");
    loop {
        wait_for_reload_signal(&paths).await;
        tracing::info!("reload signal received, reloading configuration...");
        match reload_fn() {
            Ok(()) => tracing::info!("configuration reloaded successfully"),
            Err(e) => tracing::error!("configuration reload failed: {e}"),
        }
    }
}

/// Wait for a platform-specific reload signal.
///
/// - **Unix**: waits for `SIGHUP`.
/// - **Windows**: polls for the existence of a `reload` flag file inside the
///   runtime directory every second.
async fn wait_for_reload_signal(paths: &Option<crate::runtime::paths::RuntimePaths>) {
    #[cfg(unix)]
    {
        let _ = paths;
        let Ok(mut hup) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        else {
            // If we can't register a handler, sleep forever.
            std::future::pending::<()>().await;
            return;
        };
        hup.recv().await;
    }

    #[cfg(not(unix))]
    {
        // Windows: poll for a reload flag file.
        const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            if let Some(p) = &paths {
                let flag = p.runtime_dir.join("reload");
                if flag.exists() {
                    // Remove the flag so we don't fire again immediately.
                    let _ = std::fs::remove_file(&flag);
                    return;
                }
            }
        }
    }
}
