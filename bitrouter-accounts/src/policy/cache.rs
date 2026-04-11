//! In-memory cache of policy files with AND-semantics resolution.
//!
//! Loaded from `<home>/policies/` at startup, refreshed on SIGHUP via
//! [`HotSwap`](bitrouter_core::sync::HotSwap).

use std::collections::HashMap;
use std::path::Path;

use bitrouter_core::routers::admin::{
    ParamRestrictions, ResolvedToolRules, ToolFilter, ToolPolicyResolver,
};

use super::file::{self, PolicyFile};

/// Cached policy files keyed by ID, with methods to resolve merged
/// access rules across multiple policies (AND semantics).
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
    fn resolve_filters(&self, policy_ids: &[String]) -> HashMap<String, ToolFilter> {
        let mut merged: HashMap<String, ToolFilter> = HashMap::new();

        for id in policy_ids {
            let Some(pf) = self.policies.get(id) else {
                tracing::warn!(policy_id = %id, "policy not found in cache, skipping");
                continue;
            };
            for (provider, rule) in &pf.config.tool_rules {
                if let Some(ref filter) = rule.filter {
                    merged
                        .entry(provider.clone())
                        .and_modify(|existing| merge_filter(existing, filter))
                        .or_insert_with(|| filter.clone());
                }
            }
        }

        merged
    }

    fn resolve_tool_rules(
        &self,
        policy_ids: &[String],
        provider: &str,
    ) -> Option<ResolvedToolRules> {
        let mut result = ResolvedToolRules::default();
        let mut has_rules = false;

        for id in policy_ids {
            let Some(pf) = self.policies.get(id) else {
                tracing::warn!(policy_id = %id, "policy not found in cache, skipping");
                continue;
            };
            if let Some(rule) = pf.config.tool_rules.get(provider) {
                if let Some(ref filter) = rule.filter {
                    has_rules = true;
                    match result.filter {
                        Some(ref mut existing) => merge_filter(existing, filter),
                        None => result.filter = Some(filter.clone()),
                    }
                }
                if let Some(ref restrictions) = rule.param_restrictions {
                    has_rules = true;
                    match result.param_restrictions {
                        Some(ref mut existing) => merge_param_restrictions(existing, restrictions),
                        None => result.param_restrictions = Some(restrictions.clone()),
                    }
                }
            }
        }

        has_rules.then_some(result)
    }
}

/// Merge `other` filter into `target` with AND semantics.
///
/// - deny: union (any policy can deny a tool)
/// - allow: intersection (tool must be allowed by all policies that set allow)
fn merge_filter(target: &mut ToolFilter, other: &ToolFilter) {
    // Union deny lists.
    if let Some(ref other_deny) = other.deny {
        let deny = target.deny.get_or_insert_with(Vec::new);
        for item in other_deny {
            if !deny.contains(item) {
                deny.push(item.clone());
            }
        }
    }

    // Intersect allow lists.
    match (&mut target.allow, &other.allow) {
        (Some(existing), Some(other_allow)) => {
            existing.retain(|item| other_allow.contains(item));
        }
        (None, Some(other_allow)) => {
            target.allow = Some(other_allow.clone());
        }
        _ => {}
    }
}

