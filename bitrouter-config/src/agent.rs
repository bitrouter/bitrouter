//! Config-driven agent registry — parallel to [`ConfigRoutingTable`] for models.

use bitrouter_core::routers::registry::{AgentEntry, AgentRegistry};
use bitrouter_core::routers::upstream::AgentConfig;

/// Immutable agent registry loaded from config.
///
/// Wraps a single optional upstream agent config and exposes it
/// through the [`AgentRegistry`] trait. Parallel to
/// [`ConfigRoutingTable`](crate::routing::ConfigRoutingTable) for models.
pub struct ConfigAgentRegistry {
    entry: Option<AgentEntry>,
}

impl ConfigAgentRegistry {
    /// Build a registry from the optional agent config.
    ///
    /// Converts the [`AgentConfig`] into an [`AgentEntry`] at construction
    /// time so lookups are zero-cost.
    pub fn new(agent: Option<AgentConfig>) -> Self {
        let entry = agent.map(|cfg| AgentEntry {
            id: cfg.name.clone(),
            name: Some(cfg.name),
            provider: String::new(),
            description: None,
            version: None,
            skills: Vec::new(),
            input_modes: vec!["text/plain".to_string()],
            output_modes: vec!["text/plain".to_string()],
            streaming: None,
            icon_url: None,
            documentation_url: None,
        });
        Self { entry }
    }
}

impl AgentRegistry for ConfigAgentRegistry {
    async fn list_agents(&self) -> Vec<AgentEntry> {
        self.entry
            .as_ref()
            .map(|e| vec![e.clone()])
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AgentConfig {
        AgentConfig {
            name: "test-agent".to_string(),
            url: "http://localhost:9000".to_string(),
            headers: std::collections::HashMap::new(),
            card_path: None,
        }
    }

    #[tokio::test]
    async fn empty_registry_returns_empty() {
        let reg = ConfigAgentRegistry::new(None);
        assert!(reg.list_agents().await.is_empty());
    }

    #[tokio::test]
    async fn list_returns_single_entry() {
        let reg = ConfigAgentRegistry::new(Some(test_config()));
        let agents = reg.list_agents().await;
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id, "test-agent");
        assert_eq!(agents[0].name.as_deref(), Some("test-agent"));
    }
}
