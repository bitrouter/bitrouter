//! A2A protocol client.
//!
//! Speaks JSON-RPC 2.0 to any A2A v0.3.0-compliant server. Supports agent
//! discovery, task submission (`message/send`), status polling (`tasks/get`),
//! cancellation (`tasks/cancel`), listing (`tasks/list`), streaming, and
//! push notification config CRUD.

use crate::card::AgentCard;
use crate::error::A2aGatewayError;
use crate::jsonrpc::{JsonRpcRequest, JsonRpcResponse};
use crate::message::{Message, MessageRole, Part};
use crate::request::{
    CancelTaskRequest, DeleteTaskPushNotificationConfigRequest,
    GetTaskPushNotificationConfigRequest, ListTaskPushNotificationConfigsRequest,
    SendMessageRequest, TaskPushNotificationConfig,
};
use crate::task::{GetTaskRequest, ListTasksRequest, ListTasksResponse, Task};

/// Result of a `message/send` call — may be a full Task or a direct Message.
#[derive(Debug, Clone)]
pub enum SendMessageResult {
    /// Server returned a full task with status and lifecycle.
    Task(Task),
    /// Server returned a direct message response (no task lifecycle).
    Message(Message),
}

/// A2A protocol client for communicating with remote A2A servers.
pub struct A2aClient {
    http: reqwest::Client,
}

impl Default for A2aClient {
    fn default() -> Self {
        Self::new()
    }
}

impl A2aClient {
    /// Create a new A2A client.
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    /// Create a new A2A client with a custom reqwest client.
    pub fn with_http_client(http: reqwest::Client) -> Self {
        Self { http }
    }

    // ── Discovery ──────────────────────────────────────────────

    /// Fetch an Agent Card from a remote server's well-known endpoint.
    ///
    /// Resolves `{base_url}/.well-known/agent-card.json`.
    pub async fn discover(&self, base_url: &str) -> Result<AgentCard, A2aGatewayError> {
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
            .map_err(|e| A2aGatewayError::Client(format!("discovery request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(A2aGatewayError::Client(format!(
                "discovery failed (HTTP {status}): {body}"
            )));
        }

