//! Live connection to a single upstream A2A agent.

use std::pin::Pin;
use std::sync::Arc;

use futures_core::Stream;
use tokio::sync::{Notify, RwLock};

use bitrouter_core::api::a2a::error::A2aGatewayError;
use bitrouter_core::api::a2a::gateway::A2aProxy;
use bitrouter_core::api::a2a::types::{
    AgentCard, CancelTaskRequest, DeleteTaskPushNotificationConfigRequest,
    GetTaskPushNotificationConfigRequest, GetTaskRequest, ListTaskPushNotificationConfigsRequest,
    ListTasksRequest, ListTasksResponse, SendMessageRequest, SendMessageResult, StreamResponse,
    Task, TaskPushNotificationConfig,
};
use bitrouter_core::routers::upstream::AgentConfig;

use crate::a2a::transports::A2aTransport;
use crate::a2a::transports::jsonrpc::A2aClient;

/// A live connection to a single upstream A2A agent.
///
/// Caches the agent's card from discovery and forwards all A2A protocol
/// operations to the upstream endpoint.
pub struct UpstreamA2aAgent {
    name: String,
    base_url: String,
    endpoint: String,
    transport: A2aClient,
    card: Arc<RwLock<Option<AgentCard>>>,
    card_notify: Arc<Notify>,
}

impl UpstreamA2aAgent {
    /// Connect to an upstream A2A agent by discovering its agent card.
    pub async fn connect(config: AgentConfig) -> Result<Self, A2aGatewayError> {
        config
            .validate()
            .map_err(|reason| A2aGatewayError::InvalidConfig { reason })?;

        let transport = {
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

        let card = transport.discover(&config.url).await.map_err(|e| {
            A2aGatewayError::UpstreamConnect {
                name: config.name.clone(),
                reason: e.to_string(),
            }
        })?;

        let endpoint = A2aClient::resolve_endpoint(&card).to_string();

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
            transport,
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
        let card = self.transport.discover(&self.base_url).await.map_err(|e| {
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

    /// Forward a `message/send` request to the upstream.
    pub async fn send_message(
        &self,
        request: SendMessageRequest,
    ) -> Result<StreamResponse, A2aGatewayError> {
        let result = self
            .transport
            .send_message(&self.endpoint, request)
            .await
            .map_err(|e| A2aGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: e.to_string(),
            })?;

        Ok(match result {
            SendMessageResult::Task(task) => StreamResponse::Task(*task),
            SendMessageResult::Message(msg) => StreamResponse::Message(*msg),
        })
    }

    /// Forward a `tasks/get` request to the upstream.
    pub async fn get_task(&self, request: GetTaskRequest) -> Result<Task, A2aGatewayError> {
        self.transport
            .get_task(&self.endpoint, request)
            .await
            .map_err(|e| A2aGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: e.to_string(),
            })
    }

    /// Forward a `tasks/cancel` request to the upstream.
    pub async fn cancel_task(&self, request: CancelTaskRequest) -> Result<Task, A2aGatewayError> {
        self.transport
            .cancel_task(&self.endpoint, request)
            .await
            .map_err(|e| A2aGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: e.to_string(),
            })
    }

    /// Forward a `tasks/list` request to the upstream.
    pub async fn list_tasks(
        &self,
        request: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2aGatewayError> {
        self.transport
            .list_tasks(&self.endpoint, request)
            .await
            .map_err(|e| A2aGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: e.to_string(),
            })
    }

    /// Forward a `agent/getAuthenticatedExtendedCard` request to the upstream.
    pub async fn get_extended_agent_card(&self) -> Result<AgentCard, A2aGatewayError> {
        self.transport
            .get_extended_agent_card(&self.endpoint)
            .await
            .map_err(|e| A2aGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: e.to_string(),
            })
    }

    /// Forward a push notification config set request.
    pub async fn set_push_config(
        &self,
        config: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2aGatewayError> {
        self.transport
            .set_push_config(&self.endpoint, config)
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
        config_id: Option<&str>,
    ) -> Result<TaskPushNotificationConfig, A2aGatewayError> {
        let request = GetTaskPushNotificationConfigRequest {
            id: task_id.to_string(),
            push_notification_config_id: config_id.map(|s| s.to_string()),
        };
        self.transport
            .get_push_config(&self.endpoint, request)
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
    ) -> Result<Vec<TaskPushNotificationConfig>, A2aGatewayError> {
        let request = ListTaskPushNotificationConfigsRequest {
            id: task_id.to_string(),
        };
        self.transport
            .list_push_configs(&self.endpoint, request)
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
            id: task_id.to_string(),
            push_notification_config_id: config_id.to_string(),
        };
        self.transport
            .delete_push_config(&self.endpoint, request)
            .await
            .map_err(|e| A2aGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: e.to_string(),
            })
    }
}

// ── A2aProxy trait impl ─────────────────────────────────────────────

impl A2aProxy for UpstreamA2aAgent {
    async fn send_message(
        &self,
        request: SendMessageRequest,
    ) -> Result<StreamResponse, A2aGatewayError> {
        UpstreamA2aAgent::send_message(self, request).await
    }

    async fn get_task(&self, request: GetTaskRequest) -> Result<Task, A2aGatewayError> {
        UpstreamA2aAgent::get_task(self, request).await
    }

    async fn cancel_task(&self, request: CancelTaskRequest) -> Result<Task, A2aGatewayError> {
        UpstreamA2aAgent::cancel_task(self, request).await
    }

    async fn list_tasks(
        &self,
        request: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2aGatewayError> {
        UpstreamA2aAgent::list_tasks(self, request).await
    }

    async fn send_streaming_message(
        &self,
        _request: SendMessageRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamResponse> + Send>>, A2aGatewayError> {
        Err(A2aGatewayError::UpstreamCall {
            name: self.name.clone(),
            reason: "streaming proxy not yet implemented".to_string(),
        })
    }

    async fn subscribe_to_task(
        &self,
        _task_id: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamResponse> + Send>>, A2aGatewayError> {
        Err(A2aGatewayError::UpstreamCall {
            name: self.name.clone(),
            reason: "streaming proxy not yet implemented".to_string(),
        })
    }

    async fn get_extended_agent_card(&self) -> Result<AgentCard, A2aGatewayError> {
        UpstreamA2aAgent::get_extended_agent_card(self).await
    }

    async fn set_push_config(
        &self,
        config: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2aGatewayError> {
        UpstreamA2aAgent::set_push_config(self, config).await
    }

    async fn get_push_config(
        &self,
        task_id: &str,
        config_id: Option<&str>,
    ) -> Result<TaskPushNotificationConfig, A2aGatewayError> {
        UpstreamA2aAgent::get_push_config(self, task_id, config_id).await
    }

    async fn list_push_configs(
        &self,
        task_id: &str,
    ) -> Result<Vec<TaskPushNotificationConfig>, A2aGatewayError> {
        UpstreamA2aAgent::list_push_configs(self, task_id).await
    }

    async fn delete_push_config(
        &self,
        task_id: &str,
        config_id: &str,
    ) -> Result<(), A2aGatewayError> {
        UpstreamA2aAgent::delete_push_config(self, task_id, config_id).await
    }
}
