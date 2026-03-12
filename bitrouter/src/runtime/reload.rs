//! Hot-reloadable wrappers for the routing table and model router.
//!
//! When the `hot-reload` feature is enabled, the server wraps its routing
//! table and model router in [`std::sync::RwLock`] so that configuration
//! can be swapped at runtime without restarting the process or dropping
//! in-flight requests.

use std::future::Future;
use std::sync::{Arc, RwLock};

use bitrouter_config::{BitrouterConfig, ConfigRoutingTable};
use bitrouter_core::{
    errors::Result,
    models::language::language_model::DynLanguageModel,
    routers::{
        model_router::LanguageModelRouter,
        routing_table::{RouteEntry, RoutingTable, RoutingTarget},
    },
};

use crate::runtime::paths::RuntimePaths;
use crate::runtime::router::Router;

// ── Reloadable routing table ─────────────────────────────────────────

/// A [`RoutingTable`] backed by an [`RwLock`] so that the inner
/// [`ConfigRoutingTable`] can be replaced at runtime.
pub struct ReloadableTable {
    inner: RwLock<ConfigRoutingTable>,
}

impl ReloadableTable {
    pub fn new(table: ConfigRoutingTable) -> Self {
        Self {
            inner: RwLock::new(table),
        }
    }

    /// Replace the inner routing table. Acquires a write lock, blocking
    /// readers briefly.
    pub fn swap(&self, new_table: ConfigRoutingTable) {
        // If the lock is poisoned, clear the poison and continue.
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        *guard = new_table;
    }
}

impl RoutingTable for ReloadableTable {
    fn route(
        &self,
        incoming_model_name: &str,
    ) -> impl Future<Output = Result<RoutingTarget>> + Send {
        let result = match self.inner.read() {
            Ok(guard) => guard.resolve(incoming_model_name).map(|r| RoutingTarget {
                provider_name: r.provider_name,
                model_id: r.model_id,
            }),
            Err(poisoned) => {
                poisoned
                    .into_inner()
                    .resolve(incoming_model_name)
                    .map(|r| RoutingTarget {
                        provider_name: r.provider_name,
                        model_id: r.model_id,
                    })
            }
        };
        std::future::ready(result)
    }

    fn list_routes(&self) -> Vec<RouteEntry> {
        match self.inner.read() {
            Ok(guard) => guard.list_routes(),
            Err(poisoned) => poisoned.into_inner().list_routes(),
        }
    }
}

// ── Reloadable model router ─────────────────────────────────────────

/// A [`LanguageModelRouter`] backed by an [`RwLock`] so that the inner
/// [`Router`] can be replaced at runtime when configuration changes.
pub struct ReloadableRouter {
    inner: RwLock<Router>,
    /// Shared HTTP client reused across reloads to preserve connection
    /// pool state.
    client: reqwest::Client,
}

impl ReloadableRouter {
    pub fn new(router: Router) -> Self {
        let client = reqwest::Client::new();
        Self {
            inner: RwLock::new(router),
            client,
        }
    }

    /// The shared HTTP client used by the inner router.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Replace the inner router. Acquires a write lock, blocking readers
    /// briefly.
    pub fn swap(&self, new_router: Router) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        *guard = new_router;
    }
}

impl LanguageModelRouter for ReloadableRouter {
    fn route_model(
        &self,
        target: RoutingTarget,
    ) -> impl Future<Output = Result<Box<DynLanguageModel<'static>>>> + Send {
        let result = match self.inner.read() {
            Ok(guard) => guard.route_model_sync(&target),
            Err(poisoned) => poisoned.into_inner().route_model_sync(&target),
        };
        std::future::ready(result)
    }
}

// ── Reload logic ────────────────────────────────────────────────────