        resp.json::<AgentCard>()
            .await
            .map_err(|e| A2aGatewayError::Client(format!("failed to parse agent card: {e}")))
    }

    /// Fetch an extended Agent Card via JSON-RPC.
    pub async fn get_extended_agent_card(
        &self,
        endpoint: &str,
    ) -> Result<AgentCard, A2aGatewayError> {
        let request_id = generate_request_id();
        let rpc = JsonRpcRequest::new(
            &request_id,
            "agent/getAuthenticatedExtendedCard",
            serde_json::json!({}),
        );

        let result = self.rpc_call(endpoint, &rpc).await?;

        serde_json::from_value::<AgentCard>(result)
            .map_err(|e| A2aGatewayError::Client(format!("failed to parse agent card: {e}")))
    }

    // ── Task operations (JSON-RPC 2.0) ─────────────────────────

    /// Send a message to a remote agent.
    ///
    /// This is the `message/send` JSON-RPC method.
    pub async fn send_message(
        &self,
        endpoint: &str,
        request: SendMessageRequest,
    ) -> Result<SendMessageResult, A2aGatewayError> {
        let request_id = generate_request_id();
        let params = serde_json::to_value(&request)
            .map_err(|e| A2aGatewayError::Client(format!("failed to serialize request: {e}")))?;
        let rpc = JsonRpcRequest::new(&request_id, "message/send", params);

        let result = self.rpc_call(endpoint, &rpc).await?;

        parse_send_message_result(result)
    }

    /// Convenience: send a simple text message.
    pub async fn send_text(
        &self,
        endpoint: &str,
        text: &str,
    ) -> Result<SendMessageResult, A2aGatewayError> {
        let request = SendMessageRequest {
            message: Self::text_message(text),
            configuration: None,
            metadata: None,
        };
        self.send_message(endpoint, request).await
    }

    /// Get the current state of a task.
    ///
    /// This is the `tasks/get` JSON-RPC method.
    pub async fn get_task(
        &self,
        endpoint: &str,
        request: GetTaskRequest,
    ) -> Result<Task, A2aGatewayError> {
        let request_id = generate_request_id();
        let params = serde_json::to_value(&request)
            .map_err(|e| A2aGatewayError::Client(format!("failed to serialize request: {e}")))?;
        let rpc = JsonRpcRequest::new(&request_id, "tasks/get", params);

        let result = self.rpc_call(endpoint, &rpc).await?;

        serde_json::from_value::<Task>(result)
            .map_err(|e| A2aGatewayError::Client(format!("failed to parse task response: {e}")))
    }

    /// Cancel a running task.
    ///
    /// This is the `tasks/cancel` JSON-RPC method.
    pub async fn cancel_task(
        &self,
        endpoint: &str,
        request: CancelTaskRequest,
    ) -> Result<Task, A2aGatewayError> {
        let request_id = generate_request_id();
        let params = serde_json::to_value(&request)
            .map_err(|e| A2aGatewayError::Client(format!("failed to serialize request: {e}")))?;
        let rpc = JsonRpcRequest::new(&request_id, "tasks/cancel", params);

        let result = self.rpc_call(endpoint, &rpc).await?;

        serde_json::from_value::<Task>(result)
            .map_err(|e| A2aGatewayError::Client(format!("failed to parse task response: {e}")))
    }

    /// List tasks matching a query.
    ///
    /// This is the `tasks/list` JSON-RPC method.
    pub async fn list_tasks(
        &self,
        endpoint: &str,
        request: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2aGatewayError> {
        let request_id = generate_request_id();
        let params = serde_json::to_value(&request)
            .map_err(|e| A2aGatewayError::Client(format!("failed to serialize request: {e}")))?;
        let rpc = JsonRpcRequest::new(&request_id, "tasks/list", params);

        let result = self.rpc_call(endpoint, &rpc).await?;

        serde_json::from_value::<ListTasksResponse>(result).map_err(|e| {
            A2aGatewayError::Client(format!("failed to parse list tasks response: {e}"))
        })
    }

    // ── Push notification config CRUD ──────────────────────────

    /// Set (create or update) a push notification configuration.
    pub async fn set_push_notification_config(
        &self,
        endpoint: &str,
        config: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2aGatewayError> {
        let request_id = generate_request_id();
        let params = serde_json::to_value(&config)
            .map_err(|e| A2aGatewayError::Client(format!("failed to serialize config: {e}")))?;
        let rpc = JsonRpcRequest::new(&request_id, "tasks/pushNotificationConfig/set", params);

        let result = self.rpc_call(endpoint, &rpc).await?;

        serde_json::from_value::<TaskPushNotificationConfig>(result)
            .map_err(|e| A2aGatewayError::Client(format!("failed to parse push config: {e}")))
    }

    /// Get a push notification configuration.
    pub async fn get_push_notification_config(
        &self,
        endpoint: &str,
        request: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2aGatewayError> {
        let request_id = generate_request_id();
        let params = serde_json::to_value(&request)
            .map_err(|e| A2aGatewayError::Client(format!("failed to serialize request: {e}")))?;
        let rpc = JsonRpcRequest::new(&request_id, "tasks/pushNotificationConfig/get", params);

        let result = self.rpc_call(endpoint, &rpc).await?;

        serde_json::from_value::<TaskPushNotificationConfig>(result)
            .map_err(|e| A2aGatewayError::Client(format!("failed to parse push config: {e}")))
    }

    /// List push notification configurations for a task.
    pub async fn list_push_notification_configs(
        &self,
        endpoint: &str,
        request: ListTaskPushNotificationConfigsRequest,
    ) -> Result<Vec<TaskPushNotificationConfig>, A2aGatewayError> {
        let request_id = generate_request_id();
        let params = serde_json::to_value(&request)
            .map_err(|e| A2aGatewayError::Client(format!("failed to serialize request: {e}")))?;
        let rpc = JsonRpcRequest::new(&request_id, "tasks/pushNotificationConfig/list", params);

        let result = self.rpc_call(endpoint, &rpc).await?;

        serde_json::from_value::<Vec<TaskPushNotificationConfig>>(result)
            .map_err(|e| A2aGatewayError::Client(format!("failed to parse push configs: {e}")))
    }

    /// Delete a push notification configuration.
    pub async fn delete_push_notification_config(
        &self,
        endpoint: &str,
        request: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2aGatewayError> {
        let request_id = generate_request_id();
        let params = serde_json::to_value(&request)
            .map_err(|e| A2aGatewayError::Client(format!("failed to serialize request: {e}")))?;
        let rpc = JsonRpcRequest::new(&request_id, "tasks/pushNotificationConfig/delete", params);

        let _ = self.rpc_call(endpoint, &rpc).await?;
        Ok(())
    }

    // ── Helpers ────────────────────────────────────────────────

    /// Resolve the A2A endpoint URL from an Agent Card.
    ///
    /// Returns the card's `url` field.
    pub fn resolve_endpoint(card: &AgentCard) -> &str {
        &card.url
    }

    /// Build a simple text message.
    pub fn text_message(text: &str) -> Message {
        Message {
            kind: "message".to_string(),
            role: MessageRole::User,
            parts: vec![Part::text(text)],
            message_id: generate_request_id(),
            context_id: None,
            task_id: None,
            reference_task_ids: Vec::new(),
            metadata: None,
        }
    }

    // ── Internal ───────────────────────────────────────────────

    async fn rpc_call(
        &self,
        endpoint: &str,
        request: &JsonRpcRequest,
    ) -> Result<serde_json::Value, A2aGatewayError> {
        let resp = self
            .http
            .post(endpoint)
            .header("Content-Type", "application/json")
            .json(request)
            .send()
            .await
            .map_err(|e| A2aGatewayError::Client(format!("request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(A2aGatewayError::Client(format!("HTTP {status}: {body}")));
        }

        let rpc_resp = resp.json::<JsonRpcResponse>().await.map_err(|e| {
            A2aGatewayError::Client(format!("failed to parse JSON-RPC response: {e}"))
        })?;

        rpc_resp
            .into_result()
            .map_err(|e| A2aGatewayError::Client(format!("{e}")))
    }
}

fn parse_send_message_result(
    result: serde_json::Value,
) -> Result<SendMessageResult, A2aGatewayError> {
    // v0.3.0: result is returned directly with a `kind` discriminator.
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
            // Fallback: try as Task (has id + status) or Message.
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

fn generate_request_id() -> String {
    // Simple monotonic ID. For production use, consider UUIDs.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("req-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_message_builds_correctly() {
        let msg = A2aClient::text_message("hello world");
        assert_eq!(msg.role, MessageRole::User);
        assert_eq!(msg.kind, "message");
        assert_eq!(msg.parts.len(), 1);
        match &msg.parts[0] {
            Part::Text { text, .. } => assert_eq!(text, "hello world"),
            _ => panic!("expected text part"),
        }
    }

    #[test]
    fn resolve_endpoint_from_card() {
        let card = crate::card::minimal_card(
            "test",
            "test agent",
            "1.0.0",
            "https://agent.example.com/a2a",
        );
        let ep = A2aClient::resolve_endpoint(&card);
        assert_eq!(ep, "https://agent.example.com/a2a");
    }

    #[test]
    fn generate_request_id_increments() {
        let id1 = generate_request_id();
        let id2 = generate_request_id();
        assert_ne!(id1, id2);
        assert!(id1.starts_with("req-"));
    }
}
