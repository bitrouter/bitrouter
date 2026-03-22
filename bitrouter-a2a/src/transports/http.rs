//! HTTP+JSON (REST) transport for the A2A protocol.
//!
//! Implements [`A2aTransport`] using RESTful HTTP routes per the A2A v0.3.0
//! specification. This transport is equivalent to gRPC-HTTP transcoding
//! and uses the same serde types as the JSON-RPC transport without the
//! JSON-RPC 2.0 envelope.

use crate::error::A2aGatewayError;
use crate::types::{
    AgentCard, CancelTaskRequest, DeleteTaskPushNotificationConfigRequest,
    GetTaskPushNotificationConfigRequest, GetTaskRequest, ListTaskPushNotificationConfigsRequest,
    ListTasksRequest, ListTasksResponse, Message, SendMessageRequest, SendMessageResult, Task,
    TaskPushNotificationConfig,
};

/// A2A REST (HTTP+JSON) transport client.
pub struct A2aRestClient {
    http: reqwest::Client,
}

impl Default for A2aRestClient {
    fn default() -> Self {
        Self::new()
    }
}

impl A2aRestClient {
    /// Create a new REST transport client.
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    /// Create a new REST transport client with a custom reqwest client.
    pub fn with_http_client(http: reqwest::Client) -> Self {
        Self { http }
    }

    fn client_error(msg: String) -> A2aGatewayError {
        A2aGatewayError::Client(msg)
    }
}

