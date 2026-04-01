//! Tool policy wrapper that layers visibility filters and parameter
//! restrictions on top of any [`ToolRegistry`].
//!
//! [`GuardedToolRegistry`] mirrors [`GuardedRouter`](crate::router::GuardedRouter)
//! for models — it is a composable decorator that enforces policy without
//! coupling to the routing layer.

use std::collections::HashMap;
use std::sync::RwLock;

use bitrouter_core::errors::{BitrouterError, Result};
use bitrouter_core::routers::admin::{
    AdminToolRegistry, HasParamRestrictions, ParamRestrictions, ToolFilter, ToolPolicyAdmin,
    ToolUpstreamEntry,
};
use bitrouter_core::routers::registry::{ToolEntry, ToolRegistry};
use bitrouter_core::routers::routing_table::{RouteEntry, RoutingTable, RoutingTarget};

/// A tool registry wrapper that applies visibility filters and parameter
/// restrictions.
///
/// Wraps any `T: ToolRegistry` and layers per-server policy on top:
/// - **Filters** control which tools are visible in `list_tools()`.
/// - **Restrictions** are stored here and exposed for protocol crates to
///   read at call time via [`HasParamRestrictions`].
///
/// Parallel to [`GuardedRouter`](crate::router::GuardedRouter) for models.
pub struct GuardedToolRegistry<T> {
    inner: T,
    filters: RwLock<HashMap<String, ToolFilter>>,
    restrictions: RwLock<HashMap<String, ParamRestrictions>>,
}

impl<T> GuardedToolRegistry<T> {
    /// Create a new guarded tool registry wrapping the given inner registry.
    pub fn new(
        inner: T,
        filters: HashMap<String, ToolFilter>,
        restrictions: HashMap<String, ParamRestrictions>,
    ) -> Self {
        Self {
            inner,
            filters: RwLock::new(filters),
            restrictions: RwLock::new(restrictions),
        }
    }

