//! Multi-agent upstream registry implementing gateway traits.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use bitrouter_core::api::a2a::error::A2aGatewayError;
use bitrouter_core::api::a2a::types::AgentCard;
use bitrouter_core::routers::admin::{AgentUpstreamEntry, AgentUpstreamSource};
use bitrouter_core::routers::upstream::AgentConfig;
use tokio::sync::broadcast;

use super::upstream::UpstreamA2aAgent;

/// Read-only registry for A2A agent lookup.
///
/// Provides protocol-specific access to [`AgentCard`] objects. The core
/// [`AgentRegistry`](bitrouter_core::routers::registry::AgentRegistry) trait
/// handles protocol-agnostic discovery.
pub trait A2aAgentRegistry: Send + Sync {
    /// Get an agent card by name.
    fn get(&self, name: &str) -> impl Future<Output = Option<AgentCard>> + Send;
    /// List all registered agent cards.
    fn list(&self) -> impl Future<Output = Vec<AgentCard>> + Send;
}

pub use crate::util::RefreshGuard;

/// Multi-agent upstream registry for the A2A gateway.
///
/// Holds zero or more upstream A2A agent connections keyed by name.
/// Each agent is a fully independent A2A endpoint exposed under its
/// own path prefix (`/a2a/{agent_name}/`).
pub struct UpstreamAgentRegistry {
    agents: HashMap<String, UpstreamA2aAgent>,
    external_base_url: String,
    card_change_tx: broadcast::Sender<()>,
}

impl UpstreamAgentRegistry {
    /// Connect to all configured upstream agents.
    ///
    /// Agents that fail to connect are logged and skipped so that one
    /// unreachable agent does not prevent the gateway from starting.
    pub async fn from_configs(configs: Vec<AgentConfig>, external_base_url: String) -> Self {
        let (card_change_tx, _) = broadcast::channel(16);
        let mut agents = HashMap::new();

        for cfg in configs {
            let name = cfg.name.clone();
            match UpstreamA2aAgent::connect(cfg).await {
                Ok(agent) => {
                    agents.insert(name, agent);
                }
                Err(e) => {
                    tracing::warn!(agent = %name, error = %e, "failed to connect to upstream agent, skipping");
                }
            }
        }

        Self {
            agents,
            external_base_url,
            card_change_tx,
        }
    }

    /// Returns true if at least one agent is connected.
    pub fn has_agents(&self) -> bool {
        !self.agents.is_empty()
    }

    /// Get a reference to an upstream agent by name.
    pub fn get_agent(&self, name: &str) -> Option<&UpstreamA2aAgent> {
        self.agents.get(name)
    }

    /// Require an agent by name, returning an error if not found.
    pub fn require_agent(&self, name: &str) -> Result<&UpstreamA2aAgent, A2aGatewayError> {
        self.agents
            .get(name)
            .ok_or_else(|| A2aGatewayError::AgentNotFound {
                name: name.to_string(),
            })
    }

    /// Spawn background tasks that periodically refresh each agent's card.
    ///
    /// Returns a [`RefreshGuard`] that aborts all tasks when dropped.
    pub fn spawn_refresh_listeners(self: &Arc<Self>) -> RefreshGuard {
        let mut handles = Vec::new();

        for (name, agent) in &self.agents {
            let notify = agent.card_change_notify();
            let reg = Arc::clone(self);
            let name = name.clone();
            handles.push(tokio::spawn(async move {
                loop {
                    notify.notified().await;
                    tracing::info!(agent = %name, "agent card changed, refreshing");
                    if let Some(agent) = reg.agents.get(&name) {
                        if let Err(e) = agent.refresh_card().await {
                            tracing::warn!(agent = %name, error = %e, "failed to refresh agent card");
                        } else {
                            let _ = reg.card_change_tx.send(());
                        }
                    }
                }
            }));
        }

        RefreshGuard::from_handles(handles)
    }

    /// Return the agent card with URL rewritten to the gateway's external address.
    pub async fn rewritten_card(&self, name: &str) -> Option<AgentCard> {
        let agent = self.agents.get(name)?;
        let mut card = agent.cached_card().await?;

        // Rewrite the primary URL to point to the gateway's per-agent endpoint.
        card.url = format!("{}/{}", self.external_base_url.trim_end_matches('/'), name);

        Some(card)
    }

    /// Subscribe to agent card change notifications.
    pub fn subscribe_card_changes(&self) -> broadcast::Receiver<()> {
        self.card_change_tx.subscribe()
    }
}

// ── A2aGateway trait impl ───────────────────────────────────────────

impl bitrouter_core::api::a2a::gateway::A2aGateway for UpstreamAgentRegistry {
    type Agent = UpstreamA2aAgent;

    fn require_agent(&self, name: &str) -> Result<&UpstreamA2aAgent, A2aGatewayError> {
        UpstreamAgentRegistry::require_agent(self, name)
    }

    async fn get_card(&self, name: &str) -> Option<AgentCard> {
        self.rewritten_card(name).await
    }
}

// ── A2A-internal admin trait impls ──────────────────────────────────

impl A2aAgentRegistry for UpstreamAgentRegistry {
    async fn get(&self, name: &str) -> Option<AgentCard> {
        self.rewritten_card(name).await
    }

    async fn list(&self) -> Vec<AgentCard> {
        let mut cards = Vec::new();
        for name in self.agents.keys() {
            if let Some(card) = self.rewritten_card(name).await {
                cards.push(card);
            }
        }
        cards
    }
}

// ── Core trait impls ────────────────────────────────────────────────

/// Implement core [`AgentRegistry`](bitrouter_core::routers::registry::AgentRegistry)
/// for public discovery (`GET /v1/agents`).
impl bitrouter_core::routers::registry::AgentRegistry for UpstreamAgentRegistry {
    async fn list_agents(&self) -> Vec<bitrouter_core::routers::registry::AgentEntry> {
        A2aAgentRegistry::list(self)
            .await
            .into_iter()
            .map(Into::into)
            .collect()
    }
}

/// Implement core [`AgentUpstreamSource`] for admin inspection.
impl AgentUpstreamSource for UpstreamAgentRegistry {
    async fn list_upstreams(&self) -> Vec<AgentUpstreamEntry> {
        let mut entries = Vec::new();
        for agent in self.agents.values() {
            entries.push(AgentUpstreamEntry {
                name: agent.name().to_string(),
                url: agent.url().to_string(),
                connected: agent.cached_card().await.is_some(),
            });
        }
        entries
    }
}
