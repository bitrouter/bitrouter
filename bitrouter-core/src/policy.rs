//! Policy file types, loading, and in-memory cache.
//!
//! Policy files are JSON documents stored in `<home>/policies/`. Each policy
//! defines spend limits and per-provider tool allow-lists. A single policy
//! is attached to an API key via the JWT `pol` claim.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use std::sync::RwLock;

use crate::errors::{BitrouterError, Result};
use crate::routers::admin::{
    AdminToolRegistry, ToolFilter, ToolPolicyAdmin, ToolPolicyResolver, ToolUpstreamEntry,
};
use crate::routers::content::RouteContext;
use crate::routers::registry::{ToolEntry, ToolRegistry};
use crate::routers::routing_table::{RouteEntry, RoutingTable, RoutingTarget};

// ── Per-provider policy ────────────────────────────────────────────

/// Per-provider tool policy configuration.
///
/// Defines an allow-list controlling which tools from this provider are
/// visible and callable. Used in policy files under the `tool_rules` key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolProviderPolicy {
    /// Visibility filter controlling which tools appear in discovery
    /// and are callable at request time.
    #[serde(default, flatten)]
    pub filter: ToolFilter,
}

// ── Data types ─────────────────────────────────────────────────────

/// Input context sent by OWS on stdin when invoking an executable policy.
///
/// Additional fields (`wallet`, `api_key`, etc.) are accepted and ignored
/// by serde's default behavior — only the fields needed for evaluation are
/// declared here.
#[derive(Debug, Deserialize)]
pub struct PolicyContext {
    /// CAIP-2 chain identifier (e.g. `"tempo:mainnet"`).
    #[serde(default)]
    pub chain: Option<String>,
    /// Transaction value in micro-USD.
    #[serde(default)]
    pub transaction_value: u64,
    /// Accumulated daily spend in micro-USD (provided by OWS).
    #[serde(default)]
    pub daily_total: u64,
    /// Accumulated monthly spend in micro-USD (provided by OWS).
    #[serde(default)]
    pub monthly_total: u64,
}

/// Result written to stdout after policy evaluation.
#[derive(Debug, Serialize)]
pub struct PolicyResult {
    pub allow: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Operator-defined policy configuration stored in a policy file.
///
/// Combines OWS spend-limit rules with tool access control (per-provider
/// allow-lists).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyConfig {
    /// Human-readable policy name.
    pub name: String,

    /// Maximum daily spend in micro-USD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daily_limit: Option<u64>,

    /// Maximum monthly spend in micro-USD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monthly_limit: Option<u64>,

    /// Maximum per-transaction value in micro-USD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_tx_max: Option<u64>,

    /// Allowed chains (CAIP-2). Empty means all chains allowed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_chains: Vec<String>,

    /// Policy expiration (ISO 8601). After this time, policy denies all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,

    /// Per-provider tool access rules.
    ///
    /// Keys are provider/server names (e.g. `"github"`). Values define
    /// allow-list filters for that provider. When absent or empty, no
    /// tool restrictions apply.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tool_rules: HashMap<String, ToolProviderPolicy>,
}

/// Full on-disk policy file: config + OWS integration metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyFile {
    /// Unique policy ID (UUID).
    pub id: String,

    /// The policy configuration (spend limits + tool access rules).
    #[serde(flatten)]
    pub config: PolicyConfig,

    /// Path to the evaluator executable (populated by `create`).
    pub executable: String,

    /// When this policy was created (ISO 8601).
    pub created_at: String,
}

// ── Loading ────────────────────────────────────────────────────────

/// A policy file that could not be loaded.
#[derive(Debug)]
pub struct SkippedPolicy {
    /// Path to the file that failed.
    pub path: PathBuf,
    /// Why it was skipped.
    pub error: String,
}

/// Result of loading policy files from a directory.
#[derive(Debug)]
pub struct LoadedPolicies {
    /// Successfully loaded policy files, sorted by name.
    pub policies: Vec<PolicyFile>,
    /// Files that were skipped due to read or parse errors.
    pub skipped: Vec<SkippedPolicy>,
}