/// Attempt to reload the configuration from disk and swap the routing
/// table and model router.
///
/// Returns `Ok(())` on success, or an error describing what went wrong
/// (e.g. parse failure). On error the old configuration remains active.
pub fn reload_config(
    paths: &RuntimePaths,
    table: &ReloadableTable,
    router: &ReloadableRouter,
) -> std::result::Result<(), String> {
    let env_file = paths.env_file.exists().then_some(paths.env_file.as_path());
    let config = BitrouterConfig::load_from_file(&paths.config_file, env_file)
        .map_err(|e| format!("failed to load config: {e}"))?;

    let new_table = ConfigRoutingTable::new(config.providers.clone(), config.models.clone());
    let new_router = Router::new(router.client().clone(), config.providers);

    table.swap(new_table);
    router.swap(new_router);

    Ok(())
}

/// Listen for reload signals and swap configuration on each signal.
///
/// On Unix this listens for `SIGHUP`. The function runs forever and is
/// intended to be spawned as a background task.
#[cfg(unix)]
pub async fn listen_for_reload(
    paths: RuntimePaths,
    table: Arc<ReloadableTable>,
    router: Arc<ReloadableRouter>,
) {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sighup_stream = match signal(SignalKind::hangup()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("failed to register SIGHUP handler: {e}");
            return;
        }
    };

    while sighup_stream.recv().await.is_some() {
        tracing::info!("SIGHUP received — reloading configuration");
        match reload_config(&paths, &table, &router) {
            Ok(()) => tracing::info!("configuration reloaded successfully"),
            Err(e) => tracing::error!("configuration reload failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_config::ProviderConfig;
    use bitrouter_core::routers::routing_table::RoutingTable;
    use std::collections::HashMap;

    fn sample_table() -> ConfigRoutingTable {
        let mut providers = HashMap::new();
        providers.insert(
            "openai".into(),
            ProviderConfig {
                api_protocol: Some(bitrouter_config::ApiProtocol::Openai),
                ..Default::default()
            },
        );
        ConfigRoutingTable::new(providers, HashMap::new())
    }

    #[tokio::test]
    async fn reloadable_table_routes_through_inner() {
        let table = ReloadableTable::new(sample_table());
        let result = table.route("openai:gpt-4o").await;
        let target = result.unwrap();
        assert_eq!(target.provider_name, "openai");
        assert_eq!(target.model_id, "gpt-4o");
    }

    #[tokio::test]
    async fn reloadable_table_swap_updates_routing() {
        let table = ReloadableTable::new(sample_table());

        // Initially "anthropic" is not a known provider for direct routing.
        assert!(table.route("anthropic:claude-opus-4-6").await.is_err());

        // Swap in a table that knows about anthropic.
        let mut providers = HashMap::new();
        providers.insert(
            "anthropic".into(),
            ProviderConfig {
                api_protocol: Some(bitrouter_config::ApiProtocol::Anthropic),
                ..Default::default()
            },
        );
        table.swap(ConfigRoutingTable::new(providers, HashMap::new()));

        let target = table.route("anthropic:claude-opus-4-6").await.unwrap();
        assert_eq!(target.provider_name, "anthropic");
        assert_eq!(target.model_id, "claude-opus-4-6");
    }

    #[test]
    fn reloadable_table_list_routes_delegates() {
        let table = ReloadableTable::new(sample_table());
        // No model entries → empty list.
        assert!(table.list_routes().is_empty());
    }

    #[tokio::test]
    async fn reloadable_router_routes_through_inner() {
        let mut providers = HashMap::new();
        providers.insert(
            "openai".into(),
            ProviderConfig {
                api_protocol: Some(bitrouter_config::ApiProtocol::Openai),
                api_key: Some("test-key".into()),
                ..Default::default()
            },
        );
        let router = ReloadableRouter::new(Router::new(reqwest::Client::new(), providers));

        let target = RoutingTarget {
            provider_name: "openai".into(),
            model_id: "gpt-4o".into(),
        };
        // Should succeed — the concrete model creation works with any key.
        let result = router.route_model(target).await;
        assert!(result.is_ok());
    }
}
