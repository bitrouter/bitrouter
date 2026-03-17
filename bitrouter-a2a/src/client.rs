//! A2A protocol client.
//!
//! Speaks JSON-RPC 2.0 to any A2A-compliant server. Supports agent discovery,
//! task submission (`message/send`), status polling (`tasks/get`), and
//! cancellation (`tasks/cancel`).

use crate::card::AgentCard;
use crate::error::A2aError;
use crate::jsonrpc::{JsonRpcRequest, JsonRpcResponse};
use crate::message::{Message, Part};
use crate::task::Task;

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
    pub async fn discover(&self, base_url: &str) -> Result<AgentCard, A2aError> {
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
            .map_err(|e| A2aError::Client(format!("discovery request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(A2aError::Client(format!(
                "discovery failed (HTTP {status}): {body}"
            )));
        }

        resp.json::<AgentCard>()
            .await
            .map_err(|e| A2aError::Client(format!("failed to parse agent card: {e}")))
    }

    // ── Task operations (JSON-RPC 2.0) ─────────────────────────

    /// Send a message to a remote agent and wait for the task to complete.
    ///
    /// This is the `message/send` JSON-RPC method. The endpoint URL should
    /// be the agent's A2A interface URL from its Agent Card.
    pub async fn send_message(&self, endpoint: &str, message: Message) -> Result<Task, A2aError> {
        let request_id = generate_request_id();
        let rpc = JsonRpcRequest::new(
            &request_id,
            "message/send",
            serde_json::json!({
                "message": message,
            }),
        );

        let result = self.rpc_call(endpoint, &rpc).await?;

        serde_json::from_value::<Task>(result)
            .map_err(|e| A2aError::Client(format!("failed to parse task response: {e}")))
    }

    /// Get the current state of a task.
    ///
    /// This is the `tasks/get` JSON-RPC method.
    pub async fn get_task(&self, endpoint: &str, task_id: &str) -> Result<Task, A2aError> {
        let request_id = generate_request_id();
        let rpc = JsonRpcRequest::new(
            &request_id,
            "tasks/get",
            serde_json::json!({
                "id": task_id,
            }),
        );

        let result = self.rpc_call(endpoint, &rpc).await?;

        serde_json::from_value::<Task>(result)
            .map_err(|e| A2aError::Client(format!("failed to parse task response: {e}")))
    }

    /// Cancel a running task.
    ///
    /// This is the `tasks/cancel` JSON-RPC method.
    pub async fn cancel_task(&self, endpoint: &str, task_id: &str) -> Result<Task, A2aError> {
        let request_id = generate_request_id();
        let rpc = JsonRpcRequest::new(
            &request_id,
            "tasks/cancel",
            serde_json::json!({
                "id": task_id,
            }),
        );

        let result = self.rpc_call(endpoint, &rpc).await?;

        serde_json::from_value::<Task>(result)
            .map_err(|e| A2aError::Client(format!("failed to parse task response: {e}")))
    }

    // ── Helpers ────────────────────────────────────────────────

    /// Resolve the A2A endpoint URL from an Agent Card.
    ///
    /// Returns the first `supported_interfaces` URL, or `None` if the card
    /// has no interfaces.
    pub fn resolve_endpoint(card: &AgentCard) -> Option<&str> {
        card.supported_interfaces.first().map(|i| i.url.as_str())
    }

    /// Build a simple text message.
    pub fn text_message(text: &str) -> Message {
        Message {
            role: crate::message::MessageRole::User,
            parts: vec![Part::Text {
                text: text.to_string(),
            }],
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
    ) -> Result<serde_json::Value, A2aError> {
        let resp = self
            .http
            .post(endpoint)
            .header("Content-Type", "application/json")
            .json(request)
            .send()
            .await
            .map_err(|e| A2aError::Client(format!("request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(A2aError::Client(format!("HTTP {status}: {body}")));
        }

        let rpc_resp = resp
            .json::<JsonRpcResponse>()
            .await
            .map_err(|e| A2aError::Client(format!("failed to parse JSON-RPC response: {e}")))?;

        rpc_resp
            .into_result()
            .map_err(|e| A2aError::Client(format!("{e}")))
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
        assert_eq!(msg.role, crate::message::MessageRole::User);
        assert_eq!(msg.parts.len(), 1);
        match &msg.parts[0] {
            Part::Text { text } => assert_eq!(text, "hello world"),
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
        assert_eq!(ep, Some("https://agent.example.com/a2a"));
    }

    #[test]
    fn resolve_endpoint_empty_card() {
        let mut card = crate::card::minimal_card("test", "test", "1.0.0", "http://localhost");
        card.supported_interfaces.clear();
        assert_eq!(A2aClient::resolve_endpoint(&card), None);
    }

    #[test]
    fn generate_request_id_increments() {
        let id1 = generate_request_id();
        let id2 = generate_request_id();
        assert_ne!(id1, id2);
        assert!(id1.starts_with("req-"));
    }
}