/// Load all policy files from the given directory.
///
/// Malformed or unreadable files are collected in [`LoadedPolicies::skipped`]
/// rather than logged — the caller decides how to report them.
/// Returns an empty result if the directory does not exist.
pub fn load_policies(dir: &Path) -> std::io::Result<LoadedPolicies> {
    if !dir.exists() {
        return Ok(LoadedPolicies {
            policies: Vec::new(),
            skipped: Vec::new(),
        });
    }

    let mut policies = Vec::new();
    let mut skipped = Vec::new();

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            match std::fs::read_to_string(&path) {
                Ok(content) => match serde_json::from_str::<PolicyFile>(&content) {
                    Ok(pf) => policies.push(pf),
                    Err(e) => skipped.push(SkippedPolicy {
                        path,
                        error: e.to_string(),
                    }),
                },
                Err(e) => skipped.push(SkippedPolicy {
                    path,
                    error: e.to_string(),
                }),
            }
        }
    }

    policies.sort_by(|a, b| a.config.name.cmp(&b.config.name));
    Ok(LoadedPolicies { policies, skipped })
}

/// Resolve the policy directory for a given BitRouter home.
pub fn policy_dir(home: &Path) -> PathBuf {
    home.join("policies")
}

// ── Cache ──────────────────────────────────────────────────────────

/// Cached policy files keyed by ID, with methods to resolve tool
/// allow-lists for a single policy.
pub struct PolicyCache {
    policies: HashMap<String, PolicyFile>,
}

impl PolicyCache {
    /// Build a cache from already-loaded policy files.
    pub fn new(files: Vec<PolicyFile>) -> Self {
        let policies = files.into_iter().map(|pf| (pf.id.clone(), pf)).collect();
        Self { policies }
    }

    /// Create an empty cache (no policies loaded).
    pub fn empty() -> Self {
        Self {
            policies: HashMap::new(),
        }
    }
}

impl ToolPolicyResolver for PolicyCache {
    fn resolve_filters(&self, policy_id: &str) -> HashMap<String, ToolFilter> {
        let Some(pf) = self.policies.get(policy_id) else {
            return HashMap::new();
        };

        pf.config
            .tool_rules
            .iter()
            .map(|(provider, rule)| (provider.clone(), rule.filter.clone()))
            .collect()
    }

    fn resolve_tool_filter(&self, policy_id: &str, provider: &str) -> Option<ToolFilter> {
        let pf = self.policies.get(policy_id)?;
        pf.config
            .tool_rules
            .get(provider)
            .map(|rule| rule.filter.clone())
    }
}

// ── Guarded tool registry ──────────────────────────────────────────

/// A tool registry wrapper that applies visibility filters at discovery time.
///
/// Wraps any `T: ToolRegistry` and layers per-server filter policy on top.
/// Filters control which tools are visible in `list_tools()`.
pub struct GuardedToolRegistry<T> {
    inner: T,
    filters: RwLock<HashMap<String, ToolFilter>>,
}

