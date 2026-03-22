//! Config-driven tool server types — protocol-agnostic YAML shapes.
//!
//! Transport and cost types are config-specific. Filter and restriction
//! types are re-exported from [`bitrouter_core::routers::admin`] so that
//! a single canonical definition is used throughout the stack.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// Re-export core admin types so config consumers get them from here.
pub use bitrouter_core::routers::admin::{
    ParamRestrictions, ParamRule, ParamViolationAction, ToolFilter,
};

/// Configuration for a single upstream tool server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolServerConfig {
    pub name: String,
    pub transport: ToolServerTransport,
    #[serde(default)]
    pub tool_filter: Option<ToolFilter>,
    #[serde(default)]
    pub cost: ToolCostConfig,
    #[serde(default)]
    pub param_restrictions: ParamRestrictions,
}

/// Transport type for connecting to an upstream tool server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolServerTransport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

/// Cost configuration for tool invocations on a server.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolCostConfig {
    /// Default cost per tool invocation (USD).
    #[serde(default)]
    pub default_cost_per_query: f64,
    /// Per-tool cost overrides. Keys are un-namespaced tool names.
    #[serde(default)]
    pub tool_costs: HashMap<String, f64>,
}

/// Named groups of tool servers for access control convenience.
///
/// Groups resolve at keygen time — JWT claims stay concrete server patterns.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolServerAccessGroups {
    #[serde(flatten)]
    groups: HashMap<String, Vec<String>>,
}

impl ToolServerAccessGroups {
    /// Expand patterns that reference group names into concrete server patterns.
    ///
    /// For each input pattern, split on first `/`:
    /// - If the prefix matches a group name, expand to one pattern per server
    ///   in the group, preserving the suffix.
    /// - If the prefix matches a group name and there is no suffix (bare group name),
    ///   expand to `"server/*"` for each server in the group.
    /// - Non-group patterns pass through unchanged.
    pub fn expand_patterns(&self, patterns: &[String]) -> Vec<String> {
        let mut result = Vec::new();
        for pattern in patterns {
            if let Some((prefix, suffix)) = pattern.split_once('/') {
                if let Some(servers) = self.groups.get(prefix) {
                    for server in servers {
                        result.push(format!("{server}/{suffix}"));
                    }
                } else {
                    result.push(pattern.clone());
                }
            } else if let Some(servers) = self.groups.get(pattern.as_str()) {
                for server in servers {
                    result.push(format!("{server}/*"));
                }
            } else {
                result.push(pattern.clone());
            }
        }
        result
    }

    /// Check if a group name exists.
    pub fn contains(&self, name: &str) -> bool {
        self.groups.contains_key(name)
    }

    /// Get the servers in a group.
    pub fn servers(&self, name: &str) -> Option<&[String]> {
        self.groups.get(name).map(|v| v.as_slice())
    }

    /// Return all groups as a map.
    pub fn as_map(&self) -> &HashMap<String, Vec<String>> {
        &self.groups
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_stdio() {
        let config = ToolServerConfig {
            name: "test".into(),
            transport: ToolServerTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "server".into()],
                env: HashMap::from([("KEY".into(), "VAL".into())]),
            },
            tool_filter: Some(ToolFilter {
                allow: Some(vec!["tool1".into()]),
                deny: None,
            }),
            cost: ToolCostConfig::default(),
            param_restrictions: ParamRestrictions::default(),
        };
        let json = serde_json::to_string(&config).expect("serialize");
        let parsed: ToolServerConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.name, "test");
    }

    #[test]
    fn serde_roundtrip_http() {
        let config = ToolServerConfig {
            name: "remote".into(),
            transport: ToolServerTransport::Http {
                url: "http://localhost:3000/mcp".into(),
                headers: HashMap::from([("Authorization".into(), "Bearer tok".into())]),
            },
            tool_filter: None,
            cost: ToolCostConfig::default(),
            param_restrictions: ParamRestrictions::default(),
        };
        let json = serde_json::to_string(&config).expect("serialize");
        let parsed: ToolServerConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.name, "remote");
    }

    #[test]
    fn access_groups_expand_patterns() {
        let groups = ToolServerAccessGroups {
            groups: HashMap::from([
                ("dev_tools".into(), vec!["github".into(), "jira".into()]),
                ("comms".into(), vec!["slack".into(), "email".into()]),
            ]),
        };

        let mut expanded = groups.expand_patterns(&["dev_tools/*".into()]);
        expanded.sort();
        assert_eq!(expanded, vec!["github/*", "jira/*"]);
    }

    #[test]
    fn access_groups_bare_name_expands_to_wildcard() {
        let groups = ToolServerAccessGroups {
            groups: HashMap::from([("dev_tools".into(), vec!["github".into(), "jira".into()])]),
        };
        let mut expanded = groups.expand_patterns(&["dev_tools".into()]);
        expanded.sort();
        assert_eq!(expanded, vec!["github/*", "jira/*"]);
    }

    #[test]
    fn access_groups_non_group_passthrough() {
        let groups = ToolServerAccessGroups::default();
        let expanded = groups.expand_patterns(&["direct_server/tool".into()]);
        assert_eq!(expanded, vec!["direct_server/tool"]);
    }

    #[test]
    fn access_groups_serde_roundtrip() {
        let json = r#"{
            "dev_tools": ["github", "jira"],
            "comms": ["slack"]
        }"#;
        let groups: ToolServerAccessGroups = serde_json::from_str(json).unwrap_or_default();
        assert!(groups.contains("dev_tools"));
        assert_eq!(
            groups.servers("dev_tools").map(|s: &[String]| s.len()),
            Some(2)
        );
    }

    #[test]
    fn param_restrictions_serde_roundtrip() {
        let json = r#"{
            "rules": {
                "delete_repo": {
                    "deny": ["force"],
                    "action": "reject"
                },
                "create_issue": {
                    "allow": ["title", "body"],
                    "action": "strip"
                }
            }
        }"#;
        let restrictions: ParamRestrictions = serde_json::from_str(json).unwrap_or_default();
        assert!(restrictions.rules.contains_key("delete_repo"));
        assert!(restrictions.rules.contains_key("create_issue"));
        assert_eq!(
            restrictions.rules["delete_repo"].action,
            ParamViolationAction::Reject
        );
        assert_eq!(
            restrictions.rules["create_issue"].action,
            ParamViolationAction::Strip
        );
    }
}
