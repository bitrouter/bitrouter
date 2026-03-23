//! A routing table wrapper that adds dynamic route management.
//!
//! [`DynamicRoutingTable`] wraps any [`RoutingTable`] and layers an in-memory
//! set of dynamic routes on top. Dynamic routes take precedence over the inner
//! table during resolution. All mutations are protected by a [`RwLock`].
//!
//! Dynamic routes are ephemeral — they are lost when the process exits.

use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::errors::{BitrouterError, Result};

use super::admin::{AdminRoutingTable, DynamicRoute, RouteEndpoint, RouteStrategy};
use super::registry::{ModelEntry, ModelRegistry};
use super::routing_table::{RouteEntry, RoutingTable, RoutingTarget};

/// Internal representation of a dynamic route with its round-robin counter.
struct DynamicRouteData {
    strategy: RouteStrategy,
    endpoints: Vec<RouteEndpoint>,
    counter: AtomicUsize,
}

/// A routing table wrapper that adds dynamic route management.
///
/// Wraps any `T: RoutingTable` and layers an in-memory set of dynamic routes
/// on top. Dynamic routes take precedence during resolution.
pub struct DynamicRoutingTable<T> {
    inner: T,
    routes: RwLock<HashMap<String, DynamicRouteData>>,
}

impl<T> DynamicRoutingTable<T> {
    /// Create a new dynamic routing table wrapping the given inner table.
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            routes: RwLock::new(HashMap::new()),
        }
    }

    /// Returns a reference to the wrapped inner routing table.
    pub fn inner(&self) -> &T {
        &self.inner
    }

    /// Resolve a model name against dynamic routes only.
    ///
    /// Returns `None` if no dynamic route matches.
    fn resolve_dynamic(&self, model: &str) -> Option<RoutingTarget> {
        let routes = self.routes.read().ok()?;
        let data = routes.get(model)?;

        if data.endpoints.is_empty() {
            return None;
        }

        let endpoint = match data.strategy {
            RouteStrategy::Priority => &data.endpoints[0],
            RouteStrategy::LoadBalance => {
                let idx = data.counter.fetch_add(1, Ordering::Relaxed) % data.endpoints.len();
                &data.endpoints[idx]
            }
        };

        Some(RoutingTarget {
            provider_name: endpoint.provider.clone(),
            model_id: endpoint.model_id.clone(),
        })
    }
}

impl<T: RoutingTable + Sync> RoutingTable for DynamicRoutingTable<T> {
    async fn route(&self, incoming_model_name: &str) -> Result<RoutingTarget> {
        // Dynamic routes take precedence.
        if let Some(target) = self.resolve_dynamic(incoming_model_name) {
            return Ok(target);
        }
        // Fall back to the inner table.
        self.inner.route(incoming_model_name).await
    }

    fn list_routes(&self) -> Vec<RouteEntry> {
        let mut entries = self.inner.list_routes();

        if let Ok(routes) = self.routes.read() {
            // Remove config entries that are shadowed by dynamic routes.
            entries.retain(|e| !routes.contains_key(&e.model));

            // Append dynamic route entries.
            for (model, data) in routes.iter() {
                if let Some(ep) = data.endpoints.first() {
                    entries.push(RouteEntry {
                        model: model.clone(),
                        provider: ep.provider.clone(),
                        // Dynamic routes don't track protocol; default to provider name.
                        protocol: ep.provider.clone(),
                    });
                }
            }
        }

        entries.sort_by(|a, b| a.model.cmp(&b.model));
        entries
    }
}

impl<T: ModelRegistry> ModelRegistry for DynamicRoutingTable<T> {
    fn list_models(&self) -> Vec<ModelEntry> {
        self.inner.list_models()
    }
}

impl<T: RoutingTable + Sync> AdminRoutingTable for DynamicRoutingTable<T> {
    fn add_route(&self, route: DynamicRoute) -> Result<()> {
        if route.endpoints.is_empty() {
            return Err(BitrouterError::invalid_request(
                None,
                "route must have at least one endpoint".to_owned(),
                None,
            ));
        }

        let data = DynamicRouteData {
            strategy: route.strategy,
            endpoints: route.endpoints,
            counter: AtomicUsize::new(0),
        };

        let mut routes = self
            .routes
            .write()
            .map_err(|_| BitrouterError::transport(None, "routing table lock poisoned"))?;
        routes.insert(route.model, data);
        Ok(())
    }

    fn remove_route(&self, model: &str) -> Result<bool> {
        let mut routes = self
            .routes
            .write()
            .map_err(|_| BitrouterError::transport(None, "routing table lock poisoned"))?;
        Ok(routes.remove(model).is_some())
    }