    /// Access the inner registry.
    pub fn inner(&self) -> &T {
        &self.inner
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

impl<T: Send + Sync> HasParamRestrictions for GuardedToolRegistry<T> {
    fn get_param_restrictions(&self, server: &str) -> Option<ParamRestrictions> {
        self.restrictions
            .read()
            .ok()
            .and_then(|r| r.get(server).cloned())
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
        server: &str,
        restrictions: ParamRestrictions,
    ) -> Result<()> {
        let mut r = self
            .restrictions
            .write()
            .map_err(|_| BitrouterError::transport(None, "tool policy lock poisoned"))?;
        if restrictions.rules.is_empty() {
            r.remove(server);
        } else {
            r.insert(server.to_owned(), restrictions);
        }
        Ok(())
    }
}

impl<T: AdminToolRegistry> AdminToolRegistry for GuardedToolRegistry<T> {
    async fn list_upstreams(&self) -> Vec<ToolUpstreamEntry> {
        // Get all tools from the inner (unfiltered) registry for a complete
        // server list, then compute filtered counts per server.
        let all_tools = self.inner.list_tools().await;
        let filters = self.filters.read().ok();
        let restrictions = self.restrictions.read().ok();

        // Build full server set: tools + filter keys + restriction keys.
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

        // Include servers known only from policy (no tools).
        if let Some(ref f) = filters {
            for key in f.keys() {
                counts.entry(key.clone()).or_default();
            }
        }
        if let Some(ref r) = restrictions {
            for key in r.keys() {
                counts.entry(key.clone()).or_default();
            }
        }

        let mut entries: Vec<ToolUpstreamEntry> = counts
            .into_iter()
            .map(|(name, tool_count)| {
                let filter = filters.as_ref().and_then(|f| f.get(&name).cloned());
                let param_restrictions = restrictions
                    .as_ref()
                    .and_then(|r| r.get(&name).cloned())
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
    async fn route(&self, incoming_name: &str) -> Result<RoutingTarget> {
        self.inner.route(incoming_name).await
    }

    fn list_routes(&self) -> Vec<RouteEntry> {
        self.inner.list_routes()
    }
}

// ── MCP trait delegation ──────────────────────────────────────────────
//
// These impls delegate all MCP server traits to the inner registry so
// that `GuardedToolRegistry<Arc<DynamicRoutingTable<ConfigMcpRegistry>>>`
// can satisfy the `McpServer` bound required by MCP filters.

use bitrouter_core::api::mcp::gateway::{
    McpCompletionServer, McpLoggingServer, McpPromptServer, McpResourceServer,
    McpSubscriptionServer, McpToolServer,
};
use bitrouter_core::api::mcp::types::{
    CompleteParams, CompleteResult, LoggingLevel, McpGatewayError, McpGetPromptResult, McpPrompt,
    McpResource, McpResourceContent, McpResourceTemplate, McpTool, McpToolCallResult,
};
use tokio::sync::broadcast;

impl<T: McpToolServer + ToolRegistry + Send + Sync> McpToolServer for GuardedToolRegistry<T> {
    async fn list_tools(&self) -> Vec<McpTool> {
        let core_tools = <Self as ToolRegistry>::list_tools(self).await;
        core_tools.into_iter().map(McpTool::from).collect()
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> std::result::Result<McpToolCallResult, McpGatewayError> {
        self.inner.call_tool(name, arguments).await
    }

    fn subscribe_tool_changes(&self) -> broadcast::Receiver<()> {
        self.inner.subscribe_tool_changes()
    }
}

impl<T: McpResourceServer + Send + Sync> McpResourceServer for GuardedToolRegistry<T> {
    async fn list_resources(&self) -> Vec<McpResource> {
        self.inner.list_resources().await
    }

    async fn read_resource(
        &self,
        uri: &str,
    ) -> std::result::Result<Vec<McpResourceContent>, McpGatewayError> {
        self.inner.read_resource(uri).await
    }

    async fn list_resource_templates(&self) -> Vec<McpResourceTemplate> {
        self.inner.list_resource_templates().await
    }

    fn subscribe_resource_changes(&self) -> broadcast::Receiver<()> {
        self.inner.subscribe_resource_changes()
    }
}

impl<T: McpPromptServer + Send + Sync> McpPromptServer for GuardedToolRegistry<T> {
    async fn list_prompts(&self) -> Vec<McpPrompt> {
        self.inner.list_prompts().await
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<std::collections::HashMap<String, String>>,
    ) -> std::result::Result<McpGetPromptResult, McpGatewayError> {
        self.inner.get_prompt(name, arguments).await
    }

    fn subscribe_prompt_changes(&self) -> broadcast::Receiver<()> {
        self.inner.subscribe_prompt_changes()
    }
}

impl<T: McpSubscriptionServer + Send + Sync> McpSubscriptionServer for GuardedToolRegistry<T> {
    async fn subscribe_resource(&self, uri: &str) -> std::result::Result<(), McpGatewayError> {
        self.inner.subscribe_resource(uri).await
    }

    async fn unsubscribe_resource(&self, uri: &str) -> std::result::Result<(), McpGatewayError> {
        self.inner.unsubscribe_resource(uri).await
    }
}

impl<T: McpLoggingServer + Send + Sync> McpLoggingServer for GuardedToolRegistry<T> {
    async fn set_logging_level(
        &self,
        level: LoggingLevel,
    ) -> std::result::Result<(), McpGatewayError> {
        self.inner.set_logging_level(level).await
    }
}

impl<T: McpCompletionServer + Send + Sync> McpCompletionServer for GuardedToolRegistry<T> {
    async fn complete(
        &self,
        params: CompleteParams,
    ) -> std::result::Result<CompleteResult, McpGatewayError> {
        self.inner.complete(params).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_core::routers::admin::ParamRule;
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

    #[tokio::test]
    async fn param_restrictions_roundtrip() {
        let reg = test_registry();
        assert!(reg.get_param_restrictions("github").is_none());

        let restrictions = ParamRestrictions {
            rules: HashMap::from([(
                "search".to_owned(),
                ParamRule {
                    deny: Some(vec!["force".to_owned()]),
                    allow: None,
                    action: bitrouter_core::routers::admin::ParamViolationAction::Reject,
                },
            )]),
        };
        reg.update_param_restrictions("github", restrictions)
            .await
            .ok();

        let stored = reg.get_param_restrictions("github");
        assert!(stored.is_some());
        assert!(
            stored
                .as_ref()
                .is_some_and(|s| s.rules.contains_key("search"))
        );
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
