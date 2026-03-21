//! Live connection to a single upstream A2A agent.

use std::sync::Arc;

use tokio::sync::{Notify, RwLock};

use crate::card::AgentCard;
use crate::config::A2aAgentConfig;
use crate::error::A2aGatewayError;
use crate::request::{
    CancelTaskRequest, DeleteTaskPushNotificationConfigRequest,
    GetTaskPushNotificationConfigRequest, ListTaskPushNotificationConfigsRequest,
    ListTaskPushNotificationConfigsResponse, SendMessageRequest, TaskPushNotificationConfig,
};
use crate::stream::StreamResponse;
use crate::task::{GetTaskRequest, ListTasksRequest, ListTasksResponse, Task};

use super::a2a_client::{A2aClient, SendMessageResult};

/// A live connection to a single upstream A2A agent.
///
/// Caches the agent's card from discovery and forwards all A2A protocol
/// operations to the upstream endpoint.
pub struct UpstreamA2aAgent {
    name: String,
    base_url: String,
    endpoint: String,
    client: A2aClient,
    card: Arc<RwLock<Option<AgentCard>>>,
    card_notify: Arc<Notify>,
}

impl UpstreamA2aAgent {
    /// Connect to an upstream A2A agent by discovering its agent card.
    pub async fn connect(config: A2aAgentConfig) -> Result<Self, A2aGatewayError> {
        config.validate()?;

        let client = {
            let mut headers = reqwest::header::HeaderMap::new();
            for (k, v) in &config.headers {
                let name: reqwest::header::HeaderName =
                    k.parse().map_err(|e| A2aGatewayError::InvalidConfig {
                        reason: format!("invalid header name '{k}': {e}"),
                    })?;
                let value: reqwest::header::HeaderValue =
                    v.parse().map_err(|e| A2aGatewayError::InvalidConfig {
                        reason: format!("invalid header value for '{k}': {e}"),
                    })?;
                headers.insert(name, value);
            }
            let http = reqwest::Client::builder()
                .default_headers(headers)
                .build()
                .map_err(|e| A2aGatewayError::UpstreamConnect {
                    name: config.name.clone(),
                    reason: format!("failed to build HTTP client: {e}"),
                })?;
            A2aClient::with_http_client(http)
        };

        let card =
            client
                .discover(&config.url)
                .await
                .map_err(|e| A2aGatewayError::UpstreamConnect {
                    name: config.name.clone(),
                    reason: e.to_string(),
                })?;

        let endpoint = A2aClient::resolve_endpoint(&card)
            .ok_or_else(|| A2aGatewayError::UpstreamConnect {
                name: config.name.clone(),
                reason: "agent card has no supported interfaces".to_string(),
            })?
            .to_string();

        tracing::info!(
            agent = %config.name,
            endpoint = %endpoint,
            "connected to upstream A2A agent"
        );

        let card_notify = Arc::new(Notify::new());

        Ok(Self {
            name: config.name,
            base_url: config.url,
            endpoint,
            client,
            card: Arc::new(RwLock::new(Some(card))),
            card_notify,
        })
    }

    /// Return the cached agent card.
    pub async fn cached_card(&self) -> Option<AgentCard> {
        self.card.read().await.clone()
    }

    /// Re-discover the agent card from the upstream.
    pub async fn refresh_card(&self) -> Result<(), A2aGatewayError> {
        let card = self.client.discover(&self.base_url).await.map_err(|e| {
            A2aGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: format!("card refresh failed: {e}"),
            }
        })?;
        let mut cache = self.card.write().await;
        *cache = Some(card);
        Ok(())
    }

    /// Expose the card-change notify handle for background refresh tasks.
    pub fn card_change_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.card_notify)
    }

    /// Return the agent name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return the upstream base URL.
    pub fn url(&self) -> &str {
        &self.base_url
    }

    // ── A2A protocol forwarding ──────────────────────────────────

    /// Forward a `SendMessage` request to the upstream.
    pub async fn send_message(
        &self,
        request: SendMessageRequest,
    ) -> Result<StreamResponse, A2aGatewayError> {
        let result = self
            .client
            .send_message(&self.endpoint, request)
            .await
            .map_err(|e| A2aGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: e.to_string(),
            })?;

        Ok(match result {
            SendMessageResult::Task(task) => StreamResponse::Task(task),
            SendMessageResult::Message(msg) => StreamResponse::Message(msg),
        })
    }

    /// Forward a `GetTask` request to the upstream.
    pub async fn get_task(&self, request: GetTaskRequest) -> Result<Task, A2aGatewayError> {
        self.client
            .get_task(&self.endpoint, request)
            .await
            .map_err(|e| A2aGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: e.to_string(),
            })
    }

    /// Forward a `CancelTask` request to the upstream.
    pub async fn cancel_task(&self, request: CancelTaskRequest) -> Result<Task, A2aGatewayError> {
        self.client
            .cancel_task(&self.endpoint, request)
            .await
            .map_err(|e| A2aGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: e.to_string(),
            })
    }

    /// Forward a `ListTasks` request to the upstream.
    pub async fn list_tasks(
        &self,
        request: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2aGatewayError> {
        self.client
            .list_tasks(&self.endpoint, request)
            .await
            .map_err(|e| A2aGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: e.to_string(),
            })
    }

    /// Forward a `GetExtendedAgentCard` request to the upstream.
    pub async fn get_extended_agent_card(&self) -> Result<AgentCard, A2aGatewayError> {
        self.client
            .get_extended_agent_card(&self.endpoint)
            .await
            .map_err(|e| A2aGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: e.to_string(),
            })
    }

    /// Forward a push notification config create request.
    pub async fn create_push_config(
        &self,
        config: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2aGatewayError> {
        self.client
            .create_push_notification_config(&self.endpoint, config)
            .await
            .map_err(|e| A2aGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: e.to_string(),
            })
    }

    /// Forward a push notification config get request.
    pub async fn get_push_config(
        &self,
        task_id: &str,
        config_id: &str,
    ) -> Result<TaskPushNotificationConfig, A2aGatewayError> {
        let request = GetTaskPushNotificationConfigRequest {
            tenant: None,
            id: config_id.to_string(),
            task_id: task_id.to_string(),
        };
        self.client
            .get_push_notification_config(&self.endpoint, request)
            .await
            .map_err(|e| A2aGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: e.to_string(),
            })
    }

    /// Forward a push notification config list request.
    pub async fn list_push_configs(
        &self,
        task_id: &str,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2aGatewayError> {
        let request = ListTaskPushNotificationConfigsRequest {
            tenant: None,
            task_id: task_id.to_string(),
        };
        self.client
            .list_push_notification_configs(&self.endpoint, request)
            .await
            .map_err(|e| A2aGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: e.to_string(),
            })
    }

    /// Forward a push notification config delete request.
    pub async fn delete_push_config(
        &self,
        task_id: &str,
        config_id: &str,
    ) -> Result<(), A2aGatewayError> {
        let request = DeleteTaskPushNotificationConfigRequest {
            tenant: None,
            id: config_id.to_string(),
            task_id: task_id.to_string(),
        };
        self.client
            .delete_push_notification_config(&self.endpoint, request)
            .await
            .map_err(|e| A2aGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: e.to_string(),
            })
    }
}