impl<T> GuardedToolRegistry<T> {
    /// Create a new guarded tool registry wrapping the given inner registry.
    pub fn new(inner: T, filters: HashMap<String, ToolFilter>) -> Self {
        Self {
            inner,
            filters: RwLock::new(filters),
        }
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
}

impl<T: ToolRegistry> AdminToolRegistry for GuardedToolRegistry<T> {
    async fn list_upstreams(&self) -> Vec<ToolUpstreamEntry> {
        let all_tools = self.inner.list_tools().await;
        let filters = self.filters.read().ok();

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
                ToolUpstreamEntry {
                    name,
                    tool_count,
                    filter,
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

    fn make_policy_file(id: &str, tool_rules: HashMap<String, ToolProviderPolicy>) -> PolicyFile {
        PolicyFile {
            id: id.to_string(),
            config: PolicyConfig {
                name: id.to_string(),
                daily_limit: None,
                monthly_limit: None,
                per_tx_max: None,
                allowed_chains: vec![],
                expires_at: None,
                tool_rules,
            },
            executable: String::new(),
            created_at: String::new(),
        }
    }

    #[test]
    fn policy_config_round_trips_through_json() {
        let config = PolicyConfig {
            name: "test".into(),
            daily_limit: Some(10_000_000),
            monthly_limit: None,
            per_tx_max: Some(1_000_000),
            allowed_chains: vec!["tempo:mainnet".into()],
            expires_at: None,
            tool_rules: HashMap::new(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: PolicyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "test");
        assert_eq!(parsed.daily_limit, Some(10_000_000));
        assert!(parsed.tool_rules.is_empty());
    }

    #[test]
    fn policy_file_with_tool_rules_deserializes() {
        let json = r#"{
            "id": "abc-123",
            "name": "restricted-agent",
            "tool_rules": {
                "github": {
                    "allow": ["search_code", "get_file"]
                }
            },
            "executable": "bitrouter policy eval",
            "created_at": "2026-04-10T00:00:00Z"
        }"#;
        let pf: PolicyFile = serde_json::from_str(json).unwrap();
        assert_eq!(pf.id, "abc-123");
        assert_eq!(pf.config.name, "restricted-agent");
        assert!(pf.config.tool_rules.contains_key("github"));
        let github = &pf.config.tool_rules["github"];
        assert!(github.filter.allow.is_some());
    }

    #[test]
    fn policy_file_without_tool_rules_deserializes() {
        let json = r#"{
            "id": "spend-only",
            "name": "Spend Limit",
            "daily_limit": 5000000,
            "executable": "bitrouter policy eval",
            "created_at": "2026-04-10T00:00:00Z"
        }"#;
        let pf: PolicyFile = serde_json::from_str(json).unwrap();
        assert_eq!(pf.config.daily_limit, Some(5_000_000));
        assert!(pf.config.tool_rules.is_empty());
    }

    #[test]
    fn empty_cache_resolves_nothing() {
        let cache = PolicyCache::empty();
        let filters = cache.resolve_filters("nonexistent");
        assert!(filters.is_empty());
    }

    #[test]
    fn single_policy_resolves_filter() {
        let mut tool_rules = HashMap::new();
        tool_rules.insert(
            "github".into(),
            ToolProviderPolicy {
                filter: ToolFilter {
                    allow: Some(vec!["search_code".into(), "get_file".into()]),
                },
            },
        );
        let pf = make_policy_file("p1", tool_rules);
        let cache = PolicyCache {
            policies: HashMap::from([("p1".into(), pf)]),
        };

        let filters = cache.resolve_filters("p1");
        assert!(filters.contains_key("github"));
        let f = &filters["github"];
        assert!(f.accepts("search_code"));
        assert!(f.accepts("get_file"));
        assert!(!f.accepts("delete_repo"));
    }

    #[test]
    fn no_allow_list_accepts_all() {
        let mut tool_rules = HashMap::new();
        tool_rules.insert(
            "github".into(),
            ToolProviderPolicy {
                filter: ToolFilter::default(),
            },
        );
        let pf = make_policy_file("p1", tool_rules);
        let cache = PolicyCache {
            policies: HashMap::from([("p1".into(), pf)]),
        };

        let filters = cache.resolve_filters("p1");
        let f = &filters["github"];
        assert!(f.accepts("anything"));
    }

    #[test]
    fn resolve_tool_filter_returns_none_for_missing_provider() {
        let mut tool_rules = HashMap::new();
        tool_rules.insert(
            "github".into(),
            ToolProviderPolicy {
                filter: ToolFilter {
                    allow: Some(vec!["search_code".into()]),
                },
            },
        );
        let pf = make_policy_file("p1", tool_rules);
        let cache = PolicyCache {
            policies: HashMap::from([("p1".into(), pf)]),
        };

        assert!(cache.resolve_tool_filter("p1", "github").is_some());
        assert!(cache.resolve_tool_filter("p1", "jira").is_none());
        assert!(cache.resolve_tool_filter("nonexistent", "github").is_none());
    }

    // ── GuardedToolRegistry tests ──────────────────────────────────

    use crate::tools::definition::ToolDefinition;

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
    async fn allow_filter_restricts_tools() {
        let reg = test_registry();
        reg.update_filter(
            "github",
            Some(ToolFilter {
                allow: Some(vec!["search".to_owned()]),
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
                allow: Some(vec!["search".to_owned()]),
            }),
        )
        .await
        .ok();
        assert_eq!(reg.list_tools().await.len(), 2);

        reg.update_filter("github", None).await.ok();
        assert_eq!(reg.list_tools().await.len(), 3);
    }

    #[test]
    fn tool_filter_allow_list_logic() {
        let filter = ToolFilter {
            allow: Some(vec!["search".to_owned()]),
        };
        assert!(filter.accepts("search"));
        assert!(!filter.accepts("delete"));
        assert!(!filter.accepts("create"));
    }

    #[test]
    fn tool_filter_empty_accepts_all() {
        let filter = ToolFilter::default();
        assert!(filter.accepts("anything"));
    }
}
