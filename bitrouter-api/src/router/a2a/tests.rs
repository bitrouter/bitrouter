//! Tests for A2A gateway filters.

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::Arc;

    use futures_core::Stream;
    use tokio::sync::broadcast;
    use warp::test::request;

    use bitrouter_a2a::error::A2aGatewayError;
    use bitrouter_a2a::server::{A2aDiscovery, A2aProxy};
    use bitrouter_a2a::types::*;

    use super::super::filters::a2a_gateway_filter;

    struct MockGateway {
        card: AgentCard,
        card_tx: broadcast::Sender<()>,
    }

    impl MockGateway {
        fn new() -> Self {
            let (card_tx, _) = broadcast::channel(16);
            Self {
                card: minimal_card(
                    "mock-agent",
                    "A mock agent",
                    "1.0.0",
                    "http://localhost/a2a",
                ),
                card_tx,
            }
        }
    }

    impl A2aDiscovery for MockGateway {
        async fn get_agent_card(&self) -> Option<AgentCard> {
            Some(self.card.clone())
        }

        fn subscribe_card_changes(&self) -> broadcast::Receiver<()> {
            self.card_tx.subscribe()
        }
    }

    impl A2aProxy for MockGateway {
        async fn send_message(
            &self,
            _request: SendMessageRequest,
        ) -> Result<StreamResponse, A2aGatewayError> {
            let msg = Message {
                kind: "message".to_string(),
                role: MessageRole::Agent,
                parts: vec![Part::text("echo response")],
                message_id: "resp-1".to_string(),
                context_id: None,
                task_id: None,
                reference_task_ids: Vec::new(),
                metadata: None,
            };
            Ok(StreamResponse::Message(msg))
        }

        async fn get_task(&self, request: GetTaskRequest) -> Result<Task, A2aGatewayError> {
            Ok(Task {
                kind: "task".to_string(),
                id: request.id,
                context_id: "ctx-default".to_string(),
                status: TaskStatus {
                    state: TaskState::Completed,
                    timestamp: Some("2026-03-21T00:00:00Z".to_string()),
                    message: None,
                },
                artifacts: Vec::new(),
                history: Vec::new(),
                metadata: None,
            })
        }

        async fn cancel_task(&self, request: CancelTaskRequest) -> Result<Task, A2aGatewayError> {
            Ok(Task {
                kind: "task".to_string(),
                id: request.id,
                context_id: "ctx-default".to_string(),
                status: TaskStatus {
                    state: TaskState::Canceled,
                    timestamp: Some("2026-03-21T00:00:00Z".to_string()),
                    message: None,
                },
                artifacts: Vec::new(),
                history: Vec::new(),
                metadata: None,
            })
        }

        async fn list_tasks(
            &self,
            _request: ListTasksRequest,
        ) -> Result<ListTasksResponse, A2aGatewayError> {
            Ok(ListTasksResponse {
                tasks: Vec::new(),
                next_page_token: None,
                page_size: 0,
                total_size: 0,
            })
        }

        async fn send_streaming_message(
            &self,
            _request: SendMessageRequest,
        ) -> Result<Pin<Box<dyn Stream<Item = StreamResponse> + Send>>, A2aGatewayError> {
            Err(A2aGatewayError::UpstreamCall {
                name: "mock".to_string(),
                reason: "streaming not implemented in mock".to_string(),
            })
        }

        async fn subscribe_to_task(
            &self,
            _task_id: &str,
        ) -> Result<Pin<Box<dyn Stream<Item = StreamResponse> + Send>>, A2aGatewayError> {
            Err(A2aGatewayError::UpstreamCall {
                name: "mock".to_string(),
                reason: "streaming not implemented in mock".to_string(),
            })
        }

        async fn get_extended_agent_card(&self) -> Result<AgentCard, A2aGatewayError> {
            Ok(self.card.clone())
        }

        async fn set_push_config(
            &self,
            config: TaskPushNotificationConfig,
        ) -> Result<TaskPushNotificationConfig, A2aGatewayError> {
            Ok(config)
        }

        async fn get_push_config(
            &self,
            _task_id: &str,
            _config_id: Option<&str>,
        ) -> Result<TaskPushNotificationConfig, A2aGatewayError> {
            Ok(TaskPushNotificationConfig {
                task_id: "task-1".to_string(),
                push_notification_config: PushNotificationConfig {
                    id: Some("cfg-1".to_string()),
                    url: "https://example.com/webhook".to_string(),
                    token: None,
                    authentication: None,
                },
            })
        }

        async fn list_push_configs(
            &self,
            _task_id: &str,
        ) -> Result<Vec<TaskPushNotificationConfig>, A2aGatewayError> {
            Ok(Vec::new())
        }

        async fn delete_push_config(
            &self,
            _task_id: &str,
            _config_id: &str,
        ) -> Result<(), A2aGatewayError> {
            Ok(())
        }
    }

    fn mock_filter()
    -> impl warp::Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        let gw: Option<Arc<MockGateway>> = Some(Arc::new(MockGateway::new()));
        a2a_gateway_filter(gw)
    }

    #[tokio::test]
    async fn discovery_returns_agent_card() {
        let filter = mock_filter();
        let resp = request()
            .method("GET")
            .path("/.well-known/agent-card.json")
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let card: AgentCard = serde_json::from_slice(resp.body()).unwrap_or_else(|e| {
            panic!("failed to parse card: {e}");
        });
        assert_eq!(card.name, "mock-agent");
    }

    #[tokio::test]
    async fn discovery_404_when_no_gateway() {
        let gw: Option<Arc<MockGateway>> = None;
        let filter = a2a_gateway_filter(gw);
        let resp = request()
            .method("GET")
            .path("/.well-known/agent-card.json")
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn jsonrpc_send_message() {
        let filter = mock_filter();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "req-1",
            "method": "message/send",
            "params": {
                "message": {
                    "kind": "message",
                    "role": "user",
                    "parts": [{"kind": "text", "text": "hello"}],
                    "messageId": "msg-1"
                }
            }
        });
        let resp = request()
            .method("POST")
            .path("/a2a")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).unwrap_or_default();
        assert_eq!(json["id"], "req-1");
        assert!(json["result"].is_object());
    }

    #[tokio::test]
    async fn jsonrpc_get_task() {
        let filter = mock_filter();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "req-2",
            "method": "tasks/get",
            "params": {"id": "task-123"}
        });
        let resp = request()
            .method("POST")
            .path("/a2a")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).unwrap_or_default();
        assert_eq!(json["result"]["id"], "task-123");
    }

    #[tokio::test]
    async fn jsonrpc_unknown_method() {
        let filter = mock_filter();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "req-3",
            "method": "UnknownMethod",
            "params": {}
        });
        let resp = request()
            .method("POST")
            .path("/a2a")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).unwrap_or_default();
        assert_eq!(json["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn rest_send_message() {
        let filter = mock_filter();
        let body = serde_json::json!({
            "message": {
                "kind": "message",
                "role": "user",
                "parts": [{"kind": "text", "text": "hello"}],
                "messageId": "msg-1"
            }
        });
        let resp = request()
            .method("POST")
            .path("/message:send")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn rest_get_task() {
        let filter = mock_filter();
        let resp = request()
            .method("GET")
            .path("/tasks/task-456")
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).unwrap_or_default();
        assert_eq!(json["id"], "task-456");
    }

    #[tokio::test]
    async fn rest_cancel_task() {
        let filter = mock_filter();
        let resp = request()
            .method("POST")
            .path("/tasks/task-789:cancel")
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).unwrap_or_default();
        assert_eq!(json["id"], "task-789");
        assert_eq!(json["status"]["state"], "canceled");
    }

    // ── v0.3.0 wire format validation ────────────────────────────────

    #[tokio::test]
    async fn agent_card_matches_v030_schema() {
        let filter = mock_filter();
        let resp = request()
            .method("GET")
            .path("/.well-known/agent-card.json")
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);

        let json: serde_json::Value =
            serde_json::from_slice(resp.body()).unwrap_or_else(|e| panic!("parse: {e}"));

        // v0.3.0 required fields
        assert_eq!(json["protocolVersion"], "0.3.0");
        assert!(json["name"].is_string());
        assert!(json["description"].is_string());
        assert!(json["version"].is_string());
        assert!(json["url"].is_string());
        assert!(json["defaultInputModes"].is_array());
        assert!(json["defaultOutputModes"].is_array());
        assert!(json["skills"].is_array());
        assert!(json["capabilities"].is_object());

        // v0.3.0: NO supportedInterfaces (that's v1.0)
        assert!(json.get("supportedInterfaces").is_none());

        // v0.3.0: preferredTransport is optional string, not nested object
        assert!(json.get("preferredTransport").is_none() || json["preferredTransport"].is_string());
    }

    #[tokio::test]
    async fn message_send_response_has_v030_kind_fields() {
        let filter = mock_filter();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "req-fmt-1",
            "method": "message/send",
            "params": {
                "message": {
                    "kind": "message",
                    "role": "user",
                    "parts": [{"kind": "text", "text": "hello"}],
                    "messageId": "msg-fmt-1"
                }
            }
        });
        let resp = request()
            .method("POST")
            .path("/a2a")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);

        let json: serde_json::Value =
            serde_json::from_slice(resp.body()).unwrap_or_else(|e| panic!("parse: {e}"));
        let result = &json["result"];

        // v0.3.0: StreamResponse is internally tagged with "kind"
        assert_eq!(result["kind"], "message");

        // v0.3.0: role is lowercase "agent" not "ROLE_AGENT"
        assert_eq!(result["role"], "agent");

        // v0.3.0: parts have "kind" discriminator
        let part = &result["parts"][0];
        assert_eq!(part["kind"], "text");
        assert!(part["text"].is_string());

        // v0.3.0: no "ROLE_" prefix anywhere
        let raw = std::str::from_utf8(resp.body()).unwrap_or_default();
        assert!(
            !raw.contains("ROLE_"),
            "found v1.0 ROLE_ prefix in response"
        );
    }

    #[tokio::test]
    async fn task_get_response_has_v030_format() {
        let filter = mock_filter();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "req-fmt-2",
            "method": "tasks/get",
            "params": {"id": "task-fmt-1"}
        });
        let resp = request()
            .method("POST")
            .path("/a2a")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);

        let json: serde_json::Value =
            serde_json::from_slice(resp.body()).unwrap_or_else(|e| panic!("parse: {e}"));
        let task = &json["result"];

        // v0.3.0: Task has kind "task"
        assert_eq!(task["kind"], "task");

        // v0.3.0: contextId is required (present, not null)
        assert!(task["contextId"].is_string());

        // v0.3.0: TaskState is lowercase
        assert_eq!(task["status"]["state"], "completed");

        // v0.3.0: no TASK_STATE_ prefix
        let raw = std::str::from_utf8(resp.body()).unwrap_or_default();
        assert!(
            !raw.contains("TASK_STATE_"),
            "found v1.0 TASK_STATE_ prefix in response"
        );
    }

    #[tokio::test]
    async fn v030_method_names_accepted() {
        let filter = mock_filter();

        // v0.3.0 method: tasks/cancel
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "req-v030-1",
            "method": "tasks/cancel",
            "params": {"id": "task-1"}
        });
        let resp = request()
            .method("POST")
            .path("/a2a")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).unwrap_or_default();
        assert!(json.get("error").is_none(), "tasks/cancel should succeed");

        // v0.3.0 method: agent/getAuthenticatedExtendedCard
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "req-v030-2",
            "method": "agent/getAuthenticatedExtendedCard",
            "params": {}
        });
        let resp = request()
            .method("POST")
            .path("/a2a")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).unwrap_or_default();
        assert!(
            json.get("error").is_none(),
            "agent/getAuthenticatedExtendedCard should succeed"
        );
    }

    #[tokio::test]
    async fn v10_method_names_rejected() {
        let filter = mock_filter();

        // v1.0 PascalCase names should NOT be recognized
        for method in &[
            "SendMessage",
            "GetTask",
            "CancelTask",
            "GetExtendedAgentCard",
        ] {
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": "req-reject",
                "method": method,
                "params": {}
            });
            let resp = request()
                .method("POST")
                .path("/a2a")
                .json(&body)
                .reply(&filter)
                .await;
            let json: serde_json::Value = serde_json::from_slice(resp.body()).unwrap_or_default();
            assert_eq!(
                json["error"]["code"], -32601,
                "v1.0 method {method} should be rejected as unknown"
            );
        }
    }

    #[tokio::test]
    async fn part_types_serialize_with_kind_tag() {
        // Verify the Part enum serializes with the "kind" discriminator
        let text_part = Part::text("hello");
        let json = serde_json::to_value(&text_part).unwrap_or_default();
        assert_eq!(json["kind"], "text");
        assert_eq!(json["text"], "hello");

        let data_part = Part::data(serde_json::json!({"key": "value"}));
        let json = serde_json::to_value(&data_part).unwrap_or_default();
        assert_eq!(json["kind"], "data");
        assert!(json["data"].is_object());

        let file_part = Part::file_uri("https://example.com/f.png", Some("f.png".to_string()));
        let json = serde_json::to_value(&file_part).unwrap_or_default();
        assert_eq!(json["kind"], "file");
        assert!(json["file"].is_object());
        assert_eq!(json["file"]["uri"], "https://example.com/f.png");
    }

    #[tokio::test]
    async fn task_state_serializes_lowercase() {
        // v0.3.0: states are lowercase with hyphens
        let cases = vec![
            (TaskState::Submitted, "submitted"),
            (TaskState::Working, "working"),
            (TaskState::Completed, "completed"),
            (TaskState::Failed, "failed"),
            (TaskState::Canceled, "canceled"),
            (TaskState::Rejected, "rejected"),
            (TaskState::InputRequired, "input-required"),
            (TaskState::AuthRequired, "auth-required"),
            (TaskState::Unknown, "unknown"),
        ];
        for (state, expected) in cases {
            let json = serde_json::to_value(&state).unwrap_or_default();
            assert_eq!(
                json.as_str().unwrap_or_default(),
                expected,
                "TaskState::{state:?} should serialize as \"{expected}\""
            );
        }
    }

    #[tokio::test]
    async fn message_role_serializes_lowercase() {
        let json = serde_json::to_value(&MessageRole::User).unwrap_or_default();
        assert_eq!(json, "user");
        let json = serde_json::to_value(&MessageRole::Agent).unwrap_or_default();
        assert_eq!(json, "agent");
    }
}
