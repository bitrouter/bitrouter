//! Tool registry wrapper that layers visibility filters on top of any
//! [`ToolRegistry`].
//!
//! [`GuardedToolRegistry`] mirrors [`GuardedRouter`](crate::router::GuardedRouter)
//! for models at the discovery layer — it is a composable decorator that
//! controls which tools are visible without coupling to routing or
//! call-time enforcement.
//!
//! Call-time parameter enforcement lives in
//! [`GuardedToolProvider`](crate::guarded_tool_provider::GuardedToolProvider),
//! created by [`GuardedToolRouter`](crate::tool_router::GuardedToolRouter).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use bitrouter_core::errors::{BitrouterError, Result};
use bitrouter_core::routers::admin::{
    AdminToolRegistry, ToolFilter, ToolPolicyAdmin, ToolUpstreamEntry,
};
use bitrouter_core::routers::content::RouteContext;
use bitrouter_core::routers::registry::{ToolEntry, ToolRegistry};
use bitrouter_core::routers::routing_table::{RouteEntry, RoutingTable, RoutingTarget};

/// A tool registry wrapper that applies visibility filters at discovery time.
///
/// Wraps any `T: ToolRegistry` and layers per-server filter policy on top.
/// Filters control which tools are visible in `list_tools()`.
///
/// Call-time enforcement (parameter restrictions) is handled separately by
/// [`GuardedToolProvider`](crate::guarded_tool_provider::GuardedToolProvider)
/// via the [`ToolGuardrail`](crate::tool_engine::ToolGuardrail) engine.
///
/// An optional shared `ToolGuardrail` reference allows `list_upstreams()`
/// to include parameter restriction info in admin responses.
pub struct GuardedToolRegistry<T> {
    inner: T,
    filters: RwLock<HashMap<String, ToolFilter>>,
    guardrail: Option<Arc<std::sync::RwLock<Arc<crate::tool_engine::ToolGuardrail>>>>,
}

impl<T> GuardedToolRegistry<T> {
    /// Create a new guarded tool registry wrapping the given inner registry.
    pub fn new(inner: T, filters: HashMap<String, ToolFilter>) -> Self {
        Self {
            inner,
            filters: RwLock::new(filters),
            guardrail: None,
        }
    }

    /// Attach a shared tool guardrail for admin inspection.
    pub fn with_guardrail(
        mut self,
        guardrail: Arc<std::sync::RwLock<Arc<crate::tool_engine::ToolGuardrail>>>,
    ) -> Self {
        self.guardrail = Some(guardrail);
        self
    }

    /// Access the inner registry.
    pub fn inner(&self) -> &T {
        &self.inner
    }

    /// Replace the entire filter set atomically. Used during hot-reload
    /// to synchronize filters with the reloaded configuration.
    pub fn sync_filters(&self, filters: HashMap<String, ToolFilter>) -> Result<()> {
        let mut current = self
            .filters
            .write()
            .map_err(|_| BitrouterError::transport(None, "tool policy lock poisoned"))?;
        *current = filters;
        Ok(())
    }
}

impl<T: ToolRegistry> ToolRegistry for GuardedToolRegistry<T> {
    async fn list_tools(&self) -> Vec<ToolEntry> {
        let all = self.inner.list_tools().await;
        let filters = match self.filters.read() {
            Ok(f) => f,
            // Fail closed: poisoned lock means a writer panicked, so
            // return no tools rather than leaking unfiltered data.
            Err(_) => return Vec::new(),
        };

        all.into_iter()
            .filter(|entry| match filters.get(entry.server()) {
                Some(filter) => filter.accepts(entry.tool_name()),
                None => true,
            })
            .collect()
    }
}

impl<T: ToolRegistry> ToolPolicyAdmin for GuardedToolRegistry<T> {
    async fn update_filter(&self, server: &str, filter: Option<ToolFilter>) -> Result<()> {
        let mut filters = self
            .filters
            .write()
            .map_err(|_| BitrouterError::transport(None, "tool policy lock poisoned"))?;
        match filter {
            Some(f) => {
                filters.insert(server.to_owned(), f);
            }
            None => {
                filters.remove(server);
            }
        }
        Ok(())
    }

    async fn update_param_restrictions(
        &self,
        _server: &str,
        _restrictions: bitrouter_core::routers::admin::ParamRestrictions,
    ) -> Result<()> {
        tracing::warn!(
            "runtime parameter restriction updates are managed by ToolGuardrail config — \
             this operation is a no-op"
        );
        Ok(())
    }
}

