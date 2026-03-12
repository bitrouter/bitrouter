use std::sync::Arc;

use bitrouter_api::metrics::MetricsStore;
use bitrouter_api::router::{anthropic, google, openai, routes};
use bitrouter_config::BitrouterConfig;
use bitrouter_core::hooks::HookedRouter;
use bitrouter_core::routers::{model_router::LanguageModelRouter, routing_table::RoutingTable};
use bitrouter_guardrails::{GuardedRouter, Guardrail};
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
    T: RoutingTable + Send + Sync + 'static,
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

        // Wrap with generation hooks for side-effect observation (logging,
        // token tracking, auditing). Hooks are invoked on borrowed core types
        // after generate() and for each streaming part.
        let hooks: Arc<[Arc<dyn bitrouter_core::hooks::GenerationHook>]> = Arc::from(Vec::new());
        let hooked_router = Arc::new(HookedRouter::new(guarded_router, hooks));

        if guardrail.is_disabled() {
            tracing::info!("guardrails disabled");
        } else {
            tracing::info!("guardrails enabled");
        }

        // Build JWT auth context.
        let auth_ctx = Arc::new(JwtAuthContext::new(
            self.db.as_ref().map(|db| db.as_ref().clone()),
        ));

        // Shared in-memory metrics store.
        let metrics = Arc::new(MetricsStore::new());

        let health = warp::path("health")
            .and(warp::get())
            .map(|| warp::reply::json(&serde_json::json!({ "status": "ok" })));

        // Route listing — no auth required.
        let route_list = routes::routes_filter(self.table.clone());

        // Metrics endpoint — no auth required.
        let metrics_endpoint = bitrouter_api::router::metrics::metrics_filter(metrics.clone());

        // Model API routes — gated by protocol-appropriate auth.
        // All routes use the guarded router for guardrail enforcement.
        let chat = auth_gate(auth::openai_auth(auth_ctx.clone())).and(
            openai::chat::filters::chat_completions_filter(
                self.table.clone(),
                hooked_router.clone(),
                metrics.clone(),
            ),
        );
        let messages = auth_gate(auth::anthropic_auth(auth_ctx.clone())).and(
            anthropic::messages::filters::messages_filter(
                self.table.clone(),
                hooked_router.clone(),
                metrics.clone(),
            ),
        );
        let responses = auth_gate(auth::openai_auth(auth_ctx.clone())).and(
            openai::responses::filters::responses_filter(
                self.table.clone(),
                hooked_router.clone(),
                metrics.clone(),
            ),
        );
        let generate_content = auth_gate(auth::openai_auth(auth_ctx.clone())).and(
            google::generate_content::filters::generate_content_filter(
                self.table.clone(),
                hooked_router.clone(),
                metrics.clone(),
            ),
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
                .or(route_list)
                .or(metrics_endpoint)
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
                .or(route_list)
                .or(metrics_endpoint)
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
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
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
