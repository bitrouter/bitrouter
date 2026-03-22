//! Single-agent upstream registry implementing gateway traits.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use bitrouter_core::routers::admin::{AgentUpstreamEntry, AgentUpstreamSource};
use bitrouter_core::routers::dynamic_agent::DynamicAgentRegistry;
use futures_core::Stream;
use tokio::sync::broadcast;

use crate::card::AgentCard;

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
use crate::config::A2aAgentConfig;
use crate::error::A2aGatewayError;
use crate::request::{
    CancelTaskRequest, ListTaskPushNotificationConfigsResponse, SendMessageRequest,
    TaskPushNotificationConfig,
};
use crate::server::{A2aDiscovery, A2aProxy};
use crate::stream::StreamResponse;
use crate::task::{GetTaskRequest, ListTasksRequest, ListTasksResponse, Task};

use super::upstream::UpstreamA2aAgent;

/// Guard that aborts background refresh tasks on drop.
pub struct RefreshGuard {
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
    }
}

/// Single-agent upstream registry for the A2A gateway.
///
/// Holds an optional upstream A2A agent connection and implements
/// the gateway traits ([`A2aDiscovery`], [`A2aProxy`]).
pub struct UpstreamAgentRegistry {
    agent: Option<UpstreamA2aAgent>,
    external_url: String,
    card_change_tx: broadcast::Sender<()>,
}

impl UpstreamAgentRegistry {
    /// Connect to the configured upstream agent, if any.
    pub async fn from_config(
        config: Option<A2aAgentConfig>,
        external_url: String,
    ) -> Result<Self, A2aGatewayError> {
        let (card_change_tx, _) = broadcast::channel(16);

        let agent = match config {
            Some(cfg) => {
                let agent = UpstreamA2aAgent::connect(cfg).await?;
                Some(agent)
            }
            None => None,
        };

        Ok(Self {
            agent,
            external_url,
            card_change_tx,
        })
    }

    /// Spawn background tasks that periodically refresh the agent card.
    ///
    /// Returns a [`RefreshGuard`] that aborts all tasks when dropped.
    pub fn spawn_refresh_listeners(self: &Arc<Self>) -> RefreshGuard {
        let mut handles = Vec::new();

        if let Some(ref agent) = self.agent {
            let notify = agent.card_change_notify();
            let reg = Arc::clone(self);
            let name = agent.name().to_string();
            handles.push(tokio::spawn(async move {
                loop {
                    notify.notified().await;
                    tracing::info!(agent = %name, "agent card changed, refreshing");
                    if let Some(ref agent) = reg.agent {
                        if let Err(e) = agent.refresh_card().await {
                            tracing::warn!(agent = %name, error = %e, "failed to refresh agent card");
                        } else {
                            let _ = reg.card_change_tx.send(());
                        }
                    }
                }
            }));
        }

        RefreshGuard { handles }
    }

    /// Return the agent card with URL rewritten to the gateway's external address.
    async fn rewritten_card(&self) -> Option<AgentCard> {
        let agent = self.agent.as_ref()?;
        let mut card = agent.cached_card().await?;

        // Rewrite the first interface URL to point to the gateway.
        if let Some(iface) = card.supported_interfaces.first_mut() {
            iface.url.clone_from(&self.external_url);
        }

        Some(card)
    }

    fn require_agent(&self) -> Result<&UpstreamA2aAgent, A2aGatewayError> {
        self.agent
            .as_ref()
            .ok_or_else(|| A2aGatewayError::AgentNotFound {
                name: "no upstream agent configured".to_string(),
            })
    }
}

// ── Protocol trait impls on UpstreamAgentRegistry ────────────────────

impl A2aDiscovery for UpstreamAgentRegistry {
    async fn get_agent_card(&self) -> Option<AgentCard> {
        self.rewritten_card().await
    }

    fn subscribe_card_changes(&self) -> broadcast::Receiver<()> {
        self.card_change_tx.subscribe()
    }
}

impl A2aProxy for UpstreamAgentRegistry {
    async fn send_message(
        &self,
        request: SendMessageRequest,
    ) -> Result<StreamResponse, A2aGatewayError> {
        self.require_agent()?.send_message(request).await
    }

    async fn get_task(&self, request: GetTaskRequest) -> Result<Task, A2aGatewayError> {
        self.require_agent()?.get_task(request).await
    }

    async fn cancel_task(&self, request: CancelTaskRequest) -> Result<Task, A2aGatewayError> {
        self.require_agent()?.cancel_task(request).await
    }

