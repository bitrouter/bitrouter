//! Live connection to a single upstream A2A agent.

use std::pin::Pin;
use std::sync::Arc;

use futures_core::Stream;
use tokio::sync::{Notify, RwLock};
use tokio_stream::StreamExt;

use super::config::A2aAgentConfig;
use bitrouter_core::api::a2a::gateway::A2aProxy;
use bitrouter_core::api::a2a::types::A2aGatewayError;
use bitrouter_core::api::a2a::types::{
    AgentCard, CancelTaskRequest, DeleteTaskPushNotificationConfigRequest,
    GetTaskPushNotificationConfigRequest, GetTaskRequest, ListTaskPushNotificationConfigsRequest,
    ListTasksRequest, ListTasksResponse, Message, MessageRole, Part, SendMessageRequest,
    SendMessageResult, StreamResponse, Task, TaskPushNotificationConfig,
};
use bitrouter_core::errors::{BitrouterError, Result as BResult};
use bitrouter_core::tools::provider::ToolProvider;
use bitrouter_core::tools::result::{ToolCallResult, ToolContent};

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
    pub async fn connect(config: A2aAgentConfig) -> Result<Self, A2aGatewayError> {
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

    /// Forward a `message/stream` request to the upstream.
    pub async fn send_streaming_message(
        &self,
        request: SendMessageRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<StreamResponse, A2aGatewayError>> + Send>>,
        A2aGatewayError,
    > {
        self.transport
            .send_streaming_message(&self.endpoint, request)
            .await
            .map_err(|e| A2aGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: e.to_string(),
            })
    }

    /// Forward a `tasks/resubscribe` request to the upstream.
    pub async fn subscribe_to_task(
        &self,
        task_id: &str,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<StreamResponse, A2aGatewayError>> + Send>>,
        A2aGatewayError,
    > {
        self.transport
            .subscribe_to_task(&self.endpoint, task_id)
            .await
            .map_err(|e| A2aGatewayError::UpstreamCall {
                name: self.name.clone(),
                reason: e.to_string(),
            })
    }
}

/// Wrap a fallible stream so that the first error terminates the stream
/// (logged at warn level) instead of silently dropping individual errors.
///
/// Uses a channel bridge because `tokio_stream::StreamExt` does not
/// provide `scan` / `map_while` combinators on `dyn Stream`.
fn terminate_on_error(
    source: Pin<Box<dyn Stream<Item = Result<StreamResponse, A2aGatewayError>> + Send>>,
    agent_name: String,
) -> impl Stream<Item = StreamResponse> {
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    tokio::spawn(async move {
        tokio::pin!(source);
        while let Some(item) = source.next().await {
            match item {
                Ok(event) => {
                    if tx.send(event).await.is_err() {
                        break; // Receiver dropped.
                    }
                }
                Err(e) => {
                    tracing::warn!(agent = %agent_name, error = %e, "streaming error; ending stream");
                    break;
                }
            }
        }
    });
    tokio_stream::wrappers::ReceiverStream::new(rx)
}

// ── ToolProvider impl ──────────────────────────────────────────────

impl ToolProvider for UpstreamA2aAgent {
    fn provider_name(&self) -> &str {
        &self.name
    }

    async fn call_tool(
        &self,
        _tool_id: &str,
        arguments: serde_json::Value,
    ) -> BResult<ToolCallResult> {
        // A2A agents receive a message (text), not a structured tool call.
        // The arguments are expected to contain a "message" field with the
        // user's text, or we serialize the full arguments as the message.
        let message_text = match arguments.get("message") {
            Some(serde_json::Value::String(s)) => s.clone(),
            _ => serde_json::to_string(&arguments).unwrap_or_default(),
        };

        let request = SendMessageRequest {
            message: Message {
                kind: "message".into(),
                role: MessageRole::User,
                parts: vec![Part::text(message_text)],
                message_id: format!(
                    "msg-{:x}-{:x}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos(),
                    std::process::id(),
                ),
                context_id: None,
                task_id: None,
                reference_task_ids: Vec::new(),
                metadata: None,
            },
            configuration: None,
            metadata: None,
        };

        let response = UpstreamA2aAgent::send_message(self, request)
            .await
            .map_err(|e| BitrouterError::transport(Some(&self.name), e.to_string()))?;

        Ok(a2a_response_to_tool_result(response))
    }
}

fn a2a_response_to_tool_result(response: StreamResponse) -> ToolCallResult {
    match response {
        StreamResponse::Task(task) => {
            let is_error = task.status.state == bitrouter_core::api::a2a::types::TaskState::Failed;
            let mut content: Vec<ToolContent> = task
                .artifacts
                .into_iter()
                .flat_map(|artifact| a2a_parts_to_content(artifact.parts))
                .collect();

            // Include the status message if present and no artifact content.
            if content.is_empty()
                && let Some(msg) = &task.status.message
            {
                content.extend(a2a_message_to_content(msg));
            }

            ToolCallResult {
                content,
                is_error,
                metadata: Some(serde_json::json!({
                    "task_id": task.id,
                    "state": task.status.state,
                })),
            }
        }
        StreamResponse::Message(msg) => {
            let content = a2a_parts_to_content(msg.parts);
            ToolCallResult {
                content,
                is_error: false,
                metadata: None,
            }
        }
        StreamResponse::StatusUpdate(event) => {
            let is_error = event.status.state == bitrouter_core::api::a2a::types::TaskState::Failed;
            let content = event
                .status
                .message
                .as_ref()
                .map(a2a_message_to_content)
                .unwrap_or_default();
            ToolCallResult {
                content,
                is_error,
                metadata: Some(serde_json::json!({
                    "task_id": event.task_id,
                    "state": event.status.state,
                })),
            }
        }
        StreamResponse::ArtifactUpdate(event) => {
            let content = a2a_parts_to_content(event.artifact.parts);
            ToolCallResult {
                content,
                is_error: false,
                metadata: Some(serde_json::json!({
                    "task_id": event.task_id,
                    "artifact_id": event.artifact.artifact_id,
                })),
            }
        }
    }
}

fn a2a_parts_to_content(parts: Vec<Part>) -> Vec<ToolContent> {
    parts
        .into_iter()
        .map(|part| match part {
            Part::Text { text, .. } => ToolContent::Text { text },
            Part::File { file, .. } => {
                if let Some(uri) = file.uri {
                    ToolContent::Resource { uri, text: None }
                } else if file
                    .mime_type
                    .as_deref()
                    .is_some_and(|m| m.starts_with("image/"))
                {
                    ToolContent::Image {
                        data: file.bytes.unwrap_or_default(),
                        mime_type: file
                            .mime_type
                            .unwrap_or_else(|| "application/octet-stream".into()),
                    }
                } else {
                    ToolContent::Text {
                        text: file.bytes.unwrap_or_default(),
                    }
                }
            }
            Part::Data { data, .. } => ToolContent::Json { data },
        })
        .collect()
}

fn a2a_message_to_content(msg: &Message) -> Vec<ToolContent> {
    msg.parts
        .iter()
        .map(|part| match part {
            Part::Text { text, .. } => ToolContent::Text { text: text.clone() },
            Part::Data { data, .. } => ToolContent::Json { data: data.clone() },
            Part::File { file, .. } => {
                if let Some(ref uri) = file.uri {
                    ToolContent::Resource {
                        uri: uri.clone(),
                        text: None,
                    }
                } else {
                    ToolContent::Text {
                        text: file.bytes.clone().unwrap_or_default(),
                    }
                }
            }
        })
        .collect()
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
        request: SendMessageRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamResponse> + Send>>, A2aGatewayError> {
        let name = self.name.clone();
        let inner = UpstreamA2aAgent::send_streaming_message(self, request).await?;
        Ok(Box::pin(terminate_on_error(inner, name)))
    }

    async fn subscribe_to_task(
        &self,
        task_id: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamResponse> + Send>>, A2aGatewayError> {
        let name = self.name.clone();
        let inner = UpstreamA2aAgent::subscribe_to_task(self, task_id).await?;
        Ok(Box::pin(terminate_on_error(inner, name)))
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
