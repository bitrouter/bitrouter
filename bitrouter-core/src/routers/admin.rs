//! Admin routing types and trait for runtime route management.
//!
//! Provides the [`AdminRoutingTable`] trait which extends [`RoutingTable`] with
//! methods for creating, removing, and listing dynamically-added routes at
//! runtime — without requiring config file rewrites or daemon restarts.

use serde::{Deserialize, Serialize};

use crate::errors::Result;

use super::routing_table::RoutingTable;

/// A single endpoint in a dynamic route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteEndpoint {
    /// Provider name (must exist in the providers section or built-ins).
    pub provider: String,
    /// The upstream model ID to send to this provider.
    pub model_id: String,
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
    /// The virtual model name (e.g. "research", "fast").
    pub model: String,
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
    fn remove_route(&self, model: &str) -> Result<bool>;

    /// List all dynamically-added routes.
    fn list_dynamic_routes(&self) -> Vec<DynamicRoute>;
}
