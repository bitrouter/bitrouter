//! Named groups of MCP servers for access control convenience.
//!
//! Groups resolve at keygen time — JWT claims stay concrete server patterns.
//! This allows administrators to define named collections of servers (e.g.
//! `dev_tools: [github, jira]`) and reference them in `--tools` flags.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Named groups of MCP servers for access control convenience.
///
/// Groups resolve at keygen time — JWT claims stay concrete.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpAccessGroups {
    #[serde(flatten)]
    groups: HashMap<String, Vec<String>>,
}

impl McpAccessGroups {
    /// Expand patterns that reference group names into concrete server patterns.
    ///
    /// For each input pattern, split on first `/`:
    /// - If the prefix matches a group name, expand to one pattern per server
    ///   in the group, preserving the suffix.
    ///   e.g. `"dev_tools/*"` → `["github/*", "jira/*"]` if dev_tools=[github,jira].
    ///   e.g. `"dev_tools/search"` → `["github/search", "jira/search"]`.
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
                // Bare group name with no slash — expand to server/*
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

    /// Return all groups as a map (for admin listing).
    pub fn as_map(&self) -> &HashMap<String, Vec<String>> {
        &self.groups
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_groups() -> McpAccessGroups {
        McpAccessGroups {
            groups: HashMap::from([
                ("dev_tools".into(), vec!["github".into(), "jira".into()]),
                ("comms".into(), vec!["slack".into(), "email".into()]),
            ]),
        }
    }

    #[test]
    fn expand_single_group_with_wildcard() {
        let groups = test_groups();
        let patterns = vec!["dev_tools/*".into()];
        let mut expanded = groups.expand_patterns(&patterns);
        expanded.sort();
        assert_eq!(expanded, vec!["github/*", "jira/*"]);
    }

    #[test]
    fn expand_multi_server_group() {
        let groups = test_groups();
        let patterns = vec!["comms/*".into()];
        let mut expanded = groups.expand_patterns(&patterns);
        expanded.sort();
        assert_eq!(expanded, vec!["email/*", "slack/*"]);
    }

    #[test]
    fn non_group_passthrough() {
        let groups = test_groups();
        let patterns = vec!["direct_server/tool".into()];
        let expanded = groups.expand_patterns(&patterns);
        assert_eq!(expanded, vec!["direct_server/tool"]);
    }

    #[test]
    fn mixed_patterns() {
        let groups = test_groups();
        let patterns = vec!["dev_tools/*".into(), "direct/sometool".into()];
        let expanded = groups.expand_patterns(&patterns);
        // dev_tools expands to github/*, jira/*; direct passes through
        assert!(expanded.contains(&"github/*".to_string()));
        assert!(expanded.contains(&"jira/*".to_string()));
        assert!(expanded.contains(&"direct/sometool".to_string()));
        assert_eq!(expanded.len(), 3);
    }

    #[test]
    fn pattern_with_tool_suffix() {
        let groups = test_groups();
        let patterns = vec!["dev_tools/search".into()];
        let mut expanded = groups.expand_patterns(&patterns);
        expanded.sort();
        assert_eq!(expanded, vec!["github/search", "jira/search"]);
    }

    #[test]
    fn bare_group_name_expands_to_wildcard() {
        let groups = test_groups();
        let patterns = vec!["dev_tools".into()];
        let mut expanded = groups.expand_patterns(&patterns);
        expanded.sort();
        assert_eq!(expanded, vec!["github/*", "jira/*"]);
    }

    #[test]
    fn empty_groups_no_expansion() {
        let groups = McpAccessGroups::default();
        let patterns = vec!["anything/*".into()];
        let expanded = groups.expand_patterns(&patterns);
        assert_eq!(expanded, vec!["anything/*"]);
    }

    #[test]
    fn empty_patterns_returns_empty() {
        let groups = test_groups();
        let expanded = groups.expand_patterns(&[]);
        assert!(expanded.is_empty());
    }

    #[test]
    fn contains_and_servers() {
        let groups = test_groups();
        assert!(groups.contains("dev_tools"));
        assert!(!groups.contains("nonexistent"));
        assert_eq!(
            groups.servers("comms").map(|s| {
                let mut v = s.to_vec();
                v.sort();
                v
            }),
            Some(vec!["email".into(), "slack".into()])
        );
        assert!(groups.servers("nonexistent").is_none());
    }

    #[test]
    fn serde_roundtrip() {
        let json = r#"{
            "dev_tools": ["github", "jira"],
            "comms": ["slack"]
        }"#;
        let groups: McpAccessGroups = serde_json::from_str(json).unwrap_or_default();
        assert!(groups.contains("dev_tools"));
        assert_eq!(
            groups.servers("dev_tools").map(|s: &[String]| s.len()),
            Some(2)
        );
        assert_eq!(groups.servers("comms").map(|s: &[String]| s.len()), Some(1));
    }
}