impl super::A2aTransport for A2aRestClient {
    async fn discover(&self, base_url: &str) -> Result<AgentCard, A2aGatewayError> {
        let url = format!(
            "{}/.well-known/agent-card.json",
            base_url.trim_end_matches('/')
        );
        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| Self::client_error(format!("discovery request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::client_error(format!(
                "discovery failed (HTTP {status}): {body}"
            )));
        }
        resp.json::<AgentCard>()
            .await
            .map_err(|e| Self::client_error(format!("failed to parse agent card: {e}")))
    }

    async fn get_extended_agent_card(&self, endpoint: &str) -> Result<AgentCard, A2aGatewayError> {
        let url = format!("{}/v1/card", endpoint.trim_end_matches('/'));
        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| Self::client_error(format!("get card request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::client_error(format!(
                "get card failed (HTTP {status}): {body}"
            )));
        }
        resp.json::<AgentCard>()
            .await
            .map_err(|e| Self::client_error(format!("failed to parse agent card: {e}")))
    }

    async fn send_message(
        &self,
        endpoint: &str,
        request: SendMessageRequest,
    ) -> Result<SendMessageResult, A2aGatewayError> {
        let url = format!("{}/v1/message:send", endpoint.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| Self::client_error(format!("send message request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::client_error(format!(
                "send message failed (HTTP {status}): {body}"
            )));
        }
        let result: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Self::client_error(format!("failed to parse response: {e}")))?;
        parse_send_message_result(result)
    }

    async fn get_task(
        &self,
        endpoint: &str,
        request: GetTaskRequest,
    ) -> Result<Task, A2aGatewayError> {
        let mut url = format!("{}/v1/tasks/{}", endpoint.trim_end_matches('/'), request.id);
        if let Some(hl) = request.history_length {
            url.push_str(&format!("?historyLength={hl}"));
        }
        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| Self::client_error(format!("get task request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::client_error(format!(
                "get task failed (HTTP {status}): {body}"
            )));
        }
        resp.json::<Task>()
            .await
            .map_err(|e| Self::client_error(format!("failed to parse task: {e}")))
    }

    async fn cancel_task(
        &self,
        endpoint: &str,
        request: CancelTaskRequest,
    ) -> Result<Task, A2aGatewayError> {
        let url = format!(
            "{}/v1/tasks/{}:cancel",
            endpoint.trim_end_matches('/'),
            request.id
        );
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .send()
            .await
            .map_err(|e| Self::client_error(format!("cancel task request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::client_error(format!(
                "cancel task failed (HTTP {status}): {body}"
            )));
        }
        resp.json::<Task>()
            .await
            .map_err(|e| Self::client_error(format!("failed to parse task: {e}")))
    }

    async fn list_tasks(
        &self,
        endpoint: &str,
        request: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2aGatewayError> {
        let url = format!("{}/v1/tasks", endpoint.trim_end_matches('/'));
        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .query(&request)
            .send()
            .await
            .map_err(|e| Self::client_error(format!("list tasks request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::client_error(format!(
                "list tasks failed (HTTP {status}): {body}"
            )));
        }
        resp.json::<ListTasksResponse>()
            .await
            .map_err(|e| Self::client_error(format!("failed to parse list tasks response: {e}")))
    }

    async fn set_push_config(
        &self,
        endpoint: &str,
        config: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2aGatewayError> {
        let url = format!(
            "{}/v1/tasks/{}/pushNotificationConfigs",
            endpoint.trim_end_matches('/'),
            config.task_id
        );
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&config.push_notification_config)
            .send()
            .await
            .map_err(|e| Self::client_error(format!("set push config request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::client_error(format!(
                "set push config failed (HTTP {status}): {body}"
            )));
        }
        resp.json::<TaskPushNotificationConfig>()
            .await
            .map_err(|e| Self::client_error(format!("failed to parse push config: {e}")))
    }

    async fn get_push_config(
        &self,
        endpoint: &str,
        request: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2aGatewayError> {
        let mut url = format!(
            "{}/v1/tasks/{}/pushNotificationConfigs",
            endpoint.trim_end_matches('/'),
            request.id
        );
        if let Some(ref config_id) = request.push_notification_config_id {
            url.push_str(&format!("/{config_id}"));
        }
        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| Self::client_error(format!("get push config request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::client_error(format!(
                "get push config failed (HTTP {status}): {body}"
            )));
        }
        resp.json::<TaskPushNotificationConfig>()
            .await
            .map_err(|e| Self::client_error(format!("failed to parse push config: {e}")))
    }

    async fn list_push_configs(
        &self,
        endpoint: &str,
        request: ListTaskPushNotificationConfigsRequest,
    ) -> Result<Vec<TaskPushNotificationConfig>, A2aGatewayError> {
        let url = format!(
            "{}/v1/tasks/{}/pushNotificationConfigs",
            endpoint.trim_end_matches('/'),
            request.id
        );
        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| Self::client_error(format!("list push configs request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::client_error(format!(
                "list push configs failed (HTTP {status}): {body}"
            )));
        }
        resp.json::<Vec<TaskPushNotificationConfig>>()
            .await
            .map_err(|e| Self::client_error(format!("failed to parse push configs: {e}")))
    }

    async fn delete_push_config(
        &self,
        endpoint: &str,
        request: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2aGatewayError> {
        let url = format!(
            "{}/v1/tasks/{}/pushNotificationConfigs/{}",
            endpoint.trim_end_matches('/'),
            request.id,
            request.push_notification_config_id
        );
        let resp =
            self.http.delete(&url).send().await.map_err(|e| {
                Self::client_error(format!("delete push config request failed: {e}"))
            })?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::client_error(format!(
                "delete push config failed (HTTP {status}): {body}"
            )));
        }
        Ok(())
    }
}

fn parse_send_message_result(
    result: serde_json::Value,
) -> Result<SendMessageResult, A2aGatewayError> {
    match result.get("kind").and_then(|v| v.as_str()) {
        Some("task") => {
            let task = serde_json::from_value::<Task>(result).map_err(|e| {
                A2aGatewayError::Client(format!("failed to parse task response: {e}"))
            })?;
            Ok(SendMessageResult::Task(task))
        }
        Some("message") => {
            let msg = serde_json::from_value::<Message>(result).map_err(|e| {
                A2aGatewayError::Client(format!("failed to parse message response: {e}"))
            })?;
            Ok(SendMessageResult::Message(msg))
        }
        _ => {
            if result.get("id").is_some() && result.get("status").is_some() {
                let task = serde_json::from_value::<Task>(result).map_err(|e| {
                    A2aGatewayError::Client(format!("failed to parse task response: {e}"))
                })?;
                Ok(SendMessageResult::Task(task))
            } else {
                let msg = serde_json::from_value::<Message>(result).map_err(|e| {
                    A2aGatewayError::Client(format!("failed to parse SendMessage result: {e}"))
                })?;
                Ok(SendMessageResult::Message(msg))
            }
        }
    }
}
