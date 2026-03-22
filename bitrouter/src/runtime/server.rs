use std::sync::Arc;

use bitrouter_api::router::{admin, anthropic, google, models, openai, routes};
use bitrouter_config::BitrouterConfig;
use bitrouter_core::observe::{CallerContext, ObserveCallback};
use bitrouter_core::routers::admin::AdminRoutingTable;
use bitrouter_core::routers::model_router::LanguageModelRouter;
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
    T: AdminRoutingTable + Send + Sync + 'static,
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
                .map(|id: bitrouter_accounts::identity::Identity| CallerContext {
                    account_id: Some(id.account_id.0.to_string()),
                    models: id.models,
                    budget: id.budget,
                    budget_scope: id.budget_scope,
                    budget_range: id.budget_range,
                })
                .boxed()
        } else {
            warp::any().map(CallerContext::default).boxed()
        };

        let anthropic_account_filter = if self.db.is_some() {
            let auth_filter = auth::anthropic_auth(auth_ctx.clone());
            warp::any()
                .and(auth_filter)
                .map(|id: bitrouter_accounts::identity::Identity| CallerContext {
                    account_id: Some(id.account_id.0.to_string()),
                    models: id.models,
                    budget: id.budget,
                    budget_scope: id.budget_scope,
                    budget_range: id.budget_range,
                })
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
                    let realm = mpp_config.realm.as_deref();
                    let secret_key = mpp_config.secret_key.as_deref();
                    let mut state: Option<bitrouter_api::mpp::MppState> = None;

                    #[cfg(feature = "mpp-tempo")]
                    if let (true, Some(tempo)) =
                        (state.is_none(), mpp_config.networks.tempo.as_ref())
                    {
                        state = Some(
                            bitrouter_api::mpp::MppState::from_tempo_config(
                                tempo, realm, secret_key,
                            )
                            .map_err(|e| {
                                bitrouter_config::ConfigError::ConfigParse(format!(
                                    "MPP initialization failed: {e}"
                                ))
                            })?,
                        );
                    }

                    #[cfg(feature = "mpp-solana")]
                    if let (true, Some(solana)) =
                        (state.is_none(), mpp_config.networks.solana.as_ref())
                    {
                        state = Some(
                            bitrouter_api::mpp::MppState::from_solana_config(
                                solana, realm, secret_key,
                            )
                            .map_err(|e| {
                                bitrouter_config::ConfigError::ConfigParse(format!(
                                    "MPP initialization failed: {e}"
                                ))
                            })?,
                        );
                    }

                    if let Some(s) = state.as_ref() {
                        tracing::info!(realm = s.realm(), "MPP enabled");
                    }
                    state.map(Arc::new)
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
                )
                .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                .boxed(),
                anthropic::messages::filters::messages_filter_with_mpp(
                    self.table.clone(),
                    guarded_router.clone(),
                    observer.clone(),
                    mpp.clone(),
                )
                .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                .boxed(),
                openai::responses::filters::responses_filter_with_mpp(
                    self.table.clone(),
                    guarded_router.clone(),
                    observer.clone(),
                    mpp.clone(),
                )
                .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                .boxed(),
                google::generate_content::filters::generate_content_filter_with_mpp(
                    self.table.clone(),
                    guarded_router.clone(),
                    observer.clone(),
                    mpp.clone(),
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
                    account_filter,
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
                account_filter,
            );

        // Build the full route tree. Account/session management routes are
        // only mounted when a database is configured.
        if let Some(ref db) = self.db {
            let db_conn = db.as_ref().clone();
            let mgmt_auth = auth::management_auth(auth_ctx.clone());
            let acct =
                bitrouter_accounts::filters::account_routes(db_conn.clone(), mgmt_auth.clone());
            let sess = bitrouter_accounts::filters::session_routes(db_conn, mgmt_auth);

            let all = health
                .or(metrics_route)
                .or(route_list)
                .or(model_list)
                .or(admin_routes)
                .or(chat)
                .or(messages)
                .or(responses)
                .or(generate_content)
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
            let all = health
                .or(metrics_route)
                .or(route_list)
                .or(model_list)
                .or(admin_routes)
                .or(chat)
                .or(messages)
                .or(responses)
                .or(generate_content)
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

/// Convert an auth filter into a gate that rejects unauthorized requests
/// but does not add anything to the extract tuple. This lets us compose
/// `auth_gate(auth).and(existing_filter)` without changing the existing
/// filter's handler signature.
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