    fn list_dynamic_routes(&self) -> Vec<DynamicRoute> {
        let routes = match self.routes.read() {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut result: Vec<DynamicRoute> = routes
            .iter()
            .map(|(model, data)| DynamicRoute {
                model: model.clone(),
                strategy: data.strategy.clone(),
                endpoints: data.endpoints.clone(),
            })
            .collect();
        result.sort_by(|a, b| a.model.cmp(&b.model));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StaticTable;

    impl RoutingTable for StaticTable {
        async fn route(&self, incoming: &str) -> Result<RoutingTarget> {
            if incoming == "default" {
                Ok(RoutingTarget {
                    provider_name: "openai".to_owned(),
                    model_id: "gpt-4o".to_owned(),
                })
            } else {
                Err(BitrouterError::invalid_request(
                    None,
                    format!("no route: {incoming}"),
                    None,
                ))
            }
        }

        fn list_routes(&self) -> Vec<RouteEntry> {
            vec![RouteEntry {
                model: "default".to_owned(),
                provider: "openai".to_owned(),
                protocol: "openai".to_owned(),
            }]
        }
    }

    /// Helper to call the trait method with explicit type annotation.
    async fn route(table: &DynamicRoutingTable<StaticTable>, model: &str) -> Result<RoutingTarget> {
        <DynamicRoutingTable<StaticTable> as RoutingTable>::route(table, model).await
    }

    #[tokio::test]
    async fn dynamic_route_takes_precedence() {
        let table = DynamicRoutingTable::new(StaticTable);
        table
            .add_route(DynamicRoute {
                model: "default".to_owned(),
                strategy: RouteStrategy::Priority,
                endpoints: vec![RouteEndpoint {
                    provider: "anthropic".to_owned(),
                    model_id: "claude-sonnet-4-20250514".to_owned(),
                }],
            })
            .ok();

        let target = route(&table, "default").await.ok();
        assert!(target.is_some());
        let target = target.unwrap();
        assert_eq!(target.provider_name, "anthropic");
        assert_eq!(target.model_id, "claude-sonnet-4-20250514");
    }

    #[tokio::test]
    async fn falls_back_to_inner_table() {
        let table = DynamicRoutingTable::new(StaticTable);

        let target = route(&table, "default").await.ok();
        assert!(target.is_some());
        let target = target.unwrap();
        assert_eq!(target.provider_name, "openai");
        assert_eq!(target.model_id, "gpt-4o");
    }

    #[tokio::test]
    async fn add_and_remove_dynamic_route() {
        let table = DynamicRoutingTable::new(StaticTable);

        table
            .add_route(DynamicRoute {
                model: "research".to_owned(),
                strategy: RouteStrategy::Priority,
                endpoints: vec![RouteEndpoint {
                    provider: "openai".to_owned(),
                    model_id: "o1".to_owned(),
                }],
            })
            .ok();

        assert!(route(&table, "research").await.is_ok());
        assert_eq!(table.list_dynamic_routes().len(), 1);

        let removed = table.remove_route("research").ok();
        assert_eq!(removed, Some(true));
        assert!(route(&table, "research").await.is_err());
        assert!(table.list_dynamic_routes().is_empty());
    }

    #[test]
    fn remove_nonexistent_returns_false() {
        let table = DynamicRoutingTable::new(StaticTable);
        let removed = table.remove_route("nope").ok();
        assert_eq!(removed, Some(false));
    }

    #[test]
    fn add_route_with_no_endpoints_fails() {
        let table = DynamicRoutingTable::new(StaticTable);
        let result = table.add_route(DynamicRoute {
            model: "empty".to_owned(),
            strategy: RouteStrategy::Priority,
            endpoints: vec![],
        });
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn load_balance_round_robin() {
        let table = DynamicRoutingTable::new(StaticTable);
        table
            .add_route(DynamicRoute {
                model: "balanced".to_owned(),
                strategy: RouteStrategy::LoadBalance,
                endpoints: vec![
                    RouteEndpoint {
                        provider: "openai".to_owned(),
                        model_id: "gpt-4o".to_owned(),
                    },
                    RouteEndpoint {
                        provider: "anthropic".to_owned(),
                        model_id: "claude-sonnet-4-20250514".to_owned(),
                    },
                ],
            })
            .ok();

        let t1 = route(&table, "balanced").await.ok().unwrap();
        let t2 = route(&table, "balanced").await.ok().unwrap();
        let t3 = route(&table, "balanced").await.ok().unwrap();

        assert_eq!(t1.provider_name, "openai");
        assert_eq!(t2.provider_name, "anthropic");
        assert_eq!(t3.provider_name, "openai"); // wraps around
    }

    #[test]
    fn list_routes_includes_dynamic() {
        let table = DynamicRoutingTable::new(StaticTable);
        table
            .add_route(DynamicRoute {
                model: "custom".to_owned(),
                strategy: RouteStrategy::Priority,
                endpoints: vec![RouteEndpoint {
                    provider: "anthropic".to_owned(),
                    model_id: "claude-sonnet-4-20250514".to_owned(),
                }],
            })
            .ok();

        let routes = table.list_routes();
        assert!(routes.iter().any(|r| r.model == "custom"));
        assert!(routes.iter().any(|r| r.model == "default"));
    }

    #[test]
    fn dynamic_route_shadows_config_in_list() {
        let table = DynamicRoutingTable::new(StaticTable);
        table
            .add_route(DynamicRoute {
                model: "default".to_owned(),
                strategy: RouteStrategy::Priority,
                endpoints: vec![RouteEndpoint {
                    provider: "anthropic".to_owned(),
                    model_id: "claude-sonnet-4-20250514".to_owned(),
                }],
            })
            .ok();

        let routes = table.list_routes();
        let defaults: Vec<_> = routes.iter().filter(|r| r.model == "default").collect();
        assert_eq!(defaults.len(), 1);
        assert_eq!(defaults[0].provider, "anthropic");
    }
}
