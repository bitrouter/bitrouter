//! Routing table backed by the `mcp_servers` map in `bitrouter.yaml`.
//!
//! Pure lookup: [`ServerSelector`] → [`McpTarget`]. No discovery, no per-caller
//! ACL — those belong in a [`super::PreRequestHook`] or [`super::RouteHook`].

use std::collections::HashMap;

use async_trait::async_trait;

use super::transport::{McpServerConfig, McpTransport};
use super::{AggregateMember, McpTarget, RoutingTable, ServerSelector};
use crate::caller::CallerContext;
use crate::error::{BitrouterError, Result};

/// One entry's view of the per-server aggregate config — what the binary's
/// assembly hands to [`ConfigMcpRoutingTable::from_configs`] alongside the
/// transport. Captured here so the binary's config-parsing layer doesn't have
/// to know the SDK's internal types.
#[derive(Debug, Clone)]
pub struct McpServerAggregateConfig {
    /// Whether this server participates in the aggregate fan-out endpoint.
    /// Default: `true`.
    pub aggregate: bool,
    /// Prefix prepended to upstream tool/prompt names when this server
    /// participates in aggregate fan-out. Default: `{server_name}__`.
    pub tool_prefix: String,
}

impl McpServerAggregateConfig {
    /// Defaults for one server: aggregate on, prefix `{server_name}__`.
    pub fn default_for(server_name: &str) -> Self {
        Self {
            aggregate: true,
            tool_prefix: format!("{server_name}__"),
        }
    }
}

/// In-memory routing data populated from config at startup.
#[derive(Debug, Default, Clone)]
pub struct ConfigMcpRoutingTable {
    servers: HashMap<String, McpTransport>,
    aggregate_members: Vec<AggregateMember>,
}

impl ConfigMcpRoutingTable {
    /// Build from a `name → (config, aggregate_config)` map. Validates every
    /// entry; the first invalid entry stops the build so a startup
    /// misconfiguration is loud, not silent.
    ///
    /// `aggregate_config` is the per-server `aggregate` / `tool_prefix`
    /// settings. Pass [`McpServerAggregateConfig::default_for`] if the binary
    /// has no per-server overrides.
    pub fn from_configs<I>(entries: I) -> Result<Self>
    where
        I: IntoIterator<Item = (String, McpServerConfig, McpServerAggregateConfig)>,
    {
        let mut servers = HashMap::new();
        let mut members: Vec<AggregateMember> = Vec::new();
        let mut seen_prefixes: HashMap<String, String> = HashMap::new();
        for (key, cfg, agg) in entries {
            cfg.validate()
                .map_err(|e| BitrouterError::internal(e.to_string()))?;
            if agg.aggregate {
                if let Some(other) = seen_prefixes.get(&agg.tool_prefix) {
                    return Err(BitrouterError::internal(format!(
                        "mcp aggregate: servers '{other}' and '{key}' share tool_prefix \
                         '{}' — prefixes must be unique among aggregate members",
                        agg.tool_prefix
                    )));
                }
                seen_prefixes.insert(agg.tool_prefix.clone(), key.clone());
                members.push(AggregateMember {
                    server_name: key.clone(),
                    tool_prefix: agg.tool_prefix.clone(),
                    transport: cfg.transport.clone(),
                });
            }
            // The `name` field is allowed to differ from the map key (operators
            // sometimes alias) — the map key wins for routing lookups, since
            // that's what `/mcp/{server}` hits.
            servers.insert(key, cfg.transport);
        }
        Ok(Self {
            servers,
            aggregate_members: members,
        })
    }

    /// True if no servers are configured. The binary uses this to decide
    /// whether to call `app_builder.mcp(...)` at all.
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }
}

