//! Upstream connection configuration types.
//!
//! Transport-neutral data types describing how to connect to upstream tool
//! servers and agents. Used by both `bitrouter-config` (YAML parsing) and
//! protocol crates (`bitrouter-mcp`, `bitrouter-a2a`) at runtime.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::admin::{ParamRestrictions, ToolFilter};

// ── Tool server config ──────────────────────────────────────────────

/// Configuration for a single upstream tool server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolServerConfig {
    pub name: String,
    pub transport: ToolServerTransport,
    #[serde(default)]
    pub tool_filter: Option<ToolFilter>,
    #[serde(default)]
    pub param_restrictions: ParamRestrictions,
}

impl ToolServerConfig {
    /// Validate this configuration, returning an error if it is invalid.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty() {
            return Err("server name must not be empty".into());
        }
        if self.name.contains('/') {
            return Err(format!("server name '{}' must not contain '/'", self.name));
        }
        match &self.transport {
            ToolServerTransport::Stdio { command, .. } => {
                if command.is_empty() {
                    return Err(format!(
                        "server '{}': stdio command must not be empty",
                        self.name
                    ));
                }
            }
            ToolServerTransport::Http { url, .. } => {
                if url.is_empty() {
                    return Err(format!(
                        "server '{}': http url must not be empty",
                        self.name
                    ));
                }
            }
        }
        Ok(())
    }
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

// ── Agent config ────────────────────────────────────────────────────

/// Configuration for an upstream agent to proxy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Display name for this upstream agent.
    pub name: String,

    /// Base URL of the upstream agent (used for discovery).
    pub url: String,

    /// Optional HTTP headers to send to upstream (e.g., auth tokens).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,

    /// Optional card discovery path override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card_path: Option<String>,
}

impl AgentConfig {
    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty() {
            return Err("agent name cannot be empty".to_string());
        }
        if self.name.contains('/') {
            return Err(format!("agent name '{}' cannot contain '/'", self.name));
        }
        if self.url.is_empty() {
            return Err("agent URL cannot be empty".to_string());
        }
        Ok(())
    }

    /// Get the discovery URL for this agent.
    pub fn discovery_url(&self) -> String {
        let base = self.url.trim_end_matches('/');
        let path = self
            .card_path
            .as_deref()
            .unwrap_or("/.well-known/agent-card.json");
        format!("{base}{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ToolServerConfig tests ──────────────────────────────────────

    fn test_stdio_config(name: &str, command: &str) -> ToolServerConfig {
        ToolServerConfig {
            name: name.into(),
            transport: ToolServerTransport::Stdio {
                command: command.into(),
                args: vec![],
                env: HashMap::new(),
            },
            tool_filter: None,
            param_restrictions: ParamRestrictions::default(),
        }
    }

    #[test]
    fn validate_rejects_empty_name() {
        assert!(test_stdio_config("", "echo").validate().is_err());
    }

    #[test]
    fn validate_rejects_slash_in_name() {
        assert!(test_stdio_config("a/b", "echo").validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_command() {
        assert!(test_stdio_config("test", "").validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_url() {
        let config = ToolServerConfig {
            name: "test".into(),
            transport: ToolServerTransport::Http {
                url: String::new(),
                headers: HashMap::new(),
            },
            tool_filter: None,
            param_restrictions: ParamRestrictions::default(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_accepts_valid_stdio() {
        assert!(test_stdio_config("my-server", "npx").validate().is_ok());
    }

    #[test]
    fn validate_accepts_valid_http() {
        let config = ToolServerConfig {
            name: "remote".into(),
            transport: ToolServerTransport::Http {
                url: "http://localhost:3000/mcp".into(),
                headers: HashMap::new(),
            },
            tool_filter: None,
            param_restrictions: ParamRestrictions::default(),
        };
        assert!(config.validate().is_ok());
    }

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
            param_restrictions: ParamRestrictions::default(),
        };
        let json = serde_json::to_string(&config).expect("serialize");
        let parsed: ToolServerConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.name, "remote");
    }

    // ── ToolServerAccessGroups tests ────────────────────────────────

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

    // ── AgentConfig tests ───────────────────────────────────────────

    #[test]
    fn agent_validate_rejects_empty_name() {
        let config = AgentConfig {
            name: String::new(),
            url: "http://localhost".to_string(),
            headers: HashMap::new(),
            card_path: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn agent_validate_rejects_slash_in_name() {
        let config = AgentConfig {
            name: "my/agent".to_string(),
            url: "http://localhost".to_string(),
            headers: HashMap::new(),
            card_path: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn agent_validate_rejects_empty_url() {
        let config = AgentConfig {
            name: "agent".to_string(),
            url: String::new(),
            headers: HashMap::new(),
            card_path: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn agent_validate_accepts_valid() {
        let config = AgentConfig {
            name: "test-agent".to_string(),
            url: "http://localhost:9000".to_string(),
            headers: HashMap::new(),
            card_path: None,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn agent_discovery_url_default_path() {
        let config = AgentConfig {
            name: "agent".to_string(),
            url: "https://agent.example.com".to_string(),
            headers: HashMap::new(),
            card_path: None,
        };
        assert_eq!(
            config.discovery_url(),
            "https://agent.example.com/.well-known/agent-card.json"
        );
    }

    #[test]
    fn agent_discovery_url_custom_path() {
        let config = AgentConfig {
            name: "agent".to_string(),
            url: "https://agent.example.com/".to_string(),
            headers: HashMap::new(),
            card_path: Some("/custom/card.json".to_string()),
        };
        assert_eq!(
            config.discovery_url(),
            "https://agent.example.com/custom/card.json"
        );
    }

    #[test]
    fn agent_serde_round_trip() {
        let cfg = AgentConfig {
            name: "my-agent".to_string(),
            url: "https://agent.example.com".to_string(),
            headers: HashMap::from([("Authorization".into(), "Bearer tok".into())]),
            card_path: Some("/custom/card.json".to_string()),
        };
        let json = serde_json::to_string(&cfg).expect("serialize");
        let parsed: AgentConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.name, "my-agent");
        assert_eq!(parsed.url, "https://agent.example.com");
        assert_eq!(
            parsed.headers.get("Authorization").map(String::as_str),
            Some("Bearer tok")
        );
        assert_eq!(parsed.card_path.as_deref(), Some("/custom/card.json"));
    }
}
