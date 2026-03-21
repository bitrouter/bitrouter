//! Single-agent upstream registry implementing gateway traits.

use std::pin::Pin;
use std::sync::Arc;

use futures_core::Stream;
use tokio::sync::broadcast;

use crate::admin::{AdminAgentRegistry, AgentInfo};
use crate::card::AgentCard;
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
/// the gateway traits ([`A2aDiscovery`], [`A2aProxy`], [`AdminAgentRegistry`]).
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
        // TODO: Implement SSE stream proxying via reqwest byte stream
        Err(A2aGatewayError::UpstreamCall {
            name: self.agent.as_ref().map_or("none", |a| a.name()).to_string(),
            reason: "streaming proxy not yet implemented".to_string(),
        })
    }

    async fn subscribe_to_task(
        &self,
        _task_id: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamResponse> + Send>>, A2aGatewayError> {
        // TODO: Implement SSE stream proxying via reqwest byte stream
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

impl AdminAgentRegistry for UpstreamAgentRegistry {
    async fn list_agents(&self) -> Vec<AgentInfo> {
        match &self.agent {
            Some(agent) => vec![AgentInfo {
                name: agent.name().to_string(),
                url: agent.url().to_string(),
                connected: agent.cached_card().await.is_some(),
            }],
            None => Vec::new(),
        }
    }
}
