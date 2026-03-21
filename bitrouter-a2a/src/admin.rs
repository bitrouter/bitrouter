//! Admin trait for A2A gateway inspection.

use std::future::Future;

use serde::Serialize;

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

/// Admin interface for inspecting the A2A gateway state.
pub trait AdminAgentRegistry: Send + Sync {
    /// List all configured upstream agents with connection status.
    fn list_agents(&self) -> impl Future<Output = Vec<AgentInfo>> + Send;
}
