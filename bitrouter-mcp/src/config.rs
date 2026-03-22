use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::McpGatewayError;

// Re-export core admin types used in MCP config.
pub use bitrouter_core::routers::admin::{
    ParamRestrictions, ParamRule, ParamViolationAction, ToolFilter,
};

/// Configuration for a single upstream MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
    #[serde(default)]
    pub tool_filter: Option<ToolFilter>,
    #[serde(default)]
    pub cost: ToolCostConfig,
    #[serde(default)]
    pub param_restrictions: ParamRestrictions,
}

/// Cost configuration for tool invocations on an MCP server.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolCostConfig {
    /// Default cost per tool invocation (USD).
    #[serde(default)]
    pub default_cost_per_query: f64,
    /// Per-tool cost overrides. Keys are un-namespaced tool names.
    #[serde(default)]
    pub tool_costs: HashMap<String, f64>,
}

impl ToolCostConfig {
    /// Return the cost for a given tool, falling back to the default.
    pub fn cost_for(&self, tool_name: &str) -> f64 {
        self.tool_costs
            .get(tool_name)
            .copied()
            .unwrap_or(self.default_cost_per_query)
    }
}

impl McpServerConfig {
    /// Validate this configuration, returning an error if it is invalid.
    pub fn validate(&self) -> Result<(), McpGatewayError> {
        if self.name.is_empty() {
            return Err(McpGatewayError::InvalidConfig {
                reason: "server name must not be empty".into(),
            });
        }
        if self.name.contains('/') {
            return Err(McpGatewayError::InvalidConfig {
                reason: format!("server name '{}' must not contain '/'", self.name),
            });
        }
        match &self.transport {
            McpTransport::Stdio { command, .. } => {
                if command.is_empty() {
                    return Err(McpGatewayError::InvalidConfig {
                        reason: format!("server '{}': stdio command must not be empty", self.name),
                    });
                }
            }
            McpTransport::Http { url, .. } => {
                if url.is_empty() {
                    return Err(McpGatewayError::InvalidConfig {
                        reason: format!("server '{}': http url must not be empty", self.name),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Transport type for connecting to an upstream MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpTransport {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_stdio_config(name: &str, command: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            transport: McpTransport::Stdio {
                command: command.into(),
                args: vec![],
                env: HashMap::new(),
            },
            tool_filter: None,
            cost: ToolCostConfig::default(),
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
        let config = McpServerConfig {
            name: "test".into(),
            transport: McpTransport::Http {
                url: String::new(),
                headers: HashMap::new(),
            },
            tool_filter: None,
            cost: ToolCostConfig::default(),
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
        let config = McpServerConfig {
            name: "remote".into(),
            transport: McpTransport::Http {
                url: "http://localhost:3000/mcp".into(),
                headers: HashMap::new(),
            },
            tool_filter: None,
            cost: ToolCostConfig::default(),
            param_restrictions: ParamRestrictions::default(),
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn filter_deny_takes_precedence() {
        let filter = ToolFilter {
            allow: Some(vec!["tool1".into()]),
            deny: Some(vec!["tool1".into()]),
        };
        assert!(!filter.accepts("tool1"));
    }

    #[test]
    fn filter_allow_whitelist() {
        let filter = ToolFilter {
            allow: Some(vec!["tool1".into(), "tool2".into()]),
            deny: None,
        };
        assert!(filter.accepts("tool1"));
        assert!(filter.accepts("tool2"));
        assert!(!filter.accepts("tool3"));
    }

    #[test]
    fn filter_deny_only() {
        let filter = ToolFilter {
            allow: None,
            deny: Some(vec!["secret".into()]),
        };
        assert!(!filter.accepts("secret"));
        assert!(filter.accepts("other"));
    }

    #[test]
    fn filter_no_lists_accepts_all() {
        let filter = ToolFilter {
            allow: None,
            deny: None,
        };
        assert!(filter.accepts("anything"));
    }

    #[test]
    fn serde_roundtrip_stdio() {
        let config = McpServerConfig {
            name: "test".into(),
            transport: McpTransport::Stdio {
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
        let parsed: McpServerConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.name, "test");
    }

    #[test]
    fn serde_roundtrip_http() {
        let config = McpServerConfig {
            name: "remote".into(),
            transport: McpTransport::Http {
                url: "http://localhost:3000/mcp".into(),
                headers: HashMap::from([("Authorization".into(), "Bearer tok".into())]),
            },
            tool_filter: None,
            cost: ToolCostConfig::default(),
            param_restrictions: ParamRestrictions::default(),
        };
        let json = serde_json::to_string(&config).expect("serialize");
        let parsed: McpServerConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.name, "remote");
    }

    #[test]
    fn cost_for_with_default() {
        let cost = ToolCostConfig {
            default_cost_per_query: 0.001,
            tool_costs: HashMap::new(),
        };
        assert!((cost.cost_for("anything") - 0.001).abs() < 1e-10);
    }

    #[test]
    fn cost_for_with_override() {
        let cost = ToolCostConfig {
            default_cost_per_query: 0.001,
            tool_costs: HashMap::from([("search".into(), 0.005)]),
        };
        assert!((cost.cost_for("search") - 0.005).abs() < 1e-10);
        assert!((cost.cost_for("other") - 0.001).abs() < 1e-10);
    }

    #[test]
    fn cost_for_zero_default() {
        let cost = ToolCostConfig::default();
        assert_eq!(cost.cost_for("anything"), 0.0);
    }
}
