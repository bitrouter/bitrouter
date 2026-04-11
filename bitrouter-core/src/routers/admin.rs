//! Admin types and traits for runtime management of routes and tools.
//!
//! Provides extension traits that layer admin (mutation / inspection) capabilities
//! on top of the read-only discovery traits in [`registry`](super::registry):
//!
//! | Entity  | Discovery trait    | Admin trait            |
//! |---------|--------------------|------------------------|
//! | Models  | `RoutingTable`     | `AdminRoutingTable`    |
//! | Tools   | `ToolRegistry`     | `AdminToolRegistry`    |

use std::collections::HashMap;
use std::future::Future;

use serde::{Deserialize, Serialize};

use crate::errors::Result;

use super::registry::ToolRegistry;
use super::routing_table::{ApiProtocol, RoutingTable};

/// A single endpoint in a dynamic route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteEndpoint {
    /// Provider name (must exist in the providers section or built-ins).
    pub provider: String,
    /// Upstream service identifier (model ID or tool ID).
    #[serde(alias = "model_id")]
    pub service_id: String,
    /// API protocol for this endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_protocol: Option<ApiProtocol>,
}

/// Whether a route targets a model or a tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteKind {
    /// Route resolves to a language model endpoint.
    #[default]
    Model,
    /// Route resolves to a tool endpoint.
    Tool,
    /// Route resolves to an agent endpoint.
    Agent,
}

/// Strategy for distributing requests across multiple endpoints.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteStrategy {
    /// Try endpoints in declared order.
    #[default]
    Priority,
    /// Distribute requests evenly via round-robin.
    LoadBalance,
}

/// A dynamically-configured route definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DynamicRoute {
    /// The virtual service name (e.g. "research", "fast").
    #[serde(alias = "model")]
    pub name: String,
    /// Whether this route targets a model or a tool.
    #[serde(default)]
    pub kind: RouteKind,
    /// Routing strategy across endpoints.
    #[serde(default)]
    pub strategy: RouteStrategy,
    /// One or more upstream endpoints to route to.
    pub endpoints: Vec<RouteEndpoint>,
}

/// Extension trait for routing tables that support runtime route management.
///
/// Implementations store dynamic routes separately from config-defined routes.
/// Dynamic routes take precedence during resolution.
pub trait AdminRoutingTable: RoutingTable {
    /// Create or update a dynamic route.
    fn add_route(&self, route: DynamicRoute) -> Result<()>;

    /// Remove a dynamically-added route. Returns `true` if the route existed.
    ///
    /// Config-defined routes cannot be removed.
    fn remove_route(&self, name: &str) -> Result<bool>;

    /// List all dynamically-added routes.
    fn list_dynamic_routes(&self) -> Vec<DynamicRoute>;
}

// ── Tool admin ──────────────────────────────────────────────────────

/// Allow-list filter applied to an upstream tool server.
///
/// When `allow` is `None`, all tools are visible. When `Some`, only tools
/// whose un-namespaced name appears in the list are visible.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolFilter {
    /// If set, only tools whose un-namespaced name appears in this list are visible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
}

impl ToolFilter {
    /// Returns `true` if `tool_name` (un-namespaced) passes this filter.
    pub fn accepts(&self, tool_name: &str) -> bool {
        match &self.allow {
            Some(allow) => allow.iter().any(|a| a == tool_name),
            None => true,
        }
    }
}

/// Metadata about a connected upstream tool server.
#[derive(Debug, Clone, Serialize)]
pub struct ToolUpstreamEntry {
    /// Server name.
    pub name: String,
    /// Number of tools currently visible (after filtering).
    pub tool_count: usize,
    /// Active tool filter, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<ToolFilter>,
}

/// Admin interface for inspecting tool registries at runtime.
///
/// Parallel to [`AdminRoutingTable`] for models. Extends [`ToolRegistry`]
/// with methods for inspecting upstream servers. Policy mutations (filters,
/// parameter restrictions) live in [`ToolPolicyAdmin`].
pub trait AdminToolRegistry: ToolRegistry {
    /// List all upstream tool servers with their current state.
    fn list_upstreams(&self) -> impl Future<Output = Vec<ToolUpstreamEntry>> + Send;
}

/// Admin interface for mutating tool visibility policy at runtime.
///
/// Separate from [`AdminToolRegistry`] (which manages routing topology)
/// because policy mutations are the responsibility of the policy wrapper
/// layer, not the routing layer.
pub trait ToolPolicyAdmin: ToolRegistry {
    /// Update the tool filter for a specific upstream server.
    fn update_filter(
        &self,
        server: &str,
        filter: Option<ToolFilter>,
    ) -> impl Future<Output = Result<()>> + Send;
}

impl<T: AdminToolRegistry> AdminToolRegistry for std::sync::Arc<T> {
    async fn list_upstreams(&self) -> Vec<ToolUpstreamEntry> {
        (**self).list_upstreams().await
    }
}

impl<T: ToolPolicyAdmin> ToolPolicyAdmin for std::sync::Arc<T> {
    async fn update_filter(&self, server: &str, filter: Option<ToolFilter>) -> Result<()> {
        (**self).update_filter(server, filter).await
    }
}

/// Per-caller policy resolution for tool access control.
///
/// Implementations load policy files and resolve the tool allow-list for
/// callers identified by a single policy ID. The MCP filter layer uses
/// this trait to enforce per-caller tool visibility.
pub trait ToolPolicyResolver: Send + Sync {
    /// Resolve [`ToolFilter`]s for the given policy.
    ///
    /// Returns a map of provider name → filter. Providers not mentioned
    /// in the policy are absent (meaning all-allow).
    fn resolve_filters(&self, policy_id: &str) -> HashMap<String, ToolFilter>;

    /// Resolve the tool filter for a specific provider in the given policy.
    fn resolve_tool_filter(&self, policy_id: &str, provider: &str) -> Option<ToolFilter>;
}
