//! Tests for A2A gateway filters.

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::Arc;

    use futures_core::Stream;
    use tokio::sync::broadcast;
    use warp::test::request;

    use bitrouter_a2a::card::{AgentCard, minimal_card};
    use bitrouter_a2a::error::A2aGatewayError;
    use bitrouter_a2a::message::{Message, MessageRole, Part};
    use bitrouter_a2a::request::*;
    use bitrouter_a2a::server::{A2aDiscovery, A2aProxy};
    use bitrouter_a2a::stream::StreamResponse;
    use bitrouter_a2a::task::*;

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
                role: MessageRole::Agent,
                parts: vec![Part::text("echo response")],
                message_id: "resp-1".to_string(),
                context_id: None,
                task_id: None,
                reference_task_ids: Vec::new(),
                metadata: None,
                extensions: Vec::new(),
            };
            Ok(StreamResponse::Message(msg))
        }

        async fn get_task(&self, request: GetTaskRequest) -> Result<Task, A2aGatewayError> {
            Ok(Task {
                id: request.id,
                context_id: None,
                status: TaskStatus {
                    state: TaskState::Completed,
                    timestamp: "2026-03-21T00:00:00Z".to_string(),
                    message: None,
                },
                artifacts: Vec::new(),
                history: Vec::new(),
                metadata: None,
            })
        }

        async fn cancel_task(&self, request: CancelTaskRequest) -> Result<Task, A2aGatewayError> {
            Ok(Task {
                id: request.id,
                context_id: None,
                status: TaskStatus {
                    state: TaskState::Canceled,
                    timestamp: "2026-03-21T00:00:00Z".to_string(),
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

        async fn create_push_config(
            &self,
            config: TaskPushNotificationConfig,
        ) -> Result<TaskPushNotificationConfig, A2aGatewayError> {
            Ok(config)
        }

        async fn get_push_config(
            &self,
            _task_id: &str,
            _config_id: &str,
        ) -> Result<TaskPushNotificationConfig, A2aGatewayError> {
            Ok(TaskPushNotificationConfig {
                tenant: None,
                id: Some("cfg-1".to_string()),
                task_id: Some("task-1".to_string()),
                url: "https://example.com/webhook".to_string(),
                token: None,
                authentication: None,
            })
        }

        async fn list_push_configs(
            &self,
            _task_id: &str,
        ) -> Result<ListTaskPushNotificationConfigsResponse, A2aGatewayError> {
            Ok(ListTaskPushNotificationConfigsResponse {
                configs: Vec::new(),
            })
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
            "method": "SendMessage",
            "params": {
                "message": {
                    "role": "ROLE_USER",
                    "parts": [{"text": "hello"}],
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
            "method": "GetTask",
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
                "role": "ROLE_USER",
                "parts": [{"text": "hello"}],
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
        assert_eq!(json["status"]["state"], "TASK_STATE_CANCELED");
    }
}
