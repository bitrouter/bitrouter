use std::sync::Arc;

use bitrouter_api::router::{anthropic, openai};
use bitrouter_config::BitrouterConfig;
use bitrouter_core::routers::{model_router::LanguageModelRouter, routing_table::RoutingTable};
use warp::Filter;

use crate::error::Result;

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
        }
    }

    pub async fn serve(self) -> Result<()> {
        let addr = self.config.server.listen;

        let health = warp::path("health")
            .and(warp::get())
            .map(|| warp::reply::json(&serde_json::json!({ "status": "ok" })));

        let chat =
            openai::chat::filters::chat_completions_filter(self.table.clone(), self.router.clone());
        let messages =
            anthropic::messages::filters::messages_filter(self.table.clone(), self.router.clone());
        let responses =
            openai::responses::filters::responses_filter(self.table.clone(), self.router.clone());

        let routes = health
            .or(chat)
            .or(messages)
            .or(responses)
            .recover(openai::chat::filters::rejection_handler)
            .with(warp::trace::request());

        let server = warp::serve(routes)
            .bind(addr)
            .await
            .graceful(shutdown_signal());

        tracing::info!(%addr, "server listening");
        server.run().await;
        tracing::info!("server stopped");

        Ok(())
    }
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
