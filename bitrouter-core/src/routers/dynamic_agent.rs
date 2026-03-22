//! An agent registry wrapper that adds admin inspection capabilities.
//!
//! [`DynamicAgentRegistry`] wraps any [`AgentRegistry`] and exposes
//! upstream connection metadata through the [`AdminAgentRegistry`] trait.
//!
//! Parallel to [`DynamicRoutingTable`](super::dynamic::DynamicRoutingTable)
//! for models and [`DynamicToolRegistry`](super::dynamic_tool::DynamicToolRegistry)
//! for tools.

use super::admin::{AdminAgentRegistry, AgentUpstreamEntry, AgentUpstreamSource};
use super::registry::{AgentEntry, AgentRegistry};

/// An agent registry wrapper that adds admin inspection capabilities.
///
/// Wraps any `T: AgentRegistry + AgentUpstreamSource` and delegates
/// discovery through `AgentRegistry` while exposing operational metadata
/// (connection status, URLs) through `AdminAgentRegistry`.
pub struct DynamicAgentRegistry<T> {
    inner: T,
}

impl<T> DynamicAgentRegistry<T> {
    /// Create a new dynamic agent registry wrapping the given inner registry.
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Access the inner registry for protocol-specific operations.
    pub fn inner(&self) -> &T {
        &self.inner
    }
}

impl<T: AgentRegistry> AgentRegistry for DynamicAgentRegistry<T> {
    async fn list_agents(&self) -> Vec<AgentEntry> {
        self.inner.list_agents().await
    }
}

impl<T: AgentRegistry + AgentUpstreamSource> AdminAgentRegistry for DynamicAgentRegistry<T> {
    async fn list_upstreams(&self) -> Vec<AgentUpstreamEntry> {
        self.inner.list_upstreams().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routers::admin::AgentUpstreamEntry;
    use crate::routers::registry::AgentEntry;

    struct MockAgentSource {
        entries: Vec<AgentEntry>,
        upstreams: Vec<AgentUpstreamEntry>,
    }

    impl AgentRegistry for MockAgentSource {
        async fn list_agents(&self) -> Vec<AgentEntry> {
            self.entries.clone()
        }
    }

    impl AgentUpstreamSource for MockAgentSource {
        async fn list_upstreams(&self) -> Vec<AgentUpstreamEntry> {
            self.upstreams.clone()
        }
    }

    fn test_source() -> MockAgentSource {
        MockAgentSource {
            entries: vec![AgentEntry {
                id: "test-agent".to_owned(),
                name: Some("Test Agent".to_owned()),
                provider: "a2a".to_owned(),
                description: Some("A test agent".to_owned()),
                version: Some("1.0".to_owned()),
                skills: Vec::new(),
                input_modes: vec!["text/plain".to_owned()],
                output_modes: vec!["text/plain".to_owned()],
                streaming: None,
                icon_url: None,
                documentation_url: None,
            }],
            upstreams: vec![AgentUpstreamEntry {
                name: "test-agent".to_owned(),
                url: "http://localhost:9000".to_owned(),
                connected: true,
            }],
        }
    }

    #[tokio::test]
    async fn delegates_list_agents() {
        let reg = DynamicAgentRegistry::new(test_source());
        let agents = reg.list_agents().await;
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id, "test-agent");
    }

    #[tokio::test]
    async fn delegates_list_upstreams() {
        let reg = DynamicAgentRegistry::new(test_source());
        let upstreams = reg.list_upstreams().await;
        assert_eq!(upstreams.len(), 1);
        assert_eq!(upstreams[0].name, "test-agent");
        assert!(upstreams[0].connected);
    }

    #[test]
    fn inner_accessor() {
        let reg = DynamicAgentRegistry::new(test_source());
        assert_eq!(reg.inner().entries.len(), 1);
    }
}
