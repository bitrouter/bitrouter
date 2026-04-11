use std::sync::Arc;

use bitrouter_api::router::{admin, agents, models, routes};
use bitrouter_api::router::{anthropic, google, openai};
use bitrouter_config::BitrouterConfig;
#[cfg(feature = "mcp")]
use bitrouter_core::observe::ToolObserveCallback;
use bitrouter_core::observe::{CallerContext, ObserveCallback};
use bitrouter_core::routers::admin::AdminRoutingTable;
use bitrouter_core::routers::registry::ModelRegistry;
use bitrouter_core::routers::router::LanguageModelRouter;
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
    /// Pre-built config-authoritative tool registry with policy enforcement.
    ///
    /// When provided, `serve()` uses this instead of building one internally.
    /// This allows the reload closure (built in `app.rs`) to share the same
    /// inner `DynamicRoutingTable` `Arc` and swap it on SIGHUP.
    tool_registry: Option<
        Arc<
            bitrouter_core::policy::GuardedToolRegistry<
                Arc<
                    bitrouter_core::routers::dynamic::DynamicRoutingTable<
                        bitrouter_config::ConfigToolRoutingTable,
                    >,
                >,
            >,
        >,
    >,
    /// Per-caller tool policy resolver for MCP enforcement.
    ///
    /// When set, the MCP filter layer resolves per-caller policies from
    /// the JWT `pol` claim and enforces tool allow-lists.
    policy_resolver: Option<Arc<dyn bitrouter_core::routers::admin::ToolPolicyResolver>>,
    /// Per-key revocation set for JWT `id` claim checking.
    revocation_set: Option<Arc<dyn bitrouter_core::auth::revocation::KeyRevocationSet>>,
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
            tool_registry: None,
            policy_resolver: None,
            revocation_set: None,
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

    /// Provide a pre-built tool registry with policy for hot-reload support.
    pub fn with_tool_registry(
        mut self,
        registry: Arc<
            bitrouter_core::policy::GuardedToolRegistry<
                Arc<
                    bitrouter_core::routers::dynamic::DynamicRoutingTable<
                        bitrouter_config::ConfigToolRoutingTable,
                    >,
                >,
            >,
        >,
    ) -> Self {
        self.tool_registry = Some(registry);
        self
    }

    /// Provide a per-caller policy resolver for MCP tool access enforcement.
    pub fn with_policy_resolver(
        mut self,
        resolver: Arc<dyn bitrouter_core::routers::admin::ToolPolicyResolver>,
    ) -> Self {
        self.policy_resolver = Some(resolver);
        self
    }

    /// Provide a per-key revocation set for JWT `id` claim checking.
    pub fn with_revocation_set(
        mut self,
        set: Arc<dyn bitrouter_core::auth::revocation::KeyRevocationSet>,
    ) -> Self {
        self.revocation_set = Some(set);
        self
    }

    /// Resolve the close signer for Tempo MPP.
    ///
    /// Priority:
    /// 1. OWS wallet (if `wallet` config present)
    /// 2. Hex private key from `tempo.close_signer`
    /// 3. None (close signing disabled)
    #[cfg(feature = "mpp-tempo")]
    fn resolve_close_signer(
        tempo: &bitrouter_config::TempoMppConfig,
        config: &BitrouterConfig,
    ) -> std::result::Result<Option<Arc<dyn mpp::Signer + Send + Sync>>, Box<dyn std::error::Error>>
    {
        // Try OWS wallet first.
        if let Some(wallet) = config.wallet.as_ref() {
            let credential = std::env::var("OWS_PASSPHRASE").unwrap_or_default();
            let vault_path = wallet.vault_path.as_deref().map(std::path::Path::new);

            let signer = crate::runtime::ows_signer::OwsSigner::new(
                &wallet.name,
                &credential,
                None,
                vault_path,
                None,
            )?;
            tracing::info!(
                wallet = %wallet.name,
                address = %alloy::signers::Signer::address(&signer),
                "OWS wallet loaded for MPP close signing",
            );
            return Ok(Some(Arc::new(signer)));
        }

        // Fall back to hex private key.
        if let Some(key_hex) = tempo.close_signer.as_deref() {
            let signer: mpp::PrivateKeySigner = key_hex
                .parse()
                .map_err(|e| format!("invalid close_signer hex key: {e}"))?;
            return Ok(Some(Arc::new(signer)));
        }

        Ok(None)
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
        // Clone the raw router before it is moved into the guarded wrapper —
        // MCP sampling needs the unwrapped router when the feature is enabled.
        #[cfg(feature = "mcp")]
        let raw_router = Arc::clone(&self.router);
        let guardrail = Arc::new(Guardrail::new(self.config.guardrails.clone()));
        let guarded_router = Arc::new(GuardedRouter::new(self.router, guardrail.clone()));

        if guardrail.is_disabled() {
            tracing::info!("guardrails disabled");
        } else {
            tracing::info!("guardrails enabled");
        }

        // Build JWT auth context with operator identity from wallet config.
        let operator_caip10 = self.config.wallet.as_ref().and_then(|wallet| {
            resolve_operator_caip10(wallet)
                .map_err(|e| tracing::warn!("could not resolve operator CAIP-10: {e}"))
                .ok()
        });
        let mut auth_ctx = JwtAuthContext::new(
            self.db.as_ref().map(|db| db.as_ref().clone()),
            operator_caip10,
        );
        if let Some(revocation_set) = self.revocation_set.clone() {
            auth_ctx = auth_ctx.with_revocation_set(revocation_set);
        }
        let auth_ctx = Arc::new(auth_ctx);

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
        observe_builder = observe_builder.tool_cost(move |provider, operation| {
            tool_configs
                .get(provider)
                .and_then(|tc| tc.pricing.as_ref())
                .map_or(0.0, |p| p.cost_for(operation))
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

        // Agent listing — no auth required.
        let agent_registry = if self.config.agents.is_empty() {
            None
        } else {
            Some(Arc::new(bitrouter_config::ConfigAgentRegistry::new(
                self.config.agents.clone(),
            )))
        };
        let agent_list = agents::agents_filter(agent_registry);

        // Admin route management — gated by management auth.
        let admin_routes = auth::auth_gate(auth::management_auth(auth_ctx.clone()))
            .and(admin::admin_routes_filter(self.table.clone()));

        // Admin key revocation endpoint — gated by management auth.
        let admin_key_revoke = {
            let revocation_set = self.revocation_set.clone();
            auth::auth_gate(auth::management_auth(auth_ctx.clone()))
                .and(warp::path!("admin" / "keys" / "revoke"))
                .and(warp::post())
                .and(warp::body::json::<KeyRevokeRequest>())
                .and(warp::any().map(move || revocation_set.clone()))
                .and_then(handle_key_revoke)
        };

        // Build account filter that extracts caller context when auth is enabled,
        // or returns a default (empty) caller context when no database is configured.
        // When a database is connected, budget enforcement is chained after auth:
        // accumulated spend is compared against the JWT `bgt` claim.
        let spend_store = observe.spend_store.clone();
        let account_filter = if self.db.is_some() {
            let auth_filter = auth::openai_auth(auth_ctx.clone());
            let ss = spend_store.clone();
            warp::any()
                .and(auth_filter)
                .map(identity_to_caller_context)
                .and(warp::any().map(move || ss.clone()))
                .and_then(crate::runtime::budget::check_budget)
                .boxed()
        } else {
            warp::any().map(CallerContext::default).boxed()
        };

        let anthropic_account_filter = if self.db.is_some() {
            let auth_filter = auth::anthropic_auth(auth_ctx.clone());
            let ss = spend_store.clone();
            warp::any()
                .and(auth_filter)
                .map(identity_to_caller_context)
                .and(warp::any().map(move || ss.clone()))
                .and_then(crate::runtime::budget::check_budget)
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
                            match Self::resolve_close_signer(tempo, &self.config) {
                                Ok(s) => s,
                                Err(e) => {
                                    return Err(bitrouter_config::ConfigError::ConfigParse(
                                        format!("close_signer: {e}"),
                                    )
                                    .into());
                                }
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

        // ── Tool routing table (config-authoritative) ─────────────────
        // Use pre-built registry from app.rs (enables hot-reload), or build one.
        let tool_table_ref = Arc::new(bitrouter_config::ConfigToolRoutingTable::new(
            self.config.providers.clone(),
            self.config.tools.clone(),
        ));
        #[cfg(feature = "mcp")]
        let providers_by_protocol = tool_table_ref.providers_by_protocol();

        let dynamic_tool_registry = if let Some(registry) = self.tool_registry {
            registry
        } else {
            let inner_tool_table =
                Arc::new(bitrouter_core::routers::dynamic::DynamicRoutingTable::new(
                    bitrouter_config::ConfigToolRoutingTable::new(
                        self.config.providers.clone(),
                        self.config.tools.clone(),
                    ),
                ));
            Arc::new(bitrouter_core::policy::GuardedToolRegistry::new(
                inner_tool_table,
                std::collections::HashMap::new(),
            ))
        };

        // ── Skills registry (filesystem-backed, no DB) ──────────────
        let skills_dir = self
            .paths
            .as_ref()
            .map(|p| p.home_dir.join("skills"))
            .unwrap_or_else(|| std::path::PathBuf::from("skills"));

        let skills =
            crate::runtime::agentskills_client::AgentSkillsClient::new(&tool_table_ref, skills_dir)
                .build()
                .await;
        let has_skill_tools = skills.has_skills;

        // ── MCP connections ─────────────────────────────────────────
        #[cfg(feature = "mcp")]
        let mcp = {
            crate::runtime::mcp_client::McpClient::new(
                &providers_by_protocol,
                self.table.clone(),
                raw_router,
            )
            .build()
            .await
        };
        #[cfg(not(feature = "mcp"))]
        let mcp = crate::runtime::mcp_client::McpRoutes::noop();

        // Destructure MCP outputs so fields can be consumed independently.
        #[cfg(feature = "mcp")]
        let (mcp_connections, mcp_registry, mcp_bridge_routes) =
            (mcp.connections, mcp.registry, mcp.bridge_routes);
        #[cfg(not(feature = "mcp"))]
        let mcp_bridge_routes = mcp.bridge_routes;

        // ── Lazy tool router ────────────────────────────────────────
        let lazy_tool_router = Arc::new(crate::runtime::router::LazyToolRouter::new(
            self.config.providers.clone(),
            #[cfg(feature = "mcp")]
            Arc::new(mcp_connections),
            Arc::new(reqwest::Client::new()),
        ));

        if lazy_tool_router.has_providers() {
            tracing::info!("tool router initialized");
        }

        // The tool call handler dispatches through the lazy tool router.
        // Per-caller policy enforcement (visibility + param restrictions) is
        // handled in the MCP filter layer via the policy resolver — no
        // GuardedToolRouter wrapping needed.
        #[cfg(feature = "mcp")]
        let tool_call_handler: Option<
            Arc<dyn bitrouter_core::api::mcp::gateway::ToolCallHandler>,
        > = {
            if lazy_tool_router.has_providers() {
                Some(Arc::new(
                    crate::runtime::router::RouterToolCallHandler::new(
                        lazy_tool_router.clone(),
                        dynamic_tool_registry.clone(),
                    ),
                ))
            } else {
                None
            }
        };

        // ── MCP registries ─────────────────────────────────────────
        //
        // The MCP server endpoint (POST /mcp) needs `McpServer`, satisfied
        // by `DynamicRoutingTable<ConfigMcpRegistry>` via blanket impls.
        // The MCP admin endpoint needs `AdminToolRegistry + ToolPolicyAdmin`,
        // satisfied by `GuardedToolRegistry` which layers discovery filtering.
        #[cfg(feature = "mcp")]
        let (mcp_server_inner, mcp_admin_registry) = {
            type DynMcpTable = Arc<
                bitrouter_core::routers::dynamic::DynamicRoutingTable<
                    Arc<bitrouter_providers::mcp::client::registry::ConfigMcpRegistry>,
                >,
            >;
            match mcp_registry.clone() {
                Some(mcp_reg) => {
                    let inner: DynMcpTable = Arc::new(
                        bitrouter_core::routers::dynamic::DynamicRoutingTable::new(mcp_reg),
                    );
                    let admin = Arc::new(bitrouter_core::policy::GuardedToolRegistry::new(
                        Arc::clone(&inner),
                        std::collections::HashMap::new(),
                    ));
                    (Some(inner), Some(admin))
                }
                None => (None, None),
            }
        };
        // ── MCP admin routes ───────────────────────────────────────
        #[cfg(feature = "mcp")]
        let admin_mcp_routes = {
            use bitrouter_api::router::mcp as mcp_api;
            if self.db.is_some() {
                auth::auth_gate(auth::management_auth(auth_ctx.clone()))
                    .and(mcp_api::mcp_admin_filter(mcp_admin_registry))
                    .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                    .boxed()
            } else {
                mcp_api::mcp_admin_filter(mcp_admin_registry)
                    .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                    .boxed()
            }
        };
        #[cfg(not(feature = "mcp"))]
        let admin_mcp_routes = warp::path!("mcp" / "admin" / ..)
            .and_then(|| async { Err::<String, _>(warp::reject::not_found()) })
            .map(|r: String| Box::new(r) as Box<dyn warp::Reply>)
            .boxed();

        // ── MCP server endpoint ────────────────────────────────────
        #[cfg(feature = "mcp")]
        let mcp_server = {
            use bitrouter_api::router::mcp as mcp_api;
            let tool_observer: Arc<dyn ToolObserveCallback> = observe.observer.clone();
            mcp_api::mcp_server_filter_with_observe(
                mcp_server_inner,
                tool_call_handler,
                tool_observer,
                account_filter.clone(),
                self.policy_resolver.clone(),
            )
            .map(|r| Box::new(r) as Box<dyn warp::Reply>)
            .boxed()
        };
        #[cfg(not(feature = "mcp"))]
        let mcp_server = warp::path!("mcp" / ..)
            .and_then(|| async { Err::<String, _>(warp::reject::not_found()) })
            .map(|r: String| Box::new(r) as Box<dyn warp::Reply>)
            .boxed();

        // ── Tool listing (GET /v1/tools) ────────────────────────────
        // Config is authoritative — DynamicToolRegistry<ConfigToolRoutingTable>
        // is the primary source. Skills are additive when present.
        let tool_list = {
            use bitrouter_api::router::tools;
            use bitrouter_core::routers::registry::{ToolEntry, ToolRegistry};
            type RouteFilter = warp::filters::BoxedFilter<(Box<dyn warp::Reply>,)>;

            let filter: RouteFilter = if has_skill_tools {
                struct MergedToolRegistry<A, B> {
                    primary: A,
                    secondary: B,
                }
                impl<A: ToolRegistry, B: ToolRegistry> ToolRegistry for MergedToolRegistry<A, B> {
                    async fn list_tools(&self) -> Vec<ToolEntry> {
                        let mut tools = self.primary.list_tools().await;
                        tools.extend(self.secondary.list_tools().await);
                        tools
                    }
                }
                let merged = Arc::new(MergedToolRegistry {
                    primary: dynamic_tool_registry.clone(),
                    secondary: skills.registry.clone(),
                });
                tools::tools_filter(Some(merged))
                    .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                    .boxed()
            } else {
                tools::tools_filter(Some(dynamic_tool_registry.clone()))
                    .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                    .boxed()
            };
            filter
        };

        // ── Base route tree (always present) ─────────────────────────
        let base = health
            .or(metrics_route)
            .or(route_list)
            .or(model_list)
            .or(agent_list)
            .or(admin_routes)
            .or(admin_key_revoke)
            .or(chat)
            .or(messages)
            .or(responses)
            .or(generate_content);

        // ── Compose all routes ─────────────────────────────────────────
        let all_routes = base
            .or(admin_mcp_routes)
            .or(tool_list)
            .or(skills.skills_list)
            .or(mcp_server)
            // Bridge routes come after the aggregated MCP filter so that the static
            // paths POST /mcp and GET /mcp/sse are matched first.
            .or(mcp_bridge_routes);

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

            // Pre-check that the address is available (warp's bind panics on failure).
            check_addr_available(addr)?;
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

            // Pre-check that the address is available (warp's bind panics on failure).
            check_addr_available(addr)?;
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

/// Pre-check that a socket address is available before handing it to warp
/// (whose `bind` panics on failure).
fn check_addr_available(addr: std::net::SocketAddr) -> Result<()> {
    let _listener = std::net::TcpListener::bind(addr)?;
    // The listener drops here, freeing the port for warp to claim.
    Ok(())
}

/// Map an authenticated [`Identity`] to the transport-neutral [`CallerContext`].
fn identity_to_caller_context(id: bitrouter_accounts::identity::Identity) -> CallerContext {
    CallerContext {
        account_id: Some(id.account_id.0.to_string()),
        key_id: id.key_id,
        models: id.models,
        budget: id.budget,
        budget_scope: id.budget_scope,
        issued_at: id.issued_at,
        key: id.key,
        chain: id.chain,
        policy_id: id.policy_id,
    }
}

/// Resolve the operator's CAIP-10 identity from wallet config.
///
/// Loads the OWS wallet metadata and finds the Solana account address to
/// construct the full CAIP-10 identity. This identity is used as the single
/// trust root for JWT verification — only JWTs with `iss` matching this
/// identity are accepted.
fn resolve_operator_caip10(
    wallet: &bitrouter_config::config::WalletConfig,
) -> std::result::Result<String, String> {
    use bitrouter_core::auth::chain::{Caip10, Chain};

    let vault = wallet.vault_path.as_deref().map(std::path::Path::new);
    let info = ows_lib::get_wallet(&wallet.name, vault)
        .map_err(|e| format!("failed to load wallet '{}': {e}", wallet.name))?;

    let sol_account = info
        .accounts
        .iter()
        .find(|a| a.chain_id.starts_with("solana:"))
        .ok_or_else(|| format!("wallet '{}' has no Solana account", wallet.name))?;

    let caip10 = Caip10 {
        chain: Chain::solana_mainnet(),
        address: sol_account.address.clone(),
    };

    Ok(caip10.format())
}

// ── Admin key revocation endpoint ─────────────────────────────────

/// Request body for `POST /admin/keys/revoke`.
#[derive(serde::Deserialize)]
struct KeyRevokeRequest {
    /// The API key `id` to revoke (base64url-encoded, 43 chars).
    id: String,
}

/// Handle key revocation requests.
async fn handle_key_revoke(
    body: KeyRevokeRequest,
    revocation_set: Option<Arc<dyn bitrouter_core::auth::revocation::KeyRevocationSet>>,
) -> std::result::Result<impl warp::Reply, warp::Rejection> {
    let Some(revocation_set) = revocation_set else {
        return Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({
                "error": { "message": "revocation not configured" }
            })),
            warp::http::StatusCode::SERVICE_UNAVAILABLE,
        ));
    };

    revocation_set.revoke(&body.id).await;
    tracing::info!(key_id = %body.id, "API key revoked");

    Ok(warp::reply::with_status(
        warp::reply::json(&serde_json::json!({
            "revoked": body.id
        })),
        warp::http::StatusCode::OK,
    ))
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

    if let Some(e) = rejection.find::<crate::runtime::budget::BudgetExhausted>() {
        let json = warp::reply::json(&serde_json::json!({
            "error": {
                "message": e.to_string(),
                "type": "budget_exhausted",
            }
        }));
        return Ok(Box::new(warp::reply::with_status(
            json,
            warp::http::StatusCode::TOO_MANY_REQUESTS,
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
