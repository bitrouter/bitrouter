//! In-memory cache of policy files with single-policy-per-key resolution.
//!
//! Loaded from `<home>/policies/` at startup, refreshed on SIGHUP via
//! [`HotSwap`](bitrouter_core::sync::HotSwap).

use std::collections::HashMap;
use std::path::Path;

use bitrouter_core::routers::admin::{ToolFilter, ToolPolicyResolver};

use super::file::{self, PolicyFile};

/// Cached policy files keyed by ID, with methods to resolve tool
/// allow-lists for a single policy.
pub struct PolicyCache {
    policies: HashMap<String, PolicyFile>,
}

impl PolicyCache {
    /// Load all policy files from the given directory.
    pub fn load(policy_dir: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let files = file::load_policies(policy_dir)?;
        let policies = files.into_iter().map(|pf| (pf.id.clone(), pf)).collect();
        Ok(Self { policies })
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
            tracing::warn!(policy_id = %policy_id, "policy not found in cache");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_policy_file(
        id: &str,
        tool_rules: HashMap<String, bitrouter_core::policy::ToolProviderPolicy>,
    ) -> PolicyFile {
        PolicyFile {
            id: id.to_string(),
            config: file::PolicyConfig {
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
            bitrouter_core::policy::ToolProviderPolicy {
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
            bitrouter_core::policy::ToolProviderPolicy {
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
            bitrouter_core::policy::ToolProviderPolicy {
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
}