impl<T: ToolRegistry> AdminToolRegistry for GuardedToolRegistry<T> {
    async fn list_upstreams(&self) -> Vec<ToolUpstreamEntry> {
        let all_tools = self.inner.list_tools().await;
        let filters = self.filters.read().ok();
        let guardrail_snapshot = self
            .guardrail
            .as_ref()
            .and_then(|g| g.read().ok().map(|guard| Arc::clone(&guard)));

        let mut counts: HashMap<String, usize> = HashMap::new();
        for tool in &all_tools {
            let visible = match filters.as_ref().and_then(|f| f.get(tool.server())) {
                Some(filter) => filter.accepts(tool.tool_name()),
                None => true,
            };
            if visible {
                *counts.entry(tool.server().to_owned()).or_default() += 1;
            } else {
                counts.entry(tool.server().to_owned()).or_default();
            }
        }

        // Include servers known only from filters (no tools).
        if let Some(ref f) = filters {
            for key in f.keys() {
                counts.entry(key.clone()).or_default();
            }
        }

        let mut entries: Vec<ToolUpstreamEntry> = counts
            .into_iter()
            .map(|(name, tool_count)| {
                let filter = filters.as_ref().and_then(|f| f.get(&name).cloned());
                let param_restrictions = guardrail_snapshot
                    .as_ref()
                    .and_then(|g| g.param_restrictions_for(&name).cloned())
                    .filter(|r| !r.rules.is_empty());
                ToolUpstreamEntry {
                    name,
                    tool_count,
                    filter,
                    param_restrictions,
                }
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }
}

impl<T: RoutingTable + Send + Sync> RoutingTable for GuardedToolRegistry<T> {
    async fn route(&self, incoming_name: &str, context: &RouteContext) -> Result<RoutingTarget> {
        self.inner.route(incoming_name, context).await
    }

    fn list_routes(&self) -> Vec<RouteEntry> {
        self.inner.list_routes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_core::tools::definition::ToolDefinition;

    struct StaticToolSource {
        tools: Vec<ToolEntry>,
    }

    impl ToolRegistry for StaticToolSource {
        async fn list_tools(&self) -> Vec<ToolEntry> {
            self.tools.clone()
        }
    }

    fn test_tools() -> Vec<ToolEntry> {
        vec![
            ToolEntry {
                id: "github/search".to_owned(),
                provider: "github".to_owned(),
                definition: ToolDefinition {
                    name: "Search".to_owned(),
                    description: Some("Search GitHub".to_owned()),
                    input_schema: None,
                    annotations: None,
                    input_examples: Vec::new(),
                },
            },
            ToolEntry {
                id: "github/create_issue".to_owned(),
                provider: "github".to_owned(),
                definition: ToolDefinition {
                    name: "Create Issue".to_owned(),
                    description: Some("Create an issue".to_owned()),
                    input_schema: None,
                    annotations: None,
                    input_examples: Vec::new(),
                },
            },
            ToolEntry {
                id: "jira/search".to_owned(),
                provider: "jira".to_owned(),
                definition: ToolDefinition {
                    name: "Search".to_owned(),
                    description: Some("Search Jira".to_owned()),
                    input_schema: None,
                    annotations: None,
                    input_examples: Vec::new(),
                },
            },
        ]
    }

    fn test_registry() -> GuardedToolRegistry<StaticToolSource> {
        GuardedToolRegistry::new(
            StaticToolSource {
                tools: test_tools(),
            },
            HashMap::new(),
        )
    }

    #[tokio::test]
    async fn no_filter_returns_all_tools() {
        let reg = test_registry();
        let tools = reg.list_tools().await;
        assert_eq!(tools.len(), 3);
    }

    #[tokio::test]
    async fn deny_filter_hides_tools() {
        let reg = test_registry();
        reg.update_filter(
            "github",
            Some(ToolFilter {
                allow: None,
                deny: Some(vec!["search".to_owned()]),
            }),
        )
        .await
        .ok();

        let tools = reg.list_tools().await;
        assert_eq!(tools.len(), 2);
        assert!(tools.iter().all(|t| t.id != "github/search"));
    }

    #[tokio::test]
    async fn allow_filter_restricts_tools() {
        let reg = test_registry();
        reg.update_filter(
            "github",
            Some(ToolFilter {
                allow: Some(vec!["search".to_owned()]),
                deny: None,
            }),
        )
        .await
        .ok();

        let tools = reg.list_tools().await;
        assert_eq!(tools.len(), 2); // github/search + jira/search
        assert!(tools.iter().any(|t| t.id == "github/search"));
        assert!(!tools.iter().any(|t| t.id == "github/create_issue"));
    }

    #[tokio::test]
    async fn clear_filter_restores_all() {
        let reg = test_registry();
        reg.update_filter(
            "github",
            Some(ToolFilter {
                deny: Some(vec!["search".to_owned()]),
                ..Default::default()
            }),
        )
        .await
        .ok();
        assert_eq!(reg.list_tools().await.len(), 2);

        reg.update_filter("github", None).await.ok();
        assert_eq!(reg.list_tools().await.len(), 3);
    }

    #[test]
    fn tool_filter_accepts_logic() {
        let filter = ToolFilter {
            allow: Some(vec!["search".to_owned()]),
            deny: Some(vec!["delete".to_owned()]),
        };
        assert!(filter.accepts("search"));
        assert!(!filter.accepts("delete"));
        assert!(!filter.accepts("create")); // not in allow list
    }

    #[test]
    fn tool_filter_deny_takes_precedence() {
        let filter = ToolFilter {
            allow: Some(vec!["search".to_owned()]),
            deny: Some(vec!["search".to_owned()]),
        };
        assert!(!filter.accepts("search")); // deny wins
    }

    #[test]
    fn tool_filter_empty_accepts_all() {
        let filter = ToolFilter::default();
        assert!(filter.accepts("anything"));
    }
}
