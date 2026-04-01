//! MCP server connection configuration.
//!
//! Provider-specific config types that describe how to connect to an upstream
//! MCP server. These are the MCP equivalent of `OpenAiConfig` / `AnthropicConfig`
//! for model providers.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Configuration for connecting to a single upstream MCP server.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    /// Unique server name (used for namespacing tools, resources, and prompts).
    pub name: String,
    /// Transport configuration (HTTP or stdio).
    pub transport: McpServerTransport,
}

impl McpServerConfig {
    /// Validate this configuration, returning an error if it is invalid.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty() {
            return Err("server name must not be empty".into());
        }
        if self.name.contains('/') {
            return Err(format!("server name '{}' must not contain '/'", self.name));
        }
        if self.name == "sse" {
            return Err("server name 'sse' is reserved".into());
        }
        match &self.transport {
            McpServerTransport::Http { url, .. } => {
                if url.is_empty() {
                    return Err(format!(
                        "server '{}': http url must not be empty",
                        self.name
                    ));
                }
            }
            McpServerTransport::Stdio { command, .. } => {
                if command.is_empty() {
                    return Err(format!(
                        "server '{}': stdio command must not be empty",
                        self.name
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Transport type for connecting to an upstream MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServerTransport {
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_http_config() {
        let config = McpServerConfig {
            name: "test".into(),
            transport: McpServerTransport::Http {
                url: "https://example.com".into(),
                headers: HashMap::new(),
            },
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn empty_name_rejected() {
        let config = McpServerConfig {
            name: String::new(),
            transport: McpServerTransport::Http {
                url: "https://example.com".into(),
                headers: HashMap::new(),
            },
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn slash_in_name_rejected() {
        let config = McpServerConfig {
            name: "a/b".into(),
            transport: McpServerTransport::Http {
                url: "https://example.com".into(),
                headers: HashMap::new(),
            },
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn empty_url_rejected() {
        let config = McpServerConfig {
            name: "test".into(),
            transport: McpServerTransport::Http {
                url: String::new(),
                headers: HashMap::new(),
            },
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn valid_stdio_config() {
        let config = McpServerConfig {
            name: "local".into(),
            transport: McpServerTransport::Stdio {
                command: "uvx".into(),
                args: vec!["mcp-server-git".into()],
            },
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn empty_stdio_command_rejected() {
        let config = McpServerConfig {
            name: "local".into(),
            transport: McpServerTransport::Stdio {
                command: String::new(),
                args: vec![],
            },
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn sse_name_reserved() {
        let config = McpServerConfig {
            name: "sse".into(),
            transport: McpServerTransport::Http {
                url: "https://example.com".into(),
                headers: HashMap::new(),
            },
        };
        assert!(config.validate().is_err());
    }
}
