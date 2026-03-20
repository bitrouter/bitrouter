//! Runtime tool management trait — parallel to `AdminRoutingTable` for models.

use std::collections::HashMap;
use std::future::Future;

use serde::Serialize;

use crate::config::ToolFilter;
use crate::error::McpGatewayError;
use crate::param_filter::ParamRestrictions;

/// A single aggregated tool entry for admin listing.
#[derive(Debug, Clone, Serialize)]
pub struct ToolEntry {
    /// Namespaced tool name, e.g. `"github/search"`.
    pub name: String,
    /// The upstream server that owns this tool.
    pub server: String,
    /// Human-readable description of the tool.
    pub description: String,
    /// Where this tool was registered from.
    pub source: &'static str,
}

/// Metadata about a connected upstream server.
#[derive(Debug, Clone, Serialize)]
pub struct UpstreamInfo {
    pub name: String,
    pub tool_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<ToolFilter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param_restrictions: Option<ParamRestrictions>,
}

/// Trait for runtime tool registry management.
///
/// Implementors provide read and write access to the set of MCP tools
/// available through the gateway. Parallel to `AdminRoutingTable` for models.
pub trait AdminToolRegistry: Send + Sync {
    /// List all aggregated tools across all upstreams.
    fn list_tools(&self) -> impl Future<Output = Vec<ToolEntry>> + Send;
    /// List all upstream servers with their tool counts and filter configs.
    fn list_upstreams(&self) -> impl Future<Output = Vec<UpstreamInfo>> + Send;
    /// Update the tool filter for a specific upstream server.
    fn update_filter(
        &self,
        server: &str,
        filter: Option<ToolFilter>,
    ) -> impl Future<Output = Result<(), McpGatewayError>> + Send;
    /// List all configured access groups.
    fn list_groups(&self) -> impl Future<Output = HashMap<String, Vec<String>>> + Send;
    /// Update parameter restrictions for a specific upstream server.
    fn update_param_restrictions(
        &self,
        server: &str,
        restrictions: ParamRestrictions,
    ) -> impl Future<Output = Result<(), McpGatewayError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_entry_serializes() {
        let entry = ToolEntry {
            name: "github/search".to_string(),
            server: "github".to_string(),
            description: "Search GitHub".to_string(),
            source: "config",
        };
        let json = serde_json::to_value(&entry).expect("serialize");
        assert_eq!(json["name"], "github/search");
        assert_eq!(json["server"], "github");
        assert_eq!(json["source"], "config");
    }

    #[test]
    fn upstream_info_serializes() {
        let info = UpstreamInfo {
            name: "github".to_string(),
            tool_count: 3,
            filter: None,
            param_restrictions: None,
        };
        let json = serde_json::to_value(&info).expect("serialize");
        assert_eq!(json["name"], "github");
        assert_eq!(json["tool_count"], 3);
        // None fields should be omitted
        assert!(json.get("filter").is_none());
        assert!(json.get("param_restrictions").is_none());
    }
}
