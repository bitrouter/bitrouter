//! JSON-RPC 2.0 endpoint for A2A v1.0.
//!
//! Provides `POST /a2a` dispatch for all A2A methods, routing streaming
//! methods to SSE and everything else to standard JSON responses.

pub mod convert;
pub mod dispatch;

use std::sync::Arc;

use warp::Filter;

use bitrouter_a2a::jsonrpc::JsonRpcRequest;
use bitrouter_a2a::registry::AgentCardRegistry;
use bitrouter_a2a::server::{AgentExecutor, PushNotificationStore, TaskStore};

/// Creates a warp filter for `POST /a2a` JSON-RPC dispatch.
///
/// Accepts JSON-RPC 2.0 requests and dispatches all A2A v1.0 methods
/// to the appropriate handlers. Streaming methods (`SendStreamingMessage`,
/// `SubscribeToTask`) return SSE responses; all others return JSON.
pub fn jsonrpc_filter<E, S, R, P>(
    executor: Arc<E>,
    task_store: Arc<S>,
    registry: Arc<R>,
    push_store: Arc<P>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    E: AgentExecutor + 'static,
    S: TaskStore + 'static,
    R: AgentCardRegistry + 'static,
    P: PushNotificationStore + 'static,
{
    warp::path("a2a")
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::body::json::<JsonRpcRequest>())
        .and(warp::any().map(move || executor.clone()))
        .and(warp::any().map(move || task_store.clone()))
        .and(warp::any().map(move || registry.clone()))
        .and(warp::any().map(move || push_store.clone()))
        .then(
            |request: JsonRpcRequest,
             executor: Arc<E>,
             task_store: Arc<S>,
             registry: Arc<R>,
             push_store: Arc<P>| async move {
                // Streaming methods return SSE; everything else returns JSON.
                match request.method.as_str() {
                    "SendStreamingMessage" | "SubscribeToTask" => {
                        super::streaming::handler::handle_streaming_request(
                            request, executor, task_store,
                        )
                        .await
                    }
                    _ => {
                        let response = dispatch::handle_jsonrpc(
                            request, executor, task_store, registry, push_store,
                        )
                        .await;
                        Box::new(warp::reply::json(&response)) as Box<dyn warp::Reply>
                    }
                }
            },
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::a2a::test_helpers::*;

    #[tokio::test]
    async fn jsonrpc_send_message_returns_task() {
        let (filter, _dir) = build_jsonrpc_filter();

        let body = jsonrpc_body(
            "SendMessage",
            serde_json::json!({
                "message": {
                    "role": "ROLE_USER",
                    "messageId": "msg-1",
                    "parts": [{"text": "hello"}]
                }
            }),
        );

        let resp = warp::test::request()
            .method("POST")
            .path("/a2a")
            .header("Content-Type", "application/json")
            .body(body)
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 200);
        let rpc: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse json");
        assert_eq!(rpc["jsonrpc"], "2.0");
        assert_eq!(rpc["id"], "test-1");

        // Result should be a StreamResponse: {"task": {...}}
        let result = &rpc["result"];
        assert!(
            result.get("task").is_some(),
            "expected StreamResponse with 'task' key, got: {result}"
        );
        let task = &result["task"];
        assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
        assert!(
            task["status"]["message"]["parts"][0]["text"]
                .as_str()
                .expect("text")
                .starts_with("Echo: hello")
        );
    }

    #[tokio::test]
    async fn jsonrpc_get_task_not_found() {
        let (filter, _dir) = build_jsonrpc_filter();

        let body = jsonrpc_body("GetTask", serde_json::json!({"id": "nonexistent"}));

        let resp = warp::test::request()
            .method("POST")
            .path("/a2a")
            .header("Content-Type", "application/json")
            .body(body)
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 200);
        let rpc: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse json");
        assert!(rpc.get("error").is_some());
        assert_eq!(rpc["error"]["code"], -32001);
    }

    #[tokio::test]
    async fn jsonrpc_send_then_get_task() {
        let executor = Arc::new(MockExecutor);
        let task_store = Arc::new(MockTaskStore::new());
        let registry = setup_registry();
        let push_store = Arc::new(MockPushStore);

        let filter = jsonrpc_filter(executor, task_store, registry, push_store);

        // 1. Send a message to create a task.
        let send_body = jsonrpc_body(
            "SendMessage",
            serde_json::json!({
                "message": {
                    "role": "ROLE_USER",
                    "messageId": "msg-1",
                    "parts": [{"text": "test"}]
                }
            }),
        );

        let resp = warp::test::request()
            .method("POST")
            .path("/a2a")
            .header("Content-Type", "application/json")
            .body(send_body)
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 200);
        let rpc: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse json");
        let task_id = rpc["result"]["task"]["id"]
            .as_str()
            .expect("task id")
            .to_string();

        // 2. Get the task back.
        let get_body = jsonrpc_body("GetTask", serde_json::json!({"id": task_id}));

        let resp = warp::test::request()
            .method("POST")
            .path("/a2a")
            .header("Content-Type", "application/json")
            .body(get_body)
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 200);
        let rpc: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse json");
        assert!(rpc.get("error").is_none(), "unexpected error: {rpc}");
        assert_eq!(rpc["result"]["id"], task_id);
        assert_eq!(rpc["result"]["status"]["state"], "TASK_STATE_COMPLETED");
    }

    #[tokio::test]
    async fn jsonrpc_cancel_task_not_found() {
        let (filter, _dir) = build_jsonrpc_filter();

        let body = jsonrpc_body("CancelTask", serde_json::json!({"id": "nonexistent"}));

        let resp = warp::test::request()
            .method("POST")
            .path("/a2a")
            .header("Content-Type", "application/json")
            .body(body)
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 200);
        let rpc: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse json");
        assert_eq!(rpc["error"]["code"], -32001);
    }

    #[tokio::test]
    async fn jsonrpc_send_then_cancel_task() {
        let registry = setup_registry();

        let executor = Arc::new(MockExecutor);
        let task_store = Arc::new(MockTaskStore::new());
        let push_store = Arc::new(MockPushStore);

        let filter = jsonrpc_filter(executor, task_store, registry, push_store);

        // 1. Create a task via SendMessage.
        let send_body = jsonrpc_body(
            "SendMessage",
            serde_json::json!({
                "message": {
                    "role": "ROLE_USER",
                    "messageId": "msg-1",
                    "parts": [{"text": "test"}]
                }
            }),
        );

        let resp = warp::test::request()
            .method("POST")
            .path("/a2a")
            .header("Content-Type", "application/json")
            .body(send_body)
            .reply(&filter)
            .await;

        let rpc: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse json");
        let task_id = rpc["result"]["task"]["id"]
            .as_str()
            .expect("task id")
            .to_string();

        // 2. Cancel that task.
        let cancel_body = jsonrpc_body("CancelTask", serde_json::json!({"id": task_id}));

        let resp = warp::test::request()
            .method("POST")
            .path("/a2a")
            .header("Content-Type", "application/json")
            .body(cancel_body)
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 200);
        let rpc: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse json");
        assert!(rpc.get("error").is_none(), "unexpected error: {rpc}");
        assert_eq!(rpc["result"]["id"], task_id);
        assert_eq!(rpc["result"]["status"]["state"], "TASK_STATE_CANCELED");
    }

    #[tokio::test]
    async fn jsonrpc_list_tasks_empty() {
        let (filter, _dir) = build_jsonrpc_filter();

        let body = jsonrpc_body("ListTasks", serde_json::json!({}));

        let resp = warp::test::request()
            .method("POST")
            .path("/a2a")
            .header("Content-Type", "application/json")
            .body(body)
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 200);
        let rpc: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse json");
        assert!(rpc.get("error").is_none());
        assert_eq!(rpc["result"]["tasks"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn jsonrpc_get_extended_agent_card() {
        let (filter, _dir) = build_jsonrpc_filter();

        let body = jsonrpc_body("GetExtendedAgentCard", serde_json::json!({}));

        let resp = warp::test::request()
            .method("POST")
            .path("/a2a")
            .header("Content-Type", "application/json")
            .body(body)
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 200);
        let rpc: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse json");
        assert!(
            rpc.get("error").is_none(),
            "unexpected error: {}",
            serde_json::to_string_pretty(&rpc).unwrap_or_default()
        );
        assert_eq!(rpc["result"]["name"], "test-agent");
    }

    #[tokio::test]
    async fn jsonrpc_method_not_found() {
        let (filter, _dir) = build_jsonrpc_filter();

        let body = jsonrpc_body("UnknownMethod", serde_json::json!({}));

        let resp = warp::test::request()
            .method("POST")
            .path("/a2a")
            .header("Content-Type", "application/json")
            .body(body)
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 200);
        let rpc: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse json");
        assert_eq!(rpc["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn jsonrpc_streaming_send_message() {
        let (filter, _dir) = build_jsonrpc_filter();

        let body = jsonrpc_body(
            "SendStreamingMessage",
            serde_json::json!({
                "message": {
                    "role": "ROLE_USER",
                    "messageId": "msg-stream-1",
                    "parts": [{"text": "stream test"}]
                }
            }),
        );

        let resp = warp::test::request()
            .method("POST")
            .path("/a2a")
            .header("Content-Type", "application/json")
            .body(body)
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 200);
        // SSE response should contain JSON-RPC envelope with result.
        let body = String::from_utf8_lossy(resp.body());
        assert!(
            body.contains("data:"),
            "expected SSE data: line, got: {body}"
        );
        // Extract the data line.
        for line in body.lines() {
            if let Some(data) = line.strip_prefix("data:") {
                let rpc: serde_json::Value =
                    serde_json::from_str(data.trim()).expect("parse SSE data as JSON-RPC");
                assert_eq!(rpc["jsonrpc"], "2.0");
                assert_eq!(rpc["id"], "test-1");
                // Result should be a StreamResponse (task wrapping).
                assert!(rpc["result"].get("task").is_some());
            }
        }
    }
}
