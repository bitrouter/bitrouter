use std::sync::Arc;

use bitrouter_api::router::{admin, models, routes};
use bitrouter_api::router::{anthropic, google, openai};
use bitrouter_config::BitrouterConfig;
#[cfg(feature = "a2a")]
use bitrouter_core::observe::AgentObserveCallback;
#[cfg(feature = "mcp")]
use bitrouter_core::observe::ToolObserveCallback;
use bitrouter_core::observe::{CallerContext, ObserveCallback};
use bitrouter_core::routers::admin::AdminRoutingTable;
use bitrouter_core::routers::model_router::LanguageModelRouter;
use bitrouter_core::routers::registry::ModelRegistry;
use bitrouter_guardrails::{GuardedRouter, Guardrail};
use bitrouter_observe::builder::ObserveStack;
use sea_orm::DatabaseConnection;
use warp::Filter;

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

        let tool_configs = self.config.tools.clone();
        observe_builder = observe_builder.tool_cost(move |_server, tool| {
            tool_configs
                .get(tool)
                .and_then(|tc| tc.pricing.as_ref())
                .map_or(0.0, |p| p.cost_for(tool))
        });

        let agent_tool_configs = self.config.tools.clone();
        observe_builder = observe_builder.agent_cost(move |agent, method| {
            agent_tool_configs
                .get(agent)
                .and_then(|tc| tc.pricing.as_ref())
                .map_or(0.0, |p| p.cost_for(method))
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
        let admin_routes = auth::auth_gate(auth::management_auth(auth_ctx.clone()))
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

        // ── Tool routing table ────────────────────────────────────────
        let tool_table = bitrouter_config::ConfigToolRoutingTable::new(
            self.config.providers.clone(),
            self.config.tools.clone(),
        );
        let providers_by_protocol = tool_table.providers_by_protocol();

        // ── Skills registry (filesystem-backed, no DB) ──────────────
        let skills_dir = self
            .paths
            .as_ref()
            .map(|p| p.home_dir.join("skills"))
            .unwrap_or_else(|| std::path::PathBuf::from("skills"));

        let skills = crate::runtime::agentskills_client::AgentSkillsClient::new(
            &providers_by_protocol,
            &tool_table,
            skills_dir,
        )
        .build()
        .await;

        // ── MCP registry ─────────────────────────────────────────────
        #[cfg(feature = "mcp")]
        let mcp = {
            let tool_observer: Arc<dyn ToolObserveCallback> = observe.observer.clone();
            crate::runtime::mcp_client::McpClient::new(
                &providers_by_protocol,
                self.table.clone(),
                raw_router,
            )
            .with_auth(auth_ctx.clone())
            .with_observe(tool_observer)
            .with_account_filter(account_filter.clone())
            .with_skill_registry(skills.registry.clone())
            .build()
            .await
        };
        #[cfg(not(feature = "mcp"))]
        let mcp = crate::runtime::mcp_client::McpRoutes::noop();

        // ── A2A protocol ─────────────────────────────────────────────
        #[cfg(feature = "a2a")]
        let a2a = {
            let agent_observer: Arc<dyn AgentObserveCallback> = observe.observer.clone();
            crate::runtime::a2a_client::A2aClient::new(
                &providers_by_protocol,
                self.config.server.listen,
            )
            .with_auth(auth_ctx.clone())
            .with_observe(agent_observer)
            .with_account_filter(account_filter.clone())
            .build()
            .await
        };
        #[cfg(not(feature = "a2a"))]
        let a2a = crate::runtime::a2a_client::A2aRoutes::noop();

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
            .or(a2a.gateway)
            .or(a2a.admin_agent_routes)
            .or(a2a.agent_list)
            .or(mcp.admin_tool_routes)
            .or(mcp.tool_list)
            .or(skills.skills_list)
            .or(mcp.server)
            // Bridge routes come after the aggregated MCP filter so that the static
            // paths POST /mcp and GET /mcp/sse are matched first.
            .or(mcp.bridge_routes);

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
