//! Tests for A2A type serialization and wire format compliance.
//!
//! Filter integration tests require a running upstream agent and are
//! covered by the `tests/` integration test suite.

#[cfg(test)]
mod tests {
    use bitrouter_core::api::a2a::types::*;

    // -- v0.3.0 wire format validation ----------------------------------------

    #[test]
    fn part_types_serialize_with_kind_tag() {
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

    #[test]
    fn task_state_serializes_lowercase() {
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

    #[test]
    fn message_role_serializes_lowercase() {
        let json = serde_json::to_value(&MessageRole::User).unwrap_or_default();
        assert_eq!(json, "user");
        let json = serde_json::to_value(&MessageRole::Agent).unwrap_or_default();
        assert_eq!(json, "agent");
    }

    #[test]
    fn agent_card_round_trips_through_json() {
        let card = minimal_card(
            "test-agent",
            "A test agent",
            "1.0.0",
            "http://localhost/a2a/test-agent",
        );
        let json = serde_json::to_string(&card).expect("serialize");
        let parsed: AgentCard = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.name, "test-agent");
        assert_eq!(parsed.protocol_version, "0.3.0");
    }
}

// -- Filter integration tests -------------------------------------------------

#[cfg(test)]
mod filter_tests {
    use std::pin::Pin;
    use std::sync::Arc;

    use bitrouter_core::api::a2a::gateway::{A2aGateway, A2aProxy};
    use bitrouter_core::api::a2a::types::A2aGatewayError;
    use bitrouter_core::api::a2a::types::*;
    use bitrouter_core::observe::CallerContext;
    use futures_core::Stream;
    use warp::Filter;

    // -- Mock implementations -------------------------------------------------

    struct MockAgent;

    impl A2aProxy for MockAgent {
        async fn send_message(
            &self,
            _request: SendMessageRequest,
        ) -> Result<StreamResponse, A2aGatewayError> {
            Ok(StreamResponse::Task(make_task(
                "new-task",
                TaskState::Submitted,
            )))
        }

        async fn get_task(&self, request: GetTaskRequest) -> Result<Task, A2aGatewayError> {
            if request.id == "task-1" {
                Ok(make_task("task-1", TaskState::Completed))
            } else {
                Err(A2aGatewayError::Client(format!(
                    "task not found: {}",
                    request.id
                )))
            }
        }

        async fn cancel_task(&self, request: CancelTaskRequest) -> Result<Task, A2aGatewayError> {
            Ok(make_task(&request.id, TaskState::Canceled))
        }

        async fn list_tasks(
            &self,
            _request: ListTasksRequest,
        ) -> Result<ListTasksResponse, A2aGatewayError> {
            Ok(ListTasksResponse {
                tasks: vec![make_task("task-1", TaskState::Completed)],
                next_page_token: None,
                page_size: 1,
                total_size: 1,
            })
        }

        async fn send_streaming_message(
            &self,
            _request: SendMessageRequest,
        ) -> Result<Pin<Box<dyn Stream<Item = StreamResponse> + Send>>, A2aGatewayError> {
            let items = vec![
                StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                    task_id: "stream-task".to_string(),
                    context_id: None,
                    status: TaskStatus {
                        state: TaskState::Working,
                        timestamp: None,
                        message: None,
                    },
                    is_final: false,
                    metadata: None,
                }),
                StreamResponse::Task(make_task("stream-task", TaskState::Completed)),
            ];
            Ok(Box::pin(tokio_stream::iter(items)))
        }

