//! A routing table wrapper that adds dynamic route management.
//!
//! [`DynamicRoutingTable`] wraps any [`RoutingTable`] and layers an in-memory
//! set of dynamic routes on top. Dynamic routes take precedence over the inner
//! table during resolution. All mutations are protected by a [`RwLock`].
//!
//! Dynamic routes are ephemeral — they are lost when the process exits.
//! The inner table can be hot-reloaded via [`ReloadableRoutingTable::reload`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use crate::errors::{BitrouterError, Result};

use super::admin::{
    AdminRoutingTable, AdminToolRegistry, DynamicRoute, RouteEndpoint, RouteKind, RouteStrategy,
    ToolUpstreamEntry,
};
use super::registry::{ModelEntry, ModelRegistry, ToolEntry, ToolRegistry};
use super::reload::ReloadableRoutingTable;
use super::routing_table::{
    ApiProtocol, RouteEntry, RoutingTable, RoutingTarget, strip_ansi_escapes,
};
use crate::routers::content::RouteContext;

/// Internal representation of a dynamic route with its round-robin counter.
struct DynamicRouteData {
    kind: RouteKind,
    strategy: RouteStrategy,
    endpoints: Vec<RouteEndpoint>,
    counter: AtomicUsize,
}

/// A routing table wrapper that adds dynamic route management.
///
/// Wraps any `T: RoutingTable` and layers an in-memory set of dynamic routes
/// on top. Dynamic routes take precedence during resolution.
///
/// The inner table is stored behind an [`Arc`] + [`RwLock`] so it can be
/// replaced at runtime via the [`ReloadableRoutingTable`] trait — for example,
/// when the configuration file is hot-reloaded.
pub struct DynamicRoutingTable<T> {
    inner: RwLock<Arc<T>>,
    routes: RwLock<HashMap<String, DynamicRouteData>>,
}

impl<T> DynamicRoutingTable<T> {
    /// Create a new dynamic routing table wrapping the given inner table.
    pub fn new(inner: T) -> Self {
        Self {
            inner: RwLock::new(Arc::new(inner)),
            routes: RwLock::new(HashMap::new()),
        }
    }

    /// Returns an `Arc` snapshot of the current inner routing table.
    ///
    /// The returned `Arc` is cheaply cloned and does not hold any lock,
    /// so callers may keep it for as long as needed.
    pub fn read_inner(&self) -> Arc<T> {
        self.inner
            .read()
            .map(|guard| Arc::clone(&guard))
            .unwrap_or_else(|poisoned| Arc::clone(&poisoned.into_inner()))
    }

    /// Resolve a name against dynamic routes only.
    ///
    /// Returns `None` if no dynamic route matches.
    fn resolve_dynamic(&self, name: &str) -> Option<RoutingTarget> {
        let routes = self.routes.read().ok()?;
        let data = routes.get(name)?;

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
            service_id: strip_ansi_escapes(&endpoint.service_id),
            api_protocol: endpoint.api_protocol.unwrap_or(ApiProtocol::Openai),
        })
    }
}

impl<T: RoutingTable + Send + Sync> RoutingTable for DynamicRoutingTable<T> {
    async fn route(&self, incoming_name: &str, context: &RouteContext) -> Result<RoutingTarget> {
        // Dynamic routes take precedence.
        if let Some(target) = self.resolve_dynamic(incoming_name) {
            return Ok(target);
        }
        // Clone the Arc and drop the lock before the async call.
        let inner = self.read_inner();
        inner.route(incoming_name, context).await
    }

