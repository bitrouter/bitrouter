use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{CallToolResult, Tool};
use tokio::sync::{Notify, RwLock};

use bitrouter_mcp::admin::{AdminToolRegistry, ToolEntry, UpstreamInfo};
use bitrouter_mcp::config::{McpServerConfig, ToolFilter};
use bitrouter_mcp::error::McpGatewayError;
use bitrouter_mcp::groups::McpAccessGroups;
use bitrouter_mcp::param_filter::ParamRestrictions;

use super::upstream::UpstreamConnection;

/// Aggregates multiple upstream MCP connections and routes tool calls.
///
/// The inner map is wrapped in [`RwLock`] to support runtime mutation of
/// filters without restarting the gateway.
pub struct UpstreamRegistry {
    upstreams: RwLock<HashMap<String, UpstreamConnection>>,
    groups: McpAccessGroups,
}

impl UpstreamRegistry {
    /// Connect to all configured upstreams. Fails on first error or duplicate name.
    pub async fn from_configs(
        configs: Vec<McpServerConfig>,
        groups: McpAccessGroups,
    ) -> Result<Self, McpGatewayError> {
        // Check for duplicate names
        let mut seen = std::collections::HashSet::new();
        for config in &configs {
            if !seen.insert(&config.name) {
                return Err(McpGatewayError::InvalidConfig {
                    reason: format!("duplicate server name '{}'", config.name),
                });
            }
        }

        let mut upstreams = HashMap::with_capacity(configs.len());
        for config in configs {
            let name = config.name.clone();
            tracing::info!(upstream = %name, "connecting to upstream");
            let conn = UpstreamConnection::connect(config).await?;
            upstreams.insert(name, conn);
        }

        Ok(Self {
            upstreams: RwLock::new(upstreams),
            groups,
        })
    }

    /// Return the access groups.
    pub fn groups(&self) -> &McpAccessGroups {
        &self.groups
    }

    /// Merge all namespaced tools from all upstreams.
    pub async fn aggregated_tools(&self) -> Vec<Tool> {
        let upstreams = self.upstreams.read().await;
        let mut all = Vec::new();
        for upstream in upstreams.values() {
            all.extend(upstream.namespaced_tools().await);
        }
        all
    }

    /// Route a namespaced tool call to the correct upstream.
    pub async fn route_call(
        &self,
        prefixed_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<CallToolResult, McpGatewayError> {
        let (server_name, tool_name) = parse_namespaced_tool(prefixed_name)?;
        let upstreams = self.upstreams.read().await;
        let upstream = upstreams
            .get(server_name)
            .ok_or_else(|| McpGatewayError::ToolNotFound {
                name: prefixed_name.to_owned(),
            })?;
        upstream.call_tool(tool_name, arguments).await
    }

    /// Refresh the tool cache for a specific upstream.
    pub async fn refresh_upstream(&self, name: &str) -> Result<(), McpGatewayError> {
        let upstreams = self.upstreams.read().await;
        let upstream = upstreams
            .get(name)
            .ok_or_else(|| McpGatewayError::UpstreamClosed {
                name: name.to_owned(),
            })?;
        upstream.refresh_tools().await
    }

    /// Return notify handles for all upstreams, for spawning background listeners.
    pub async fn tool_change_notifiers(&self) -> Vec<(String, Arc<Notify>)> {
        let upstreams = self.upstreams.read().await;
        upstreams
            .iter()
            .map(|(name, conn)| (name.clone(), conn.tool_change_notify()))
            .collect()
    }

    /// Update the tool filter for a running upstream.
    pub async fn update_filter(
        &self,
        server_name: &str,
        filter: Option<ToolFilter>,
    ) -> Result<(), McpGatewayError> {
        let upstreams = self.upstreams.read().await;
        let upstream = upstreams
            .get(server_name)
            .ok_or_else(|| McpGatewayError::ToolNotFound {
                name: server_name.to_owned(),
            })?;
        upstream.set_filter(filter).await;
        Ok(())
    }

    /// Update the parameter restrictions for a running upstream.
    pub async fn update_param_restrictions(
        &self,
        server_name: &str,
        restrictions: ParamRestrictions,
    ) -> Result<(), McpGatewayError> {
        let upstreams = self.upstreams.read().await;
        let upstream = upstreams
            .get(server_name)
            .ok_or_else(|| McpGatewayError::ToolNotFound {
                name: server_name.to_owned(),
            })?;
        upstream.set_param_restrictions(restrictions).await;
        Ok(())
    }

    /// List all upstream servers with their tool counts and current filters.
    pub async fn list_upstreams(&self) -> Vec<UpstreamInfo> {
        let upstreams = self.upstreams.read().await;
        let mut infos = Vec::with_capacity(upstreams.len());
        for (name, conn) in upstreams.iter() {
            let restrictions = conn.param_restrictions().await;
            let has_restrictions = !restrictions.rules.is_empty();
            infos.push(UpstreamInfo {
                name: name.clone(),
                tool_count: conn.tool_count().await,
                filter: conn.filter().await,
                param_restrictions: if has_restrictions {
                    Some(restrictions)
                } else {
                    None
                },
            });
        }
        infos
    }
}

/// Implement [`AdminToolRegistry`] for the runtime registry.
impl AdminToolRegistry for UpstreamRegistry {
    async fn list_tools(&self) -> Vec<ToolEntry> {
        let tools = self.aggregated_tools().await;
        tools
            .into_iter()
            .map(|t| {
                let (server, _tool) = t
                    .name
                    .split_once('/')
                    .unwrap_or(("unknown", t.name.as_ref()));
                ToolEntry {
                    name: t.name.to_string(),
                    server: server.to_owned(),
                    description: t.description.as_deref().unwrap_or_default().to_owned(),
                    source: "config",
                }
            })
            .collect()
    }

    async fn list_upstreams(&self) -> Vec<UpstreamInfo> {
        self.list_upstreams().await
    }

    async fn update_filter(
        &self,
        server: &str,
        filter: Option<ToolFilter>,
    ) -> Result<(), McpGatewayError> {
        self.update_filter(server, filter).await
    }

    async fn list_groups(&self) -> HashMap<String, Vec<String>> {
        self.groups().as_map().clone()
    }

    async fn update_param_restrictions(
        &self,
        server: &str,
        restrictions: ParamRestrictions,
    ) -> Result<(), McpGatewayError> {
        self.update_param_restrictions(server, restrictions).await
    }
}

/// Split a namespaced tool name `server/tool` on the first `/`.
fn parse_namespaced_tool(name: &str) -> Result<(&str, &str), McpGatewayError> {
    name.split_once('/')
        .ok_or_else(|| McpGatewayError::ToolNotFound {
            name: name.to_owned(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_splits_on_first_slash() {
        let (server, tool) = parse_namespaced_tool("myserver/mytool").expect("valid");
        assert_eq!(server, "myserver");
        assert_eq!(tool, "mytool");
    }

    #[test]
    fn parse_preserves_slashes_in_tool_name() {
        let (server, tool) = parse_namespaced_tool("srv/path/to/tool").expect("valid");
        assert_eq!(server, "srv");
        assert_eq!(tool, "path/to/tool");
    }

    #[test]
    fn parse_errors_on_no_slash() {
        let result = parse_namespaced_tool("noslash");
        assert!(result.is_err());
    }
}