/// Merge `other` param restrictions into `target` with AND semantics.
///
/// Per-tool rules are merged: deny lists unioned, allow lists intersected,
/// strictest action wins (Reject > Strip).
fn merge_param_restrictions(target: &mut ParamRestrictions, other: &ParamRestrictions) {
    use bitrouter_core::routers::admin::ParamViolationAction;

    for (tool, other_rule) in &other.rules {
        target
            .rules
            .entry(tool.clone())
            .and_modify(|existing| {
                // Union deny lists.
                if let Some(ref other_deny) = other_rule.deny {
                    let deny = existing.deny.get_or_insert_with(Vec::new);
                    for item in other_deny {
                        if !deny.contains(item) {
                            deny.push(item.clone());
                        }
                    }
                }
                // Intersect allow lists.
                match (&mut existing.allow, &other_rule.allow) {
                    (Some(ea), Some(oa)) => {
                        ea.retain(|item| oa.contains(item));
                    }
                    (None, Some(oa)) => {
                        existing.allow = Some(oa.clone());
                    }
                    _ => {}
                }
                // Strictest action wins.
                if matches!(other_rule.action, ParamViolationAction::Reject) {
                    existing.action = ParamViolationAction::Reject;
                }
            })
            .or_insert_with(|| other_rule.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_core::routers::admin::{ParamRule, ParamViolationAction};

    fn make_policy_file(
        id: &str,
        tool_rules: HashMap<String, super::super::config::ToolProviderPolicy>,
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
        let filters = cache.resolve_filters(&["nonexistent".into()]);
        assert!(filters.is_empty());
    }

    /// Empty policy_ids means "no restrictions" (owner mode).
    /// This is intentional: `pol: []` in a JWT should not enforce anything.
    #[test]
    fn empty_policy_ids_resolves_nothing() {
        let mut tool_rules = HashMap::new();
        tool_rules.insert(
            "github".into(),
            super::super::config::ToolProviderPolicy {
                filter: Some(ToolFilter {
                    allow: None,
                    deny: Some(vec!["delete_repo".into()]),
                }),
                param_restrictions: None,
            },
        );
        let pf = make_policy_file("p1", tool_rules);
        let cache = PolicyCache {
            policies: HashMap::from([("p1".into(), pf)]),
        };

        // Empty slice = no policies to evaluate = no restrictions.
        let filters = cache.resolve_filters(&[]);
        assert!(filters.is_empty());
        assert!(cache.resolve_tool_rules(&[], "github").is_none());
    }

    #[test]
    fn single_policy_resolves_filter() {
        let mut tool_rules = HashMap::new();
        tool_rules.insert(
            "github".into(),
            super::super::config::ToolProviderPolicy {
                filter: Some(ToolFilter {
                    allow: None,
                    deny: Some(vec!["delete_repo".into()]),
                }),
                param_restrictions: None,
            },
        );
        let pf = make_policy_file("p1", tool_rules);
        let cache = PolicyCache {
            policies: HashMap::from([("p1".into(), pf)]),
        };

        let filters = cache.resolve_filters(&["p1".into()]);
        assert!(filters.contains_key("github"));
        let f = &filters["github"];
        assert!(!f.accepts("delete_repo"));
        assert!(f.accepts("search_code"));
    }

    #[test]
    fn two_policies_deny_union() {
        let mut rules1 = HashMap::new();
        rules1.insert(
            "github".into(),
            super::super::config::ToolProviderPolicy {
                filter: Some(ToolFilter {
                    allow: None,
                    deny: Some(vec!["delete_repo".into()]),
                }),
                param_restrictions: None,
            },
        );
        let mut rules2 = HashMap::new();
        rules2.insert(
            "github".into(),
            super::super::config::ToolProviderPolicy {
                filter: Some(ToolFilter {
                    allow: None,
                    deny: Some(vec!["delete_branch".into()]),
                }),
                param_restrictions: None,
            },
        );
        let cache = PolicyCache {
            policies: HashMap::from([
                ("p1".into(), make_policy_file("p1", rules1)),
                ("p2".into(), make_policy_file("p2", rules2)),
            ]),
        };

        let filters = cache.resolve_filters(&["p1".into(), "p2".into()]);
        let f = &filters["github"];
        assert!(!f.accepts("delete_repo"));
        assert!(!f.accepts("delete_branch"));
        assert!(f.accepts("search_code"));
    }

    #[test]
    fn two_policies_allow_intersection() {
        let mut rules1 = HashMap::new();
        rules1.insert(
            "github".into(),
            super::super::config::ToolProviderPolicy {
                filter: Some(ToolFilter {
                    allow: Some(vec!["search_code".into(), "get_file".into()]),
                    deny: None,
                }),
                param_restrictions: None,
            },
        );
        let mut rules2 = HashMap::new();
        rules2.insert(
            "github".into(),
            super::super::config::ToolProviderPolicy {
                filter: Some(ToolFilter {
                    allow: Some(vec!["search_code".into(), "list_repos".into()]),
                    deny: None,
                }),
                param_restrictions: None,
            },
        );
        let cache = PolicyCache {
            policies: HashMap::from([
                ("p1".into(), make_policy_file("p1", rules1)),
                ("p2".into(), make_policy_file("p2", rules2)),
            ]),
        };

        let filters = cache.resolve_filters(&["p1".into(), "p2".into()]);
        let f = &filters["github"];
        assert!(f.accepts("search_code"));
        assert!(!f.accepts("get_file"));
        assert!(!f.accepts("list_repos"));
    }

    #[test]
    fn param_restrictions_merge_strictest_action() {
        let mut rules1 = HashMap::new();
        rules1.insert(
            "github".into(),
            super::super::config::ToolProviderPolicy {
                filter: None,
                param_restrictions: Some(ParamRestrictions {
                    rules: HashMap::from([(
                        "search".into(),
                        ParamRule {
                            deny: Some(vec!["force".into()]),
                            allow: None,
                            action: ParamViolationAction::Strip,
                        },
                    )]),
                }),
            },
        );
        let mut rules2 = HashMap::new();
        rules2.insert(
            "github".into(),
            super::super::config::ToolProviderPolicy {
                filter: None,
                param_restrictions: Some(ParamRestrictions {
                    rules: HashMap::from([(
                        "search".into(),
                        ParamRule {
                            deny: Some(vec!["debug".into()]),
                            allow: None,
                            action: ParamViolationAction::Reject,
                        },
                    )]),
                }),
            },
        );
        let cache = PolicyCache {
            policies: HashMap::from([
                ("p1".into(), make_policy_file("p1", rules1)),
                ("p2".into(), make_policy_file("p2", rules2)),
            ]),
        };

        let resolved = cache
            .resolve_tool_rules(&["p1".into(), "p2".into()], "github")
            .expect("should resolve");
        let restrictions = resolved
            .param_restrictions
            .expect("should have restrictions");
        let rule = &restrictions.rules["search"];
        // Deny union: both force and debug denied.
        assert!(
            rule.deny
                .as_ref()
                .is_some_and(|d| d.contains(&"force".into()))
        );
        assert!(
            rule.deny
                .as_ref()
                .is_some_and(|d| d.contains(&"debug".into()))
        );
        // Strictest action wins.
        assert!(matches!(rule.action, ParamViolationAction::Reject));
    }
}