    fn list_routes(&self) -> Vec<RouteEntry> {
        let mut entries = self
            .inner
            .read()
            .map(|inner| inner.list_routes())
            .unwrap_or_default();

        if let Ok(routes) = self.routes.read() {
            // Remove config entries that are shadowed by dynamic routes.
            entries.retain(|e| !routes.contains_key(&e.name));

            // Append dynamic route entries.
            for (name, data) in routes.iter() {
                if let Some(ep) = data.endpoints.first() {
                    entries.push(RouteEntry {
                        name: name.clone(),
                        provider: ep.provider.clone(),
                        protocol: ep.api_protocol.unwrap_or(ApiProtocol::Openai),
                    });
                }
            }
        }

        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }
}

impl<T: ModelRegistry> ModelRegistry for DynamicRoutingTable<T> {
    fn list_models(&self) -> Vec<ModelEntry> {
        self.inner
            .read()
            .map(|inner| inner.list_models())
            .unwrap_or_default()
    }
}

impl<T: ToolRegistry> ToolRegistry for DynamicRoutingTable<T> {
    async fn list_tools(&self) -> Vec<ToolEntry> {
        self.read_inner().list_tools().await
    }
}

impl<T: ToolRegistry + Send + Sync> AdminToolRegistry for DynamicRoutingTable<T> {
    async fn list_upstreams(&self) -> Vec<ToolUpstreamEntry> {
        let tools = self.read_inner().list_tools().await;
        let mut counts: HashMap<String, usize> = HashMap::new();
        for tool in &tools {
            *counts.entry(tool.server().to_owned()).or_default() += 1;
        }

        let mut entries: Vec<ToolUpstreamEntry> = counts
            .into_iter()
            .map(|(name, tool_count)| ToolUpstreamEntry {
                name,
                tool_count,
                filter: None,
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }
}

impl<T: RoutingTable + Send + Sync> AdminRoutingTable for DynamicRoutingTable<T> {
    fn add_route(&self, route: DynamicRoute) -> Result<()> {
        if route.endpoints.is_empty() {
            return Err(BitrouterError::invalid_request(
                None,
                "route must have at least one endpoint".to_owned(),
                None,
            ));
        }

        let data = DynamicRouteData {
            kind: route.kind,
            strategy: route.strategy,
            endpoints: route.endpoints,
            counter: AtomicUsize::new(0),
        };

        let mut routes = self
            .routes
            .write()
            .map_err(|_| BitrouterError::transport(None, "routing table lock poisoned"))?;
        routes.insert(route.name, data);
        Ok(())
    }

    fn remove_route(&self, name: &str) -> Result<bool> {
        let mut routes = self
            .routes
            .write()
            .map_err(|_| BitrouterError::transport(None, "routing table lock poisoned"))?;
        Ok(routes.remove(name).is_some())
    }

    fn list_dynamic_routes(&self) -> Vec<DynamicRoute> {
        let routes = match self.routes.read() {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut result: Vec<DynamicRoute> = routes
            .iter()
            .map(|(name, data)| DynamicRoute {
                name: name.clone(),
                kind: data.kind.clone(),
                strategy: data.strategy.clone(),
                endpoints: data.endpoints.clone(),
            })
            .collect();
        result.sort_by(|a, b| a.name.cmp(&b.name));
        result
    }
}

impl<T> ReloadableRoutingTable for DynamicRoutingTable<T> {
    type Inner = T;

    fn reload(&self, new_inner: T) -> Result<()> {
        let mut inner = self
            .inner
            .write()
            .map_err(|_| BitrouterError::transport(None, "inner routing table lock poisoned"))?;
        *inner = Arc::new(new_inner);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routers::admin::RouteKind;

    struct StaticTable;

    impl RoutingTable for StaticTable {
        async fn route(&self, incoming: &str, _context: &RouteContext) -> Result<RoutingTarget> {
            if incoming == "default" {
                Ok(RoutingTarget {
                    provider_name: "openai".to_owned(),
                    service_id: "gpt-4o".to_owned(),
                    api_protocol: ApiProtocol::Openai,
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
                name: "default".to_owned(),
                provider: "openai".to_owned(),
                protocol: ApiProtocol::Openai,
            }]
        }
    }

    /// Helper to call the trait method with explicit type annotation.
    async fn route(table: &DynamicRoutingTable<StaticTable>, name: &str) -> Result<RoutingTarget> {
        <DynamicRoutingTable<StaticTable> as RoutingTable>::route(
            table,
            name,
            &RouteContext::default(),
        )
        .await
    }

    #[tokio::test]
    async fn dynamic_route_takes_precedence() {
        let table = DynamicRoutingTable::new(StaticTable);
        table
            .add_route(DynamicRoute {
                name: "default".to_owned(),
                kind: RouteKind::Model,
                strategy: RouteStrategy::Priority,
                endpoints: vec![RouteEndpoint {
                    provider: "anthropic".to_owned(),
                    service_id: "claude-sonnet-4-20250514".to_owned(),
                    api_protocol: Some(ApiProtocol::Anthropic),
                }],
            })
            .ok();

        let target = route(&table, "default").await.ok();
        assert!(target.is_some());
        let target = target.unwrap();
        assert_eq!(target.provider_name, "anthropic");
        assert_eq!(target.service_id, "claude-sonnet-4-20250514");
        assert_eq!(target.api_protocol, ApiProtocol::Anthropic);
    }

    #[tokio::test]
    async fn falls_back_to_inner_table() {
        let table = DynamicRoutingTable::new(StaticTable);

        let target = route(&table, "default").await.ok();
        assert!(target.is_some());
        let target = target.unwrap();
        assert_eq!(target.provider_name, "openai");
        assert_eq!(target.service_id, "gpt-4o");
    }

    #[tokio::test]
    async fn add_and_remove_dynamic_route() {
        let table = DynamicRoutingTable::new(StaticTable);

        table
            .add_route(DynamicRoute {
                name: "research".to_owned(),
                kind: RouteKind::Model,
                strategy: RouteStrategy::Priority,
                endpoints: vec![RouteEndpoint {
                    provider: "openai".to_owned(),
                    service_id: "o1".to_owned(),
                    api_protocol: None,
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
            name: "empty".to_owned(),
            kind: RouteKind::Model,
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
                name: "balanced".to_owned(),
                kind: RouteKind::Model,
                strategy: RouteStrategy::LoadBalance,
                endpoints: vec![
                    RouteEndpoint {
                        provider: "openai".to_owned(),
                        service_id: "gpt-4o".to_owned(),
                        api_protocol: None,
                    },
                    RouteEndpoint {
                        provider: "anthropic".to_owned(),
                        service_id: "claude-sonnet-4-20250514".to_owned(),
                        api_protocol: None,
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
                name: "custom".to_owned(),
                kind: RouteKind::Model,
                strategy: RouteStrategy::Priority,
                endpoints: vec![RouteEndpoint {
                    provider: "anthropic".to_owned(),
                    service_id: "claude-sonnet-4-20250514".to_owned(),
                    api_protocol: None,
                }],
            })
            .ok();

        let routes = table.list_routes();
        assert!(routes.iter().any(|r| r.name == "custom"));
        assert!(routes.iter().any(|r| r.name == "default"));
    }

    #[test]
    fn dynamic_route_shadows_config_in_list() {
        let table = DynamicRoutingTable::new(StaticTable);
        table
            .add_route(DynamicRoute {
                name: "default".to_owned(),
                kind: RouteKind::Model,
                strategy: RouteStrategy::Priority,
                endpoints: vec![RouteEndpoint {
                    provider: "anthropic".to_owned(),
                    service_id: "claude-sonnet-4-20250514".to_owned(),
                    api_protocol: None,
                }],
            })
            .ok();

        let routes = table.list_routes();
        let defaults: Vec<_> = routes.iter().filter(|r| r.name == "default").collect();
        assert_eq!(defaults.len(), 1);
        assert_eq!(defaults[0].provider, "anthropic");
    }

    #[tokio::test]
    async fn reload_replaces_inner_table() {
        struct FlexTable {
            use_anthropic: bool,
        }

        impl RoutingTable for FlexTable {
            async fn route(
                &self,
                incoming: &str,
                _context: &RouteContext,
            ) -> Result<RoutingTarget> {
                if incoming == "default" {
                    if self.use_anthropic {
                        Ok(RoutingTarget {
                            provider_name: "anthropic".to_owned(),
                            service_id: "claude-sonnet-4-20250514".to_owned(),
                            api_protocol: ApiProtocol::Anthropic,
                        })
                    } else {
                        Ok(RoutingTarget {
                            provider_name: "openai".to_owned(),
                            service_id: "gpt-4o".to_owned(),
                            api_protocol: ApiProtocol::Openai,
                        })
                    }
                } else {
                    Err(BitrouterError::invalid_request(
                        None,
                        format!("no route: {incoming}"),
                        None,
                    ))
                }
            }
        }

        async fn flex_route(t: &DynamicRoutingTable<FlexTable>, m: &str) -> Result<RoutingTarget> {
            <DynamicRoutingTable<FlexTable> as RoutingTable>::route(t, m, &RouteContext::default())
                .await
        }

        let table = DynamicRoutingTable::new(FlexTable {
            use_anthropic: false,
        });

        // Before reload: routes to openai
        let target = flex_route(&table, "default").await.unwrap();
        assert_eq!(target.provider_name, "openai");

        // Reload with anthropic config
        table
            .reload(FlexTable {
                use_anthropic: true,
            })
            .unwrap();

        // After reload: routes to anthropic
        let target = flex_route(&table, "default").await.unwrap();
        assert_eq!(target.provider_name, "anthropic");
    }

    #[tokio::test]
    async fn reload_preserves_dynamic_routes() {
        struct FlexTable {
            use_anthropic: bool,
        }

        impl RoutingTable for FlexTable {
            async fn route(
                &self,
                incoming: &str,
                _context: &RouteContext,
            ) -> Result<RoutingTarget> {
                if incoming == "default" {
                    if self.use_anthropic {
                        Ok(RoutingTarget {
                            provider_name: "anthropic".to_owned(),
                            service_id: "claude-sonnet-4-20250514".to_owned(),
                            api_protocol: ApiProtocol::Anthropic,
                        })
                    } else {
                        Ok(RoutingTarget {
                            provider_name: "openai".to_owned(),
                            service_id: "gpt-4o".to_owned(),
                            api_protocol: ApiProtocol::Openai,
                        })
                    }
                } else {
                    Err(BitrouterError::invalid_request(
                        None,
                        format!("no route: {incoming}"),
                        None,
                    ))
                }
            }
        }

        async fn flex_route(t: &DynamicRoutingTable<FlexTable>, m: &str) -> Result<RoutingTarget> {
            <DynamicRoutingTable<FlexTable> as RoutingTable>::route(t, m, &RouteContext::default())
                .await
        }

        let table = DynamicRoutingTable::new(FlexTable {
            use_anthropic: false,
        });

        // Add a dynamic route
        table
            .add_route(DynamicRoute {
                name: "research".to_owned(),
                kind: RouteKind::Model,
                strategy: RouteStrategy::Priority,
                endpoints: vec![RouteEndpoint {
                    provider: "openai".to_owned(),
                    service_id: "o1".to_owned(),
                    api_protocol: None,
                }],
            })
            .unwrap();

        // Reload inner table
        table
            .reload(FlexTable {
                use_anthropic: true,
            })
            .unwrap();

        // Dynamic route is still intact
        let target = flex_route(&table, "research").await.unwrap();
        assert_eq!(target.provider_name, "openai");
        assert_eq!(target.service_id, "o1");

        // Inner table was reloaded
        let target = flex_route(&table, "default").await.unwrap();
        assert_eq!(target.provider_name, "anthropic");
    }
}
