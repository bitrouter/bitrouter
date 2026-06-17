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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpServerConfig {
    /// Server id. URL-safe; no slashes.
    pub name: String,
    /// Wire transport.
    pub transport: McpTransport,
    /// Whether this server participates in the aggregate fan-out endpoint
    /// (typically `POST /mcp`). Default: `true`.
    #[serde(default = "default_true")]
    pub aggregate: bool,
    /// Prefix prepended to upstream tool/prompt names when this server
    /// participates in aggregate fan-out. When `None`, the config-load layer
    /// fills in `{server_name}__`.
    #[serde(default)]
    pub tool_prefix: Option<String>,
}

fn default_true() -> bool {
    true
}

impl McpServerConfig {
    /// Build a server config with default aggregation settings (`aggregate:
    /// true`, `tool_prefix: None` so the config-load layer fills in
    /// `{name}__`). Convenience for tests and programmatic config builders.
    pub fn with_defaults(name: impl Into<String>, transport: McpTransport) -> Self {
        Self {
            name: name.into(),
            transport,
            aggregate: true,
            tool_prefix: None,
        }
    }
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
    /// HTTP transport whose `url` failed the upstream SSRF / scheme check.
    #[error("mcp server '{name}': http url rejected: {reason}")]
    UnsafeHttpUrl {
        /// The server whose URL was rejected.
        name: String,
        /// Why it was rejected, from [`crate::url_validator::validate_upstream_url`].
        reason: String,
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
            // A configured MCP endpoint is dialled with any static `headers`
            // (e.g. `Authorization`) attached, so an attacker-editable
            // `bitrouter.yaml` pointing at `http://169.254.169.254/` or an
            // internal host is the same SSRF risk as a provider `api_base`.
            // Gate it through the same validator.
            McpTransport::Http { url, .. } => crate::url_validator::validate_upstream_url(url)
                .map_err(|e| McpConfigError::UnsafeHttpUrl {
                    name: self.name.clone(),
                    reason: e.to_string(),
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
        let cfg = McpServerConfig::with_defaults(
            "ctx7",
            McpTransport::Http {
                url: "https://mcp.example.com/v1/mcp".into(),
                headers: [("Authorization".to_string(), "Bearer x".to_string())]
                    .into_iter()
                    .collect(),
            },
        );
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json["transport"]["type"], "http");
        assert_eq!(json["transport"]["url"], "https://mcp.example.com/v1/mcp");
        let back: McpServerConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back.name, "ctx7");
    }

    #[test]
    fn stdio_transport_round_trips_through_serde() {
        let cfg = McpServerConfig::with_defaults(
            "local-git",
            McpTransport::Stdio {
                command: "uvx".into(),
                args: vec!["mcp-server-git".into()],
                env: Default::default(),
            },
        );
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json["transport"]["type"], "stdio");
        assert_eq!(json["transport"]["command"], "uvx");
        let back: McpServerConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back.name, "local-git");
    }

    #[test]
    fn aggregate_and_tool_prefix_defaults_when_omitted() {
        // Bare entry — `aggregate` and `tool_prefix` are absent in the YAML
        // shape but must materialise as the documented defaults.
        let json = serde_json::json!({
            "name": "x",
            "transport": { "type": "http", "url": "https://x" }
        });
        let cfg: McpServerConfig = serde_json::from_value(json).unwrap();
        assert!(cfg.aggregate, "aggregate must default to true");
        assert!(
            cfg.tool_prefix.is_none(),
            "tool_prefix must default to None so the config-load layer fills in '{{name}}__'"
        );
    }

    #[test]
    fn aggregate_opt_out_round_trips() {
        let json = serde_json::json!({
            "name": "linear",
            "transport": { "type": "http", "url": "https://linear" },
            "aggregate": false,
            "tool_prefix": "lin__"
        });
        let cfg: McpServerConfig = serde_json::from_value(json).unwrap();
        assert!(!cfg.aggregate);
        assert_eq!(cfg.tool_prefix.as_deref(), Some("lin__"));
    }

    #[test]
    fn validate_rejects_empty_name() {
        let cfg = McpServerConfig::with_defaults(
            String::new(),
            McpTransport::Http {
                url: "https://x".into(),
                headers: Default::default(),
            },
        );
        assert!(matches!(
            cfg.validate(),
            Err(McpConfigError::InvalidName { .. })
        ));
    }

    #[test]
    fn validate_rejects_slash_in_name() {
        let cfg = McpServerConfig::with_defaults(
            "foo/bar",
            McpTransport::Http {
                url: "https://x".into(),
                headers: Default::default(),
            },
        );
        assert!(matches!(
            cfg.validate(),
            Err(McpConfigError::InvalidName { .. })
        ));
    }

    #[test]
    fn validate_rejects_reserved_sse_name() {
        let cfg = McpServerConfig::with_defaults(
            "sse",
            McpTransport::Http {
                url: "https://x".into(),
                headers: Default::default(),
            },
        );
        assert!(matches!(
            cfg.validate(),
            Err(McpConfigError::InvalidName { .. })
        ));
    }

    #[test]
    fn validate_rejects_empty_http_url() {
        let cfg = McpServerConfig::with_defaults(
            "x",
            McpTransport::Http {
                url: String::new(),
                headers: Default::default(),
            },
        );
        assert!(matches!(
            cfg.validate(),
            Err(McpConfigError::EmptyHttpUrl { .. })
        ));
    }

    #[test]
    fn validate_rejects_empty_stdio_command() {
        let cfg = McpServerConfig::with_defaults(
            "x",
            McpTransport::Stdio {
                command: String::new(),
                args: vec![],
                env: Default::default(),
            },
        );
        assert!(matches!(
            cfg.validate(),
            Err(McpConfigError::EmptyStdioCommand { .. })
        ));
    }

    #[test]
    fn validate_accepts_well_formed_http_and_stdio() {
        assert!(
            McpServerConfig::with_defaults(
                "ctx7",
                McpTransport::Http {
                    url: "https://x".into(),
                    headers: Default::default(),
                },
            )
            .validate()
            .is_ok()
        );
        assert!(
            McpServerConfig::with_defaults(
                "git",
                McpTransport::Stdio {
                    command: "uvx".into(),
                    args: vec!["mcp-server-git".into()],
                    env: Default::default(),
                },
            )
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn validate_rejects_ssrf_http_url() {
        // Metadata-service, link-local, private-range, and bad-scheme URLs are
        // refused with the same gate the provider `api_base` uses.
        for url in [
            "http://169.254.169.254/",
            "http://metadata.google.internal/",
            "http://10.0.0.5/mcp",
            "file:///etc/passwd",
        ] {
            let cfg = McpServerConfig::with_defaults(
                "rogue",
                McpTransport::Http {
                    url: url.into(),
                    headers: Default::default(),
                },
            );
            assert!(
                matches!(cfg.validate(), Err(McpConfigError::UnsafeHttpUrl { .. })),
                "{url} should be rejected"
            );
        }
    }

    #[test]
    fn validate_accepts_https_and_loopback_mcp_url() {
        // Public https and loopback http stay valid — local MCP servers are a
        // first-class use case and must not be broken by the SSRF gate.
        for url in [
            "https://mcp.example.com/v1/mcp",
            "http://127.0.0.1:3000/mcp",
            "http://localhost/mcp",
        ] {
            let cfg = McpServerConfig::with_defaults(
                "ok",
                McpTransport::Http {
                    url: url.into(),
                    headers: Default::default(),
                },
            );
            assert!(cfg.validate().is_ok(), "{url} should be accepted");
        }
    }
}
