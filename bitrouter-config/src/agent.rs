//! Config-driven agent registry — parallel to [`ConfigRoutingTable`] for models.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use bitrouter_core::routers::registry::{AgentEntry, AgentRegistry};

/// Configuration for an upstream agent to proxy.
///
/// Protocol-agnostic YAML config shape. Protocol crates convert this
/// into their own types at assembly time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Display name for this upstream agent.
    pub name: String,

    /// Base URL of the upstream agent (used for discovery).
    pub url: String,

    /// Optional HTTP headers to send to upstream (e.g., auth tokens).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,

    /// Optional card discovery path override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card_path: Option<String>,
}

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
            headers: HashMap::new(),
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

    #[test]
    fn serde_round_trip() {
        let cfg = AgentConfig {
            name: "my-agent".to_string(),
            url: "https://agent.example.com".to_string(),
            headers: HashMap::from([("Authorization".into(), "Bearer tok".into())]),
            card_path: Some("/custom/card.json".to_string()),
        };
        let yaml = serde_yaml::to_string(&cfg).expect("serialize");
        let parsed: AgentConfig = serde_yaml::from_str(&yaml).expect("deserialize");
        assert_eq!(parsed.name, "my-agent");
        assert_eq!(parsed.url, "https://agent.example.com");
        assert_eq!(
            parsed.headers.get("Authorization").map(String::as_str),
            Some("Bearer tok")
        );
        assert_eq!(parsed.card_path.as_deref(), Some("/custom/card.json"));
    }
}
