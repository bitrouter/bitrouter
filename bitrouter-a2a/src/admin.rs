//! A2A registry traits — parallel to `RoutingTable` / `AdminRoutingTable` for models.

use std::future::Future;

use serde::Serialize;

use crate::card::AgentCard;

/// Information about a proxied upstream A2A agent.
#[derive(Debug, Clone, Serialize)]
pub struct AgentInfo {
    /// Agent name (from config).
    pub name: String,
    /// Upstream agent URL.
    pub url: String,
    /// Whether the upstream connection is active.
    pub connected: bool,
}

/// Read-only registry for A2A agent lookup.
///
/// Parallel to [`RoutingTable`](bitrouter_core::routers::routing_table::RoutingTable)
/// for models. Provides read access to agent cards configured in the gateway.
pub trait AgentRegistry: Send + Sync {
    /// Get an agent card by name.
    fn get(&self, name: &str) -> impl Future<Output = Option<AgentCard>> + Send;
    /// List all registered agent cards.
    fn list(&self) -> impl Future<Output = Vec<AgentCard>> + Send;
}

/// Admin interface extending [`AgentRegistry`] with inspection methods.
///
/// Parallel to [`AdminRoutingTable`](bitrouter_core::routers::admin::AdminRoutingTable)
/// for models. Adds admin-level visibility into agent connection status.
pub trait AdminAgentRegistry: AgentRegistry {
    /// List all configured upstream agents with connection status.
    fn list_agents(&self) -> impl Future<Output = Vec<AgentInfo>> + Send;
}