    async fn list_tasks(
        &self,
        request: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2aGatewayError> {
        self.require_agent()?.list_tasks(request).await
    }

    async fn send_streaming_message(
        &self,
        _request: SendMessageRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamResponse> + Send>>, A2aGatewayError> {
        Err(A2aGatewayError::UpstreamCall {
            name: self.agent.as_ref().map_or("none", |a| a.name()).to_string(),
            reason: "streaming proxy not yet implemented".to_string(),
        })
    }

    async fn subscribe_to_task(
        &self,
        _task_id: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamResponse> + Send>>, A2aGatewayError> {
        Err(A2aGatewayError::UpstreamCall {
            name: self.agent.as_ref().map_or("none", |a| a.name()).to_string(),
            reason: "streaming proxy not yet implemented".to_string(),
        })
    }

    async fn get_extended_agent_card(&self) -> Result<AgentCard, A2aGatewayError> {
        self.require_agent()?.get_extended_agent_card().await
    }

    async fn create_push_config(
        &self,
        config: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2aGatewayError> {
        self.require_agent()?.create_push_config(config).await
    }

    async fn get_push_config(
        &self,
        task_id: &str,
        config_id: &str,
    ) -> Result<TaskPushNotificationConfig, A2aGatewayError> {
        self.require_agent()?
            .get_push_config(task_id, config_id)
            .await
    }

    async fn list_push_configs(
        &self,
        task_id: &str,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2aGatewayError> {
        self.require_agent()?.list_push_configs(task_id).await
    }

    async fn delete_push_config(
        &self,
        task_id: &str,
        config_id: &str,
    ) -> Result<(), A2aGatewayError> {
        self.require_agent()?
            .delete_push_config(task_id, config_id)
            .await
    }
}

// ── A2A-internal admin trait impls ──────────────────────────────────

impl A2aAgentRegistry for UpstreamAgentRegistry {
    async fn get(&self, name: &str) -> Option<AgentCard> {
        let agent = self.agent.as_ref()?;
        if agent.name() == name {
            self.rewritten_card().await
        } else {
            None
        }
    }

    async fn list(&self) -> Vec<AgentCard> {
        match self.rewritten_card().await {
            Some(card) => vec![card],
            None => Vec::new(),
        }
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
        match &self.agent {
            Some(agent) => vec![AgentUpstreamEntry {
                name: agent.name().to_string(),
                url: agent.url().to_string(),
                connected: agent.cached_card().await.is_some(),
            }],
            None => Vec::new(),
        }
    }
}

// ── Protocol trait passthrough on DynamicAgentRegistry ───────────────
//
// These impls let the runtime pass DynamicAgentRegistry<UpstreamAgentRegistry>
// directly to the A2A gateway filters.

impl A2aDiscovery for DynamicAgentRegistry<Arc<UpstreamAgentRegistry>> {
    async fn get_agent_card(&self) -> Option<AgentCard> {
        self.inner().get_agent_card().await
    }

    fn subscribe_card_changes(&self) -> broadcast::Receiver<()> {
        self.inner().subscribe_card_changes()
    }
}

impl A2aProxy for DynamicAgentRegistry<Arc<UpstreamAgentRegistry>> {
    async fn send_message(
        &self,
        request: SendMessageRequest,
    ) -> Result<StreamResponse, A2aGatewayError> {
        self.inner().send_message(request).await
    }

    async fn get_task(&self, request: GetTaskRequest) -> Result<Task, A2aGatewayError> {
        self.inner().get_task(request).await
    }

    async fn cancel_task(&self, request: CancelTaskRequest) -> Result<Task, A2aGatewayError> {
        self.inner().cancel_task(request).await
    }

    async fn list_tasks(
        &self,
        request: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2aGatewayError> {
        self.inner().list_tasks(request).await
    }

    async fn send_streaming_message(
        &self,
        request: SendMessageRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamResponse> + Send>>, A2aGatewayError> {
        self.inner().send_streaming_message(request).await
    }

    async fn subscribe_to_task(
        &self,
        task_id: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamResponse> + Send>>, A2aGatewayError> {
        self.inner().subscribe_to_task(task_id).await
    }

    async fn get_extended_agent_card(&self) -> Result<AgentCard, A2aGatewayError> {
        self.inner().get_extended_agent_card().await
    }

    async fn create_push_config(
        &self,
        config: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2aGatewayError> {
        self.inner().create_push_config(config).await
    }

    async fn get_push_config(
        &self,
        task_id: &str,
        config_id: &str,
    ) -> Result<TaskPushNotificationConfig, A2aGatewayError> {
        self.inner().get_push_config(task_id, config_id).await
    }

    async fn list_push_configs(
        &self,
        task_id: &str,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2aGatewayError> {
        self.inner().list_push_configs(task_id).await
    }

    async fn delete_push_config(
        &self,
        task_id: &str,
        config_id: &str,
    ) -> Result<(), A2aGatewayError> {
        self.inner().delete_push_config(task_id, config_id).await
    }
}
