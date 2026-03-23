//! Config-driven agent registry and pricing — parallel to [`ConfigRoutingTable`] for models.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use bitrouter_core::routers::registry::{AgentEntry, AgentRegistry};
use bitrouter_core::routers::upstream::AgentConfig;

/// Pricing for agent invocations.
///
/// Each A2A method call has a flat per-invocation cost. Individual
/// methods can override the default rate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentPricing {
    /// Default cost per agent method invocation (USD).
    #[serde(default)]
    pub default_cost_per_call: f64,
    /// Per-method cost overrides. Keys are A2A method names (e.g. `"message/send"`).
    #[serde(default)]
    pub methods: HashMap<String, f64>,
}

impl AgentPricing {
    /// Return the cost for a given method, falling back to the default.
    pub fn cost_for(&self, method: &str) -> f64 {
        self.methods
            .get(method)
            .copied()
            .unwrap_or(self.default_cost_per_call)
    }
}

/// Immutable agent registry loaded from config.
///
/// Wraps a list of upstream agent configs and exposes them
/// through the [`AgentRegistry`] trait. Parallel to
/// [`ConfigRoutingTable`](crate::routing::ConfigRoutingTable) for models.
pub struct ConfigAgentRegistry {
    entries: Vec<AgentEntry>,
}

impl ConfigAgentRegistry {
    /// Build a registry from the agent configs.
    ///
    /// Converts each [`AgentConfig`] into an [`AgentEntry`] at construction
    /// time so lookups are zero-cost.
    pub fn new(agents: Vec<AgentConfig>) -> Self {
        let entries = agents
            .into_iter()
            .map(|cfg| AgentEntry {
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
            })
            .collect();
        Self { entries }
    }
}

impl AgentRegistry for ConfigAgentRegistry {
    async fn list_agents(&self) -> Vec<AgentEntry> {
        self.entries.clone()
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
        let reg = ConfigAgentRegistry::new(Vec::new());
        assert!(reg.list_agents().await.is_empty());
    }

    #[tokio::test]
    async fn list_returns_single_entry() {
        let reg = ConfigAgentRegistry::new(vec![test_config()]);
        let agents = reg.list_agents().await;
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id, "test-agent");
        assert_eq!(agents[0].name.as_deref(), Some("test-agent"));
    }

    #[tokio::test]
    async fn list_returns_multiple_entries() {
        let mut second = test_config();
        second.name = "second-agent".to_string();
        let reg = ConfigAgentRegistry::new(vec![test_config(), second]);
        let agents = reg.list_agents().await;
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].id, "test-agent");
        assert_eq!(agents[1].id, "second-agent");
    }

    // ── AgentPricing tests ──────────────────────────────────────────

    #[test]
    fn agent_pricing_cost_for_default() {
        let pricing = AgentPricing {
            default_cost_per_call: 0.01,
            methods: HashMap::new(),
        };
        assert!((pricing.cost_for("message/send") - 0.01).abs() < 1e-10);
    }

    #[test]
    fn agent_pricing_cost_for_override() {
        let pricing = AgentPricing {
            default_cost_per_call: 0.01,
            methods: HashMap::from([("message/send".into(), 0.05)]),
        };
        assert!((pricing.cost_for("message/send") - 0.05).abs() < 1e-10);
        assert!((pricing.cost_for("tasks/get") - 0.01).abs() < 1e-10);
    }

    #[test]
    fn agent_pricing_zero_default() {
        let pricing = AgentPricing::default();
        assert_eq!(pricing.cost_for("anything"), 0.0);
    }

    #[test]
    fn agent_pricing_serde_round_trip() {
        let pricing = AgentPricing {
            default_cost_per_call: 0.02,
            methods: HashMap::from([("message/send".into(), 0.1)]),
        };
        let yaml = serde_yaml::to_string(&pricing).expect("serialize");
        let parsed: AgentPricing = serde_yaml::from_str(&yaml).expect("deserialize");
        assert!((parsed.default_cost_per_call - 0.02).abs() < 1e-10);
        assert!((parsed.cost_for("message/send") - 0.1).abs() < 1e-10);
    }
}
