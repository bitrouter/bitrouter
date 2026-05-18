//! MCP upstream-transport descriptors.
//!
//! The MCP spec defines two client-→-server transports:
//!
//! - **Streamable HTTP** — JSON-RPC POSTed to a single URL, optional SSE
//!   responses for streaming results. Spec:
//!   <https://modelcontextprotocol.io/specification/2025-06-18/basic/transports#streamable-http>.
//! - **stdio** — the client launches the server as a child process and
//!   exchanges newline-delimited JSON-RPC over its stdio pipes. Spec:
//!   <https://modelcontextprotocol.io/specification/2025-06-18/basic/transports#stdio>.
//!
//! These types are always available (no `mcp` feature required) so a consumer
//! can implement a custom [`super::Executor`] against them without pulling in
//! the bundled rmcp-backed executor.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// How to dial one upstream MCP server.
///
/// Serde tag is `type: "http" | "stdio"` to match the wire shape used in
/// `bitrouter.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpTransport {
    /// Streamable HTTP transport — `POST <url>` JSON-RPC with optional SSE
    /// responses.
    Http {
        /// The MCP endpoint URL (e.g. `https://mcp.example.com/v1/mcp`).
        url: String,
        /// Static headers added to every request (e.g. `Authorization`).
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    /// Stdio transport — spawn `command` with `args` and exchange JSON-RPC
    /// over the child's stdin/stdout.
    Stdio {
        /// The program to spawn (resolved via `$PATH`).
        command: String,
        /// Arguments to pass to the child.
        #[serde(default)]
        args: Vec<String>,
        /// Extra environment variables for the child. Inherited env is kept.
        #[serde(default)]
        env: HashMap<String, String>,
    },
}

/// One configured upstream MCP server, as written in `bitrouter.yaml`.
///
/// The same `name` becomes the URL segment in `POST /mcp/{name}` and the
/// identifier the [`super::RoutingTable`] resolves against. Restrictions
/// chosen to match the v0 config schema for paste-compatibility:
///
/// - non-empty
/// - no `/` (collides with the URL path segment)
/// - not literally `sse` (reserved by the spec's deprecated transport name)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Server id. URL-safe; no slashes.
    pub name: String,
    /// Wire transport.
    pub transport: McpTransport,
}

/// Errors returned by [`McpServerConfig::validate`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum McpConfigError {
    /// The `name` field violates one of the documented restrictions.
    #[error("invalid mcp server name '{name}': {reason}")]
    InvalidName {
        /// The offending name (or empty string).
        name: String,
        /// Human-readable explanation.
        reason: String,
    },
    /// HTTP transport with an empty `url`.
    #[error("mcp server '{name}': http url must not be empty")]
    EmptyHttpUrl {
        /// The server whose URL is missing.
        name: String,
    },
    /// Stdio transport with an empty `command`.
    #[error("mcp server '{name}': stdio command must not be empty")]
    EmptyStdioCommand {
        /// The server whose command is missing.
        name: String,
    },
}

impl McpServerConfig {
    /// Verify the config is internally consistent. Called by
    /// [`super::config_routing::ConfigMcpRoutingTable`] at construction time so
    /// a malformed `bitrouter.yaml` is rejected at startup, not on first use.
    pub fn validate(&self) -> Result<(), McpConfigError> {
        if self.name.is_empty() {
            return Err(McpConfigError::InvalidName {
                name: String::new(),
                reason: "must not be empty".into(),
            });
        }
        if self.name.contains('/') {
            return Err(McpConfigError::InvalidName {
                name: self.name.clone(),
                reason: "must not contain '/'".into(),
            });
        }
        if self.name == "sse" {
            return Err(McpConfigError::InvalidName {
                name: self.name.clone(),
                reason: "reserved (deprecated transport name)".into(),
            });
        }
        match &self.transport {
            McpTransport::Http { url, .. } if url.is_empty() => Err(McpConfigError::EmptyHttpUrl {
                name: self.name.clone(),
            }),
            McpTransport::Stdio { command, .. } if command.is_empty() => {
                Err(McpConfigError::EmptyStdioCommand {
                    name: self.name.clone(),
                })
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_transport_round_trips_through_serde() {
        let cfg = McpServerConfig {
            name: "ctx7".into(),
            transport: McpTransport::Http {
                url: "https://mcp.example.com/v1/mcp".into(),
                headers: [("Authorization".to_string(), "Bearer x".to_string())]
                    .into_iter()
                    .collect(),
            },
        };
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json["transport"]["type"], "http");
        assert_eq!(json["transport"]["url"], "https://mcp.example.com/v1/mcp");
        let back: McpServerConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back.name, "ctx7");
    }

    #[test]
    fn stdio_transport_round_trips_through_serde() {
        let cfg = McpServerConfig {
            name: "local-git".into(),
            transport: McpTransport::Stdio {
                command: "uvx".into(),
                args: vec!["mcp-server-git".into()],
                env: Default::default(),
            },
        };
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json["transport"]["type"], "stdio");
        assert_eq!(json["transport"]["command"], "uvx");
        let back: McpServerConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back.name, "local-git");
    }

    #[test]
    fn validate_rejects_empty_name() {
        let cfg = McpServerConfig {
            name: String::new(),
            transport: McpTransport::Http {
                url: "https://x".into(),
                headers: Default::default(),
            },
        };
        assert!(matches!(
            cfg.validate(),
            Err(McpConfigError::InvalidName { .. })
        ));
    }

    #[test]
    fn validate_rejects_slash_in_name() {
        let cfg = McpServerConfig {
            name: "foo/bar".into(),
            transport: McpTransport::Http {
                url: "https://x".into(),
                headers: Default::default(),
            },
        };
        assert!(matches!(
            cfg.validate(),
            Err(McpConfigError::InvalidName { .. })
        ));
    }

    #[test]
    fn validate_rejects_reserved_sse_name() {
        let cfg = McpServerConfig {
            name: "sse".into(),
            transport: McpTransport::Http {
                url: "https://x".into(),
                headers: Default::default(),
            },
        };
        assert!(matches!(
            cfg.validate(),
            Err(McpConfigError::InvalidName { .. })
        ));
    }

    #[test]
    fn validate_rejects_empty_http_url() {
        let cfg = McpServerConfig {
            name: "x".into(),
            transport: McpTransport::Http {
                url: String::new(),
                headers: Default::default(),
            },
        };
        assert!(matches!(
            cfg.validate(),
            Err(McpConfigError::EmptyHttpUrl { .. })
        ));
    }

    #[test]
    fn validate_rejects_empty_stdio_command() {
        let cfg = McpServerConfig {
            name: "x".into(),
            transport: McpTransport::Stdio {
                command: String::new(),
                args: vec![],
                env: Default::default(),
            },
        };
        assert!(matches!(
            cfg.validate(),
            Err(McpConfigError::EmptyStdioCommand { .. })
        ));
    }

    #[test]
    fn validate_accepts_well_formed_http_and_stdio() {
        assert!(
            McpServerConfig {
                name: "ctx7".into(),
                transport: McpTransport::Http {
                    url: "https://x".into(),
                    headers: Default::default(),
                },
            }
            .validate()
            .is_ok()
        );
        assert!(
            McpServerConfig {
                name: "git".into(),
                transport: McpTransport::Stdio {
                    command: "uvx".into(),
                    args: vec!["mcp-server-git".into()],
                    env: Default::default(),
                },
            }
            .validate()
            .is_ok()
        );
    }
}
