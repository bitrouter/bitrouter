use std::sync::Arc;

use bitrouter_api::router::{anthropic, openai};
use bitrouter_config::BitrouterConfig;
use bitrouter_core::routers::{model_router::LanguageModelRouter, routing_table::RoutingTable};
use sea_orm::DatabaseConnection;
use warp::Filter;

use crate::auth::{self, AuthContext, Unauthorized};
use crate::error::Result;
use crate::keys;

/// A stub model router that rejects all requests with a descriptive error.
///
/// Used when the server starts without a real provider-backed router. Health
/// checks and other non-model endpoints still work; only model API requests
/// will return an error.
pub struct StubModelRouter;

impl LanguageModelRouter for StubModelRouter {
    async fn route_model(
        &self,
        _target: bitrouter_core::routers::routing_table::RoutingTarget,
    ) -> bitrouter_core::errors::Result<
        Box<bitrouter_core::models::language::language_model::DynLanguageModel<'static>>,
    > {
        Err(bitrouter_core::errors::BitrouterError::unsupported(
            "runtime",
            "model routing",
            Some("no model router configured — configure providers to enable API endpoints".into()),
        ))
    }
}

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

    /// Set the database connection for virtual key lookups and key management.
    pub fn with_db(mut self, db: DatabaseConnection) -> Self {
        self.db = Some(Arc::new(db));
        self
    }

    pub async fn serve(self) -> Result<()> {
        let addr = self.config.server.listen;

        // Build auth context.
        let auth_ctx = Arc::new(AuthContext::new(
            self.config.master_key.as_deref(),
            self.db.as_ref().map(|db| db.as_ref().clone()),
        ));

        let health = warp::path("health")
            .and(warp::get())
            .map(|| warp::reply::json(&serde_json::json!({ "status": "ok" })));

        // Model API routes — gated by protocol-appropriate auth.
        let chat = auth_gate(auth::openai_auth(auth_ctx.clone())).and(
            openai::chat::filters::chat_completions_filter(self.table.clone(), self.router.clone()),
        );
        let messages = auth_gate(auth::anthropic_auth(auth_ctx.clone())).and(
            anthropic::messages::filters::messages_filter(self.table.clone(), self.router.clone()),
        );
        let responses = auth_gate(auth::openai_auth(auth_ctx.clone())).and(
            openai::responses::filters::responses_filter(self.table.clone(), self.router.clone()),
        );

        // Key management routes — always mounted (returns 404 if no DB, since
        // the filter will not match without the DB anyway).
        let key_mgmt = keys::key_routes(auth_ctx.clone(), self.db.clone());

        let routes = health
            .or(chat)
            .or(messages)
            .or(responses)
            .or(key_mgmt)
            .recover(handle_auth_rejection)
            .with(warp::trace::request());

        let server = warp::serve(routes)
            .bind(addr)
            .await
            .graceful(shutdown_signal());

        if auth_ctx.is_open() {
            tracing::info!(%addr, "server listening (auth disabled — no master_key configured)");
        } else {
            tracing::info!(%addr, "server listening (auth enabled)");
        }
        server.run().await;
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