        async fn subscribe_to_task(
            &self,
            task_id: &str,
        ) -> Result<Pin<Box<dyn Stream<Item = StreamResponse> + Send>>, A2aGatewayError> {
            let items = vec![
                StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                    task_id: task_id.to_string(),
                    context_id: None,
                    status: TaskStatus {
                        state: TaskState::Working,
                        timestamp: None,
                        message: None,
                    },
                    is_final: false,
                    metadata: None,
                }),
                StreamResponse::Task(make_task(task_id, TaskState::Completed)),
            ];
            Ok(Box::pin(tokio_stream::iter(items)))
        }

        async fn get_extended_agent_card(&self) -> Result<AgentCard, A2aGatewayError> {
            Ok(make_card())
        }

        async fn set_push_config(
            &self,
            config: TaskPushNotificationConfig,
        ) -> Result<TaskPushNotificationConfig, A2aGatewayError> {
            Ok(config)
        }

        async fn get_push_config(
            &self,
            task_id: &str,
            config_id: Option<&str>,
        ) -> Result<TaskPushNotificationConfig, A2aGatewayError> {
            Ok(TaskPushNotificationConfig {
                task_id: task_id.to_string(),
                push_notification_config: PushNotificationConfig {
                    id: config_id.map(|s| s.to_string()),
                    url: "https://example.com/push".to_string(),
                    token: None,
                    authentication: None,
                },
            })
        }

        async fn list_push_configs(
            &self,
            task_id: &str,
        ) -> Result<Vec<TaskPushNotificationConfig>, A2aGatewayError> {
            Ok(vec![TaskPushNotificationConfig {
                task_id: task_id.to_string(),
                push_notification_config: PushNotificationConfig {
                    id: Some("cfg-1".to_string()),
                    url: "https://example.com/push".to_string(),
                    token: None,
                    authentication: None,
                },
            }])
        }

        async fn delete_push_config(
            &self,
            _task_id: &str,
            _config_id: &str,
        ) -> Result<(), A2aGatewayError> {
            Ok(())
        }
    }

    struct MockGateway {
        agent: MockAgent,
        card: AgentCard,
    }

    impl MockGateway {
        fn new() -> Self {
            Self {
                agent: MockAgent,
                card: make_card(),
            }
        }
    }

    impl A2aGateway for MockGateway {
        type Agent = MockAgent;

        fn require_agent(&self, name: &str) -> Result<&Self::Agent, A2aGatewayError> {
            if name == "test-agent" {
                Ok(&self.agent)
            } else {
                Err(A2aGatewayError::AgentNotFound {
                    name: name.to_string(),
                })
            }
        }

        async fn get_card(&self, name: &str) -> Option<AgentCard> {
            if name == "test-agent" {
                Some(self.card.clone())
            } else {
                None
            }
        }
    }

    // -- Helpers --------------------------------------------------------------

    fn make_card() -> AgentCard {
        minimal_card(
            "test-agent",
            "A mock test agent",
            "1.0.0",
            "http://localhost/a2a/test-agent",
        )
    }

    fn make_task(id: &str, state: TaskState) -> Task {
        Task {
            kind: "task".to_string(),
            id: id.to_string(),
            context_id: "ctx-1".to_string(),
            status: TaskStatus {
                state,
                timestamp: None,
                message: None,
            },
            artifacts: Vec::new(),
            history: Vec::new(),
            metadata: None,
        }
    }

    fn make_filter() -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
    {
        let gateway = Arc::new(MockGateway::new());
        crate::router::a2a::a2a_gateway_filter(
            Some(gateway),
            None,
            warp::any().and_then(|| async { Ok::<_, warp::Rejection>(CallerContext::default()) }),
        )
    }

    fn jsonrpc_body(id: &str, method: &str, params: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        })
    }

    // -- Discovery tests ------------------------------------------------------

    #[tokio::test]
    async fn well_known_agent_card() {
        let filter = make_filter();
        let resp = warp::test::request()
            .method("GET")
            .path("/a2a/test-agent/.well-known/agent-card.json")
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = serde_json::from_slice(resp.body()).expect("json");
        assert_eq!(body["name"], "test-agent");
        assert_eq!(body["protocolVersion"], "0.3.0");
    }

    #[tokio::test]
    async fn well_known_unknown_agent() {
        let filter = make_filter();
        let resp = warp::test::request()
            .method("GET")
            .path("/a2a/unknown/.well-known/agent-card.json")
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 404);
    }

    // -- JSON-RPC dispatch tests ----------------------------------------------

    #[tokio::test]
    async fn jsonrpc_message_send() {
        let filter = make_filter();
        let body = jsonrpc_body(
            "1",
            "message/send",
            serde_json::json!({
                "message": {
                    "role": "user",
                    "parts": [{"kind": "text", "text": "Hello"}],
                    "messageId": "msg-1"
                }
            }),
        );
        let resp = warp::test::request()
            .method("POST")
            .path("/a2a/test-agent")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("json");
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], "1");
        assert!(json["result"].is_object());
        // StreamResponse::Task is internally tagged with kind = "task"
        assert_eq!(json["result"]["kind"], "task");
        assert_eq!(json["result"]["id"], "new-task");
        assert_eq!(json["result"]["status"]["state"], "submitted");
    }

    #[tokio::test]
    async fn jsonrpc_tasks_get() {
        let filter = make_filter();
        let body = jsonrpc_body("2", "tasks/get", serde_json::json!({"id": "task-1"}));
        let resp = warp::test::request()
            .method("POST")
            .path("/a2a/test-agent")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("json");
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], "2");
        assert_eq!(json["result"]["id"], "task-1");
        assert_eq!(json["result"]["status"]["state"], "completed");
    }

    #[tokio::test]
    async fn jsonrpc_tasks_cancel() {
        let filter = make_filter();
        let body = jsonrpc_body("3", "tasks/cancel", serde_json::json!({"id": "task-1"}));
        let resp = warp::test::request()
            .method("POST")
            .path("/a2a/test-agent")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("json");
        assert_eq!(json["id"], "3");
        assert_eq!(json["result"]["id"], "task-1");
        assert_eq!(json["result"]["status"]["state"], "canceled");
    }

    #[tokio::test]
    async fn jsonrpc_tasks_list() {
        let filter = make_filter();
        let body = jsonrpc_body("4", "tasks/list", serde_json::json!({}));
        let resp = warp::test::request()
            .method("POST")
            .path("/a2a/test-agent")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("json");
        assert_eq!(json["id"], "4");
        let tasks = json["result"]["tasks"].as_array().expect("tasks array");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["id"], "task-1");
    }

    #[tokio::test]
    async fn jsonrpc_extended_card() {
        let filter = make_filter();
        let body = jsonrpc_body(
            "5",
            "agent/getAuthenticatedExtendedCard",
            serde_json::json!({}),
        );
        let resp = warp::test::request()
            .method("POST")
            .path("/a2a/test-agent")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("json");
        assert_eq!(json["id"], "5");
        assert_eq!(json["result"]["name"], "test-agent");
    }

    #[tokio::test]
    async fn jsonrpc_unknown_method() {
        let filter = make_filter();
        let body = jsonrpc_body("6", "foo/bar", serde_json::json!({}));
        let resp = warp::test::request()
            .method("POST")
            .path("/a2a/test-agent")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("json");
        assert_eq!(json["id"], "6");
        assert_eq!(json["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn jsonrpc_unknown_agent() {
        let filter = make_filter();
        let body = jsonrpc_body("7", "message/send", serde_json::json!({}));
        let resp = warp::test::request()
            .method("POST")
            .path("/a2a/unknown")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("json");
        assert_eq!(json["id"], "7");
        assert!(json["error"].is_object());
        assert_eq!(json["error"]["code"], -32001);
    }

    // -- Push notification tests ----------------------------------------------

    #[tokio::test]
    async fn jsonrpc_push_set() {
        let filter = make_filter();
        let body = jsonrpc_body(
            "10",
            "tasks/pushNotificationConfig/set",
            serde_json::json!({
                "taskId": "task-1",
                "pushNotificationConfig": {
                    "url": "https://example.com/push"
                }
            }),
        );
        let resp = warp::test::request()
            .method("POST")
            .path("/a2a/test-agent")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("json");
        assert_eq!(json["id"], "10");
        assert_eq!(
            json["result"]["pushNotificationConfig"]["url"],
            "https://example.com/push"
        );
    }

    #[tokio::test]
    async fn jsonrpc_push_get() {
        let filter = make_filter();
        let body = jsonrpc_body(
            "11",
            "tasks/pushNotificationConfig/get",
            serde_json::json!({"id": "task-1"}),
        );
        let resp = warp::test::request()
            .method("POST")
            .path("/a2a/test-agent")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("json");
        assert_eq!(json["id"], "11");
        assert_eq!(json["result"]["taskId"], "task-1");
    }

    #[tokio::test]
    async fn jsonrpc_push_list() {
        let filter = make_filter();
        let body = jsonrpc_body(
            "12",
            "tasks/pushNotificationConfig/list",
            serde_json::json!({"id": "task-1"}),
        );
        let resp = warp::test::request()
            .method("POST")
            .path("/a2a/test-agent")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("json");
        assert_eq!(json["id"], "12");
        let configs = json["result"].as_array().expect("configs array");
        assert_eq!(configs.len(), 1);
    }

    #[tokio::test]
    async fn jsonrpc_push_delete() {
        let filter = make_filter();
        let body = jsonrpc_body(
            "13",
            "tasks/pushNotificationConfig/delete",
            serde_json::json!({
                "id": "task-1",
                "pushNotificationConfigId": "cfg-1"
            }),
        );
        let resp = warp::test::request()
            .method("POST")
            .path("/a2a/test-agent")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("json");
        assert_eq!(json["id"], "13");
        assert_eq!(json["result"]["success"], true);
    }

    // -- Edge case tests ------------------------------------------------------

    #[tokio::test]
    async fn well_known_card_has_etag_and_cache_control() {
        let filter = make_filter();
        let resp = warp::test::request()
            .method("GET")
            .path("/a2a/test-agent/.well-known/agent-card.json")
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        assert!(resp.headers().contains_key("etag"));
        assert!(resp.headers().contains_key("cache-control"));
        assert_eq!(
            resp.headers()
                .get("cache-control")
                .expect("cache-control header")
                .to_str()
                .unwrap_or_default(),
            "max-age=3600"
        );
    }

    #[tokio::test]
    async fn jsonrpc_id_preserved_in_response() {
        let filter = make_filter();
        let body = jsonrpc_body(
            "my-custom-id-42",
            "tasks/get",
            serde_json::json!({"id": "task-1"}),
        );
        let resp = warp::test::request()
            .method("POST")
            .path("/a2a/test-agent")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("json");
        assert_eq!(json["id"], "my-custom-id-42");
    }

    #[tokio::test]
    async fn jsonrpc_response_has_protocol_version() {
        let filter = make_filter();
        let body = jsonrpc_body("1", "tasks/get", serde_json::json!({"id": "task-1"}));
        let resp = warp::test::request()
            .method("POST")
            .path("/a2a/test-agent")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("json");
        assert_eq!(json["jsonrpc"], "2.0");
    }

    #[tokio::test]
    async fn jsonrpc_error_response_has_message() {
        let filter = make_filter();
        let body = jsonrpc_body("1", "foo/bar", serde_json::json!({}));
        let resp = warp::test::request()
            .method("POST")
            .path("/a2a/test-agent")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("json");
        assert!(json["error"]["message"].is_string());
        let msg = json["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("foo/bar"), "error should mention the method");
    }

    #[tokio::test]
    async fn jsonrpc_unknown_agent_error_code() {
        let filter = make_filter();
        let body = jsonrpc_body("1", "tasks/get", serde_json::json!({"id": "task-1"}));
        let resp = warp::test::request()
            .method("POST")
            .path("/a2a/not-registered")
            .json(&body)
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(resp.body()).expect("json");
        // Agent not found returns -32001 gateway error
        assert_eq!(json["error"]["code"], -32001);
    }
}
