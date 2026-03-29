//! A tool registry wrapper that adds runtime filter and restriction management.
//!
//! [`DynamicToolRegistry`] wraps any [`ToolRegistry`] and layers per-server
//! filters and parameter restrictions on top. Filters affect which tools are
//! visible; restrictions are stored for protocol-level call-time enforcement.
//!
//! Parallel to [`DynamicRoutingTable`](super::dynamic::DynamicRoutingTable)
//! for models.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::errors::{BitrouterError, Result};

use super::admin::{AdminToolRegistry, ParamRestrictions, ToolFilter, ToolUpstreamEntry};
use crate::tools::registry::{ToolEntry, ToolGateway, ToolRegistry};
use crate::tools::result::ToolCallResult;

/// A tool registry wrapper that adds runtime filter and restriction management.
///
/// Wraps any `T: ToolRegistry` and layers per-server state on top:
/// - **Filters** control which tools are visible in `list_tools()`.
/// - **Restrictions** are stored here and exposed for protocol crates to
///   read at call time via [`get_param_restrictions`](Self::get_param_restrictions).
pub struct DynamicToolRegistry<T> {
    inner: T,
    filters: RwLock<HashMap<String, ToolFilter>>,
    restrictions: RwLock<HashMap<String, ParamRestrictions>>,
}

impl<T> DynamicToolRegistry<T> {
    /// Create a new dynamic tool registry wrapping the given inner registry.
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

    /// Access the inner registry for protocol-specific operations.
    pub fn inner(&self) -> &T {
        &self.inner
    }

    /// Read the current parameter restrictions for a server.
    ///
    /// Protocol crates call this at tool-call time to enforce restrictions.
    pub fn get_param_restrictions(&self, server: &str) -> Option<ParamRestrictions> {
        self.restrictions
            .read()
            .ok()
            .and_then(|r| r.get(server).cloned())
    }

    /// Read the current filter for a server.
    pub fn get_filter(&self, server: &str) -> Option<ToolFilter> {
        self.filters
            .read()
            .ok()
            .and_then(|f| f.get(server).cloned())
    }

    /// Check whether a server name exists in the known set.
    fn known_servers(&self) -> Vec<String>
    where
        T: ToolRegistry,
    {
        // We cannot call async list_tools here, so we derive from filters/restrictions.
        // The authoritative set is populated at construction time from config.
        let mut servers: std::collections::HashSet<String> = std::collections::HashSet::new();
        if let Ok(f) = self.filters.read() {
            servers.extend(f.keys().cloned());
        }
        if let Ok(r) = self.restrictions.read() {
            servers.extend(r.keys().cloned());
        }
        servers.into_iter().collect()
    }
}

/// Extract the server (provider) name from a tool ID.
///
/// Tool IDs are namespaced as `"server/tool_name"`. Returns the portion
/// before the first `/`, or the entire string if no `/` is present.
fn server_of(tool_id: &str) -> &str {
    tool_id.split_once('/').map(|(s, _)| s).unwrap_or(tool_id)
}

/// Extract the un-namespaced tool name from a tool ID.
fn tool_name_of(tool_id: &str) -> &str {
    tool_id.split_once('/').map(|(_, t)| t).unwrap_or(tool_id)
}

impl<T: ToolGateway> ToolGateway for DynamicToolRegistry<T> {
    async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> Result<ToolCallResult> {
        let server = server_of(name);
        let tool = tool_name_of(name);

        // Enforce parameter restrictions before forwarding.
        let arguments = if let Some(restrictions) = self.get_param_restrictions(server) {
            let mut map = match arguments {
                serde_json::Value::Object(m) => Some(m),
                serde_json::Value::Null => None,
                other => Some(serde_json::Map::from_iter([("value".to_owned(), other)])),
            };
            restrictions.check(tool, &mut map)?;
            map.map(serde_json::Value::Object)
                .unwrap_or(serde_json::Value::Null)
        } else {
            arguments
        };

        self.inner.call_tool(name, arguments).await
    }
}

