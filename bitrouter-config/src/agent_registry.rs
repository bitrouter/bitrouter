//! Config-driven A2A agent registry — parallel to [`ConfigRoutingTable`] for models.

use bitrouter_a2a::admin::AgentRegistry;
use bitrouter_a2a::card::{self, AgentCard};
use bitrouter_a2a::config::A2aAgentConfig;

/// Immutable agent registry loaded from config.
///
/// Wraps a single optional upstream agent config and exposes it
/// through the [`AgentRegistry`] trait. Parallel to
/// [`ConfigRoutingTable`](crate::routing::ConfigRoutingTable) for models.
pub struct ConfigAgentRegistry {
    card: Option<(String, AgentCard)>,
}

impl ConfigAgentRegistry {
    /// Build a registry from the optional agent config.
    ///
    /// Converts the [`A2aAgentConfig`] into an [`AgentCard`] at construction
    /// time so lookups are zero-cost.
    pub fn new(agent: Option<A2aAgentConfig>) -> Self {
        let card = agent.map(|cfg| {
            let card = card::minimal_card(&cfg.name, &cfg.name, "0.1.0", &cfg.url);
            (cfg.name, card)
        });
        Self { card }
    }
}

impl AgentRegistry for ConfigAgentRegistry {
    async fn get(&self, name: &str) -> Option<AgentCard> {
        self.card
            .as_ref()
            .filter(|(n, _)| n == name)
            .map(|(_, c)| c.clone())
    }

    async fn list(&self) -> Vec<AgentCard> {
        self.card
            .as_ref()
            .map(|(_, c)| vec![c.clone()])
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn test_config() -> A2aAgentConfig {
        A2aAgentConfig {
            name: "test-agent".to_string(),
            url: "http://localhost:9000".to_string(),
            headers: HashMap::new(),
            card_path: None,
        }
    }

    #[tokio::test]
    async fn empty_registry_returns_none() {
        let reg = ConfigAgentRegistry::new(None);
        assert!(reg.get("anything").await.is_none());
        assert!(reg.list().await.is_empty());
    }

    #[tokio::test]
    async fn get_by_name() {
        let reg = ConfigAgentRegistry::new(Some(test_config()));
        let card = reg.get("test-agent").await;
        assert!(card.is_some());
        assert_eq!(card.as_ref().map(|c| c.name.as_str()), Some("test-agent"));
    }

    #[tokio::test]
    async fn get_wrong_name_returns_none() {
        let reg = ConfigAgentRegistry::new(Some(test_config()));
        assert!(reg.get("other").await.is_none());
    }

    #[tokio::test]
    async fn list_returns_single_card() {
        let reg = ConfigAgentRegistry::new(Some(test_config()));
        let cards = reg.list().await;
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].name, "test-agent");
    }
}