#[async_trait]
impl RoutingTable for ConfigMcpRoutingTable {
    async fn resolve(
        &self,
        selector: &ServerSelector,
        _caller: &CallerContext,
    ) -> Result<McpTarget> {
        match selector {
            ServerSelector::Direct(server) => {
                let transport = self.servers.get(server).cloned().ok_or_else(|| {
                    BitrouterError::NotFound(format!("no mcp server configured for '{server}'"))
                })?;
                Ok(McpTarget::Direct {
                    server_name: server.clone(),
                    transport,
                })
            }
            ServerSelector::Aggregate => Ok(McpTarget::Aggregate {
                members: self.aggregate_members.clone(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(name: &str, url: &str) -> (String, McpServerConfig, McpServerAggregateConfig) {
        (
            name.to_string(),
            McpServerConfig::with_defaults(
                name,
                McpTransport::Http {
                    url: url.to_string(),
                    headers: Default::default(),
                },
            ),
            McpServerAggregateConfig::default_for(name),
        )
    }

    fn caller() -> CallerContext {
        CallerContext::new("k", "u")
    }

    #[tokio::test]
    async fn resolves_configured_server() {
        let table =
            ConfigMcpRoutingTable::from_configs([server("ctx7", "https://mcp.example.com/v1/mcp")])
                .unwrap();
        let target = table
            .resolve(&ServerSelector::Direct("ctx7".into()), &caller())
            .await
            .unwrap();
        match target {
            McpTarget::Direct {
                server_name,
                transport,
            } => {
                assert_eq!(server_name, "ctx7");
                match transport {
                    McpTransport::Http { url, .. } => {
                        assert_eq!(url, "https://mcp.example.com/v1/mcp");
                    }
                    _ => panic!("expected Http transport"),
                }
            }
            _ => panic!("expected Direct target"),
        }
    }

    #[tokio::test]
    async fn unknown_server_returns_not_found() {
        let table = ConfigMcpRoutingTable::from_configs([server("ctx7", "https://x")]).unwrap();
        let err = table
            .resolve(&ServerSelector::Direct("missing".into()), &caller())
            .await
            .unwrap_err();
        assert_eq!(err.status(), 404);
    }

    #[tokio::test]
    async fn aggregate_selector_returns_all_opted_in_members() {
        let table = ConfigMcpRoutingTable::from_configs([
            server("a", "https://a"),
            server("b", "https://b"),
        ])
        .unwrap();
        let target = table
            .resolve(&ServerSelector::Aggregate, &caller())
            .await
            .unwrap();
        match target {
            McpTarget::Aggregate { members } => {
                assert_eq!(members.len(), 2);
                let mut prefixes: Vec<_> = members.iter().map(|m| m.tool_prefix.clone()).collect();
                prefixes.sort();
                assert_eq!(prefixes, vec!["a__".to_string(), "b__".to_string()]);
            }
            _ => panic!("expected Aggregate target"),
        }
    }

    #[tokio::test]
    async fn aggregate_excludes_opted_out_servers() {
        let (_, cfg_a, _) = server("a", "https://a");
        let (_, cfg_b, _) = server("b", "https://b");
        let table = ConfigMcpRoutingTable::from_configs([
            (
                "a".to_string(),
                cfg_a,
                McpServerAggregateConfig {
                    aggregate: false,
                    tool_prefix: "a__".into(),
                },
            ),
            (
                "b".to_string(),
                cfg_b,
                McpServerAggregateConfig::default_for("b"),
            ),
        ])
        .unwrap();
        let target = table
            .resolve(&ServerSelector::Aggregate, &caller())
            .await
            .unwrap();
        match target {
            McpTarget::Aggregate { members } => {
                assert_eq!(members.len(), 1);
                assert_eq!(members[0].server_name, "b");
            }
            _ => panic!("expected Aggregate target"),
        }
    }

    #[test]
    fn duplicate_prefix_is_rejected() {
        let (_, cfg_a, _) = server("a", "https://a");
        let (_, cfg_b, _) = server("b", "https://b");
        let err = ConfigMcpRoutingTable::from_configs([
            (
                "a".to_string(),
                cfg_a,
                McpServerAggregateConfig {
                    aggregate: true,
                    tool_prefix: "shared__".into(),
                },
            ),
            (
                "b".to_string(),
                cfg_b,
                McpServerAggregateConfig {
                    aggregate: true,
                    tool_prefix: "shared__".into(),
                },
            ),
        ])
        .unwrap_err();
        assert!(
            err.to_string().contains("share tool_prefix"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn invalid_entry_fails_construction() {
        let bad = (
            "bad".to_string(),
            McpServerConfig::with_defaults(
                "bad",
                McpTransport::Http {
                    url: String::new(),
                    headers: Default::default(),
                },
            ),
            McpServerAggregateConfig::default_for("bad"),
        );
        assert!(ConfigMcpRoutingTable::from_configs([bad]).is_err());
    }
}