impl<T: ToolRegistry> ToolRegistry for DynamicToolRegistry<T> {
    async fn list_tools(&self) -> Vec<ToolEntry> {
        let all = self.inner.list_tools().await;
        let filters = match self.filters.read() {
            Ok(f) => f,
            Err(_) => return all,
        };

        all.into_iter()
            .filter(|entry| {
                let server = server_of(&entry.id);
                match filters.get(server) {
                    Some(filter) => filter.accepts(tool_name_of(&entry.id)),
                    None => true,
                }
            })
            .collect()
    }
}

impl<T: ToolGateway> AdminToolRegistry for DynamicToolRegistry<T> {
    async fn list_upstreams(&self) -> Vec<ToolUpstreamEntry> {
        // Get the filtered tool list to compute per-server tool counts.
        let tools = <Self as ToolRegistry>::list_tools(self).await;
        let mut counts: HashMap<String, usize> = HashMap::new();
        for tool in &tools {
            let server = server_of(&tool.id);
            *counts.entry(server.to_owned()).or_default() += 1;
        }

        // Also include servers that have filters/restrictions but no visible tools.
        let servers = self.known_servers();
        for s in &servers {
            counts.entry(s.clone()).or_default();
        }

        let filters = self.filters.read().ok();
        let restrictions = self.restrictions.read().ok();

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

    async fn update_filter(&self, server: &str, filter: Option<ToolFilter>) -> Result<()> {
        let mut filters = self
            .filters
            .write()
            .map_err(|_| BitrouterError::transport(None, "tool registry lock poisoned"))?;
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
            .map_err(|_| BitrouterError::transport(None, "tool registry lock poisoned"))?;
        if restrictions.rules.is_empty() {
            r.remove(server);
        } else {
            r.insert(server.to_owned(), restrictions);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::definition::ToolDefinition;

    struct StaticToolSource {
        tools: Vec<ToolEntry>,
    }

    impl ToolRegistry for StaticToolSource {
        async fn list_tools(&self) -> Vec<ToolEntry> {
            self.tools.clone()
        }
    }

    impl ToolGateway for StaticToolSource {
        async fn call_tool(
            &self,
            _name: &str,
            _arguments: serde_json::Value,
        ) -> Result<ToolCallResult> {
            Ok(ToolCallResult {
                content: vec![],
                is_error: false,
                metadata: None,
            })
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
                    input_modes: Vec::new(),
                    output_modes: Vec::new(),
                    examples: Vec::new(),
                    tags: Vec::new(),
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
                    input_modes: Vec::new(),
                    output_modes: Vec::new(),
                    examples: Vec::new(),
                    tags: Vec::new(),
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
                    input_modes: Vec::new(),
                    output_modes: Vec::new(),
                    examples: Vec::new(),
                    tags: Vec::new(),
                },
            },
        ]
    }

    fn test_registry() -> DynamicToolRegistry<StaticToolSource> {
        DynamicToolRegistry::new(
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
    async fn list_upstreams_reflects_state() {
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

        let upstreams = reg.list_upstreams().await;
        let github = upstreams.iter().find(|u| u.name == "github");
        assert!(github.is_some());
        let github = github.unwrap();
        assert_eq!(github.tool_count, 1); // only create_issue visible
        assert!(github.filter.is_some());
    }

    #[tokio::test]
    async fn param_restrictions_roundtrip() {
        let reg = test_registry();
        assert!(reg.get_param_restrictions("github").is_none());

        let restrictions = ParamRestrictions {
            rules: HashMap::from([(
                "search".to_owned(),
                super::super::admin::ParamRule {
                    deny: Some(vec!["force".to_owned()]),
                    allow: None,
                    action: super::super::admin::ParamViolationAction::Reject,
                },
            )]),
        };
        reg.update_param_restrictions("github", restrictions)
            .await
            .ok();

        let stored = reg.get_param_restrictions("github");
        assert!(stored.is_some());
        assert!(stored.unwrap().rules.contains_key("search"));
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
