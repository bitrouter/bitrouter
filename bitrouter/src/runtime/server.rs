use std::sync::Arc;

use bitrouter_api::router::{admin, admin_agents, admin_tools, models, routes};
use bitrouter_api::router::{anthropic, google, openai};
use bitrouter_config::BitrouterConfig;
use bitrouter_core::observe::{CallerContext, ObserveCallback};
use bitrouter_core::routers::admin::AdminRoutingTable;
use bitrouter_core::routers::model_router::LanguageModelRouter;
use bitrouter_core::routers::registry::ModelRegistry;
use bitrouter_guardrails::{GuardedRouter, Guardrail};
use bitrouter_observe::composite::CompositeObserver;
use bitrouter_observe::cost::Pricing;
use bitrouter_observe::metrics::MetricsCollector;
use bitrouter_observe::observer::SpendObserver;
use bitrouter_observe::spend::memory::InMemorySpendStore;
use bitrouter_observe::spend::sea_orm_store::SeaOrmSpendStore;
use bitrouter_observe::spend::store;
use sea_orm::DatabaseConnection;
use warp::Filter;

#[cfg(feature = "mcp")]
use bitrouter_api::router::mcp as mcp_admin;

use crate::runtime::auth::{self, JwtAuthContext, Unauthorized};
use crate::runtime::error::Result;

pub struct ServerPlan<T, R> {
    config: BitrouterConfig,
    table: Arc<T>,
    router: Arc<R>,
    db: Option<Arc<DatabaseConnection>>,
}

