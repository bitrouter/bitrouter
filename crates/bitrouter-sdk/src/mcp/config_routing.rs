//! Routing table backed by the `mcp_servers` map in `bitrouter.yaml`.
//!
//! Pure lookup: server-name → [`McpTarget`]. No discovery, no per-caller
//! ACL — those belong in a [`super::PreRequestHook`] or [`super::RouteHook`].

use std::collections::HashMap;

use async_trait::async_trait;

use super::transport::{McpServerConfig, McpTransport};
use super::{McpTarget, RoutingTable};
use crate::caller::CallerContext;
use crate::error::{BitrouterError, Result};

/// In-memory `name → transport` map populated from config at startup.
#[derive(Debug, Default, Clone)]
pub struct ConfigMcpRoutingTable {
    servers: HashMap<String, McpTransport>,
}

impl ConfigMcpRoutingTable {
    /// Build from a `name → McpServerConfig` map. Validates every entry; the
    /// first invalid entry stops the build so a startup misconfiguration is
    /// loud, not silent.
    pub fn from_configs<I>(entries: I) -> Result<Self>
    where
        I: IntoIterator<Item = (String, McpServerConfig)>,
    {
        let mut servers = HashMap::new();
        for (key, cfg) in entries {
            cfg.validate()
                .map_err(|e| BitrouterError::internal(e.to_string()))?;
            // The `name` field is allowed to differ from the map key (operators
            // sometimes alias) — the map key wins for routing lookups, since
            // that's what `/mcp/{server}` hits.
            servers.insert(key, cfg.transport);
        }
        Ok(Self { servers })
    }

    /// True if no servers are configured. The binary uses this to decide
    /// whether to call `app_builder.mcp(...)` at all.
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// The configured server ids (in arbitrary order).
    pub fn server_ids(&self) -> impl Iterator<Item = &str> {
        self.servers.keys().map(String::as_str)
    }

    /// Direct transport lookup, used by CLI verbs that want to introspect
    /// configuration without going through a [`super::Pipeline`].
    pub fn lookup(&self, server: &str) -> Option<&McpTransport> {
        self.servers.get(server)
    }
}

#[async_trait]
impl RoutingTable for ConfigMcpRoutingTable {
    async fn resolve(&self, server: &str, _caller: &CallerContext) -> Result<McpTarget> {
        let transport = self.servers.get(server).cloned().ok_or_else(|| {
            BitrouterError::NotFound(format!("no mcp server configured for '{server}'"))
        })?;
        Ok(McpTarget {
            server_name: server.to_string(),
            transport,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caller::PaymentMethod;

    fn server(name: &str, url: &str) -> (String, McpServerConfig) {
        (
            name.to_string(),
            McpServerConfig {
                name: name.to_string(),
                transport: McpTransport::Http {
                    url: url.to_string(),
                    headers: Default::default(),
                },
            },
        )
    }

    fn caller() -> CallerContext {
        CallerContext::new("k", "u", PaymentMethod::None)
    }

    #[tokio::test]
    async fn resolves_configured_server() {
        let table =
            ConfigMcpRoutingTable::from_configs([server("ctx7", "https://mcp.example.com/v1/mcp")])
                .unwrap();
        let target = table.resolve("ctx7", &caller()).await.unwrap();
        assert_eq!(target.server_name, "ctx7");
        match target.transport {
            McpTransport::Http { url, .. } => {
                assert_eq!(url, "https://mcp.example.com/v1/mcp");
            }
            _ => panic!("expected Http transport"),
        }
    }

    #[tokio::test]
    async fn unknown_server_returns_not_found() {
        let table = ConfigMcpRoutingTable::from_configs([server("ctx7", "https://x")]).unwrap();
        let err = table.resolve("missing", &caller()).await.unwrap_err();
        assert_eq!(err.status(), 404);
    }

    #[test]
    fn invalid_entry_fails_construction() {
        let bad = (
            "bad".to_string(),
            McpServerConfig {
                name: "bad".into(),
                transport: McpTransport::Http {
                    url: String::new(),
                    headers: Default::default(),
                },
            },
        );
        assert!(ConfigMcpRoutingTable::from_configs([bad]).is_err());
    }

    #[test]
    fn lookup_returns_transport_for_introspection() {
        let table = ConfigMcpRoutingTable::from_configs([server("ctx7", "https://x")]).unwrap();
        assert!(matches!(
            table.lookup("ctx7"),
            Some(McpTransport::Http { .. })
        ));
        assert!(table.lookup("missing").is_none());
    }
}