impl<T, R> ServerPlan<T, R>
where
    T: AdminRoutingTable + ModelRegistry + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    pub fn new(config: BitrouterConfig, table: Arc<T>, router: Arc<R>) -> Self {
        Self {
            config,
            table,
            router,
            db: None,
        }
    }

    pub fn with_db(mut self, db: Arc<DatabaseConnection>) -> Self {
        self.db = Some(db);
        self
    }

    pub async fn serve(self) -> Result<()> {
        let addr = self.config.server.listen;

        // Build guardrail engine from config and wrap the model router.
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

        // Build spend store: SeaORM-backed if DB is available, otherwise in-memory.
        let spend_store: Arc<dyn store::SpendStore> = match &self.db {
            Some(db) => Arc::new(SeaOrmSpendStore::new(db.as_ref().clone())),
            None => Arc::new(InMemorySpendStore::new()),
        };

        // Build pricing lookup from the config's provider definitions.
        let providers = self.config.providers.clone();
        let pricing_fn = move |provider: &str, model: &str| {
            let mp = providers
                .get(provider)
                .and_then(|p| p.models.as_ref())
                .and_then(|models| models.get(model))
                .map(|info| &info.pricing)
                .cloned()
                .unwrap_or_default();
            Pricing {
                input_no_cache: mp.input_tokens.no_cache,
                input_cache_read: mp.input_tokens.cache_read,
                input_cache_write: mp.input_tokens.cache_write,
                output_text: mp.output_tokens.text,
                output_reasoning: mp.output_tokens.reasoning,
            }
        };

        // Compose observers: spend tracking + metrics aggregation.
        let spend_observer = Arc::new(SpendObserver::new(spend_store, Arc::new(pricing_fn)));
        let metrics_collector = Arc::new(MetricsCollector::new());
        let observer: Arc<dyn ObserveCallback> = Arc::new(CompositeObserver::new(vec![
            spend_observer as Arc<dyn ObserveCallback>,
            metrics_collector.clone() as Arc<dyn ObserveCallback>,
        ]));

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
        let chat = openai::chat::filters::chat_completions_filter_with_observe(
            self.table.clone(),
            guarded_router.clone(),
            observer.clone(),
            account_filter.clone(),
        );
        let messages = anthropic::messages::filters::messages_filter_with_observe(
            self.table.clone(),
            guarded_router.clone(),
            observer.clone(),
            anthropic_account_filter,
        );
        let responses = openai::responses::filters::responses_filter_with_observe(
            self.table.clone(),
            guarded_router.clone(),
            observer.clone(),
            account_filter.clone(),
        );
        let generate_content =
            google::generate_content::filters::generate_content_filter_with_observe(
                self.table.clone(),
                guarded_router.clone(),
                observer.clone(),
                account_filter,
            );

        // ── MCP registry ─────────────────────────────────────────────
        #[cfg(feature = "mcp")]
        let (admin_tool_routes, mcp_server, tool_list, _refresh_guard) = {
            use bitrouter_core::routers::admin::{ParamRestrictions, ToolFilter};
            use bitrouter_core::routers::dynamic_tool::DynamicToolRegistry;
            use bitrouter_mcp::client::registry::ConfigMcpRegistry;

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

            let (registry, refresh_guard) =
                match ConfigMcpRegistry::from_configs(mcp_configs, mcp_groups).await {
                    Ok(reg) => {
                        tracing::info!(
                            "MCP registry started with {} upstreams",
                            self.config.mcp_servers.len()
                        );
                        // Build Arc first so we can spawn refresh listeners,
                        // then wrap with DynamicToolRegistry.
                        let inner = Arc::new(reg);
                        let guard = inner.spawn_refresh_listeners().await;
                        let wrapped = Arc::new(DynamicToolRegistry::new(
                            inner,
                            initial_filters,
                            initial_restrictions,
                            groups,
                        ));
                        (Some(wrapped), Some(guard))
                    }
                    Err(e) => {
                        tracing::warn!("MCP registry failed to start: {e}");
                        (None, None)
                    }
                };

            let admin = auth_gate(auth::management_auth(auth_ctx.clone()))
                .and(admin_tools::admin_tools_filter(registry.clone()));
            let server = mcp_admin::mcp_server_filter(registry.clone());
            let tools = bitrouter_api::router::tools::tools_filter(registry);

            (admin, server, tools, refresh_guard)
        };

        // ── A2A protocol ─────────────────────────────────────────────
        #[cfg(feature = "a2a")]
        let (a2a_routes, admin_agent_routes, agent_list, _a2a_refresh_guard) = {
            use bitrouter_a2a::client::registry::UpstreamAgentRegistry;
            use bitrouter_core::routers::dynamic_agent::DynamicAgentRegistry;

            let external_url = format!("http://{}/a2a", self.config.server.listen);
            let a2a_config = self.config.a2a_agent.clone();

            let (registry, refresh_guard) =
                match UpstreamAgentRegistry::from_config(a2a_config, external_url).await {
                    Ok(reg) => {
                        if self.config.a2a_agent.is_some() {
                            tracing::info!("A2A gateway started");
                        }
                        let inner = Arc::new(reg);
                        let guard = inner.spawn_refresh_listeners();
                        let wrapped = Arc::new(DynamicAgentRegistry::new(inner));
                        (Some(wrapped), Some(guard))
                    }
                    Err(e) => {
                        tracing::warn!("A2A gateway failed to start: {e}");
                        (None, None)
                    }
                };

            let admin = auth_gate(auth::management_auth(auth_ctx.clone()))
                .and(admin_agents::admin_agents_filter(registry.clone()));
            let agents = bitrouter_api::router::agents::agents_filter(registry.clone());

            (
                bitrouter_api::router::a2a::a2a_gateway_filter(registry),
                admin,
                agents,
                refresh_guard,
            )
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

        // ── Compose optional routes ──────────────────────────────────
        #[cfg(all(feature = "a2a", feature = "mcp"))]
        let all_routes = base
            .or(a2a_routes)
            .or(admin_agent_routes)
            .or(agent_list)
            .or(admin_tool_routes)
            .or(tool_list)
            .or(mcp_server);

        #[cfg(all(feature = "a2a", not(feature = "mcp")))]
        let all_routes = base.or(a2a_routes).or(admin_agent_routes).or(agent_list);

        #[cfg(all(not(feature = "a2a"), feature = "mcp"))]
        let all_routes = base.or(admin_tool_routes).or(tool_list).or(mcp_server);

        #[cfg(all(not(feature = "a2a"), not(feature = "mcp")))]
        let all_routes = base;

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

/// Rejection handler that turns [`Unauthorized`] into a JSON 401 response.
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
        return Ok(warp::reply::with_status(
            json,
            warp::http::StatusCode::UNAUTHORIZED,
        ));
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
