//! Warp HTTP filters for A2A agent discovery, JSON-RPC dispatch, streaming,
//! and REST-style bindings.
//!
//! Provides the standard `/.well-known/agent-card.json` discovery endpoint,
//! a `/a2a/agents` listing endpoint, `POST /a2a` JSON-RPC endpoint,
//! SSE streaming for `SendStreamingMessage` / `SubscribeToTask`, and
//! REST-style HTTP bindings per the A2A v1.0 specification.

use std::convert::Infallible;
use std::sync::Arc;

use tokio_stream::StreamExt;
use warp::Filter;

use bitrouter_a2a::jsonrpc::JsonRpcRequest;
use bitrouter_a2a::registry::AgentCardRegistry;
use bitrouter_a2a::request::{
    SendMessageRequest, SubscribeToTaskRequest, TaskPushNotificationConfig,
};
use bitrouter_a2a::server::{AgentExecutor, ExecutorContext, PushNotificationStore, TaskStore};
use bitrouter_a2a::stream::StreamResponse;

use super::handler;

/// Creates a warp filter for `GET /.well-known/agent-card.json`.
///
/// Returns the first registered agent card (alphabetically), or a specific
/// agent when `?name=<agent_name>` is provided. Includes `Cache-Control`
/// and `ETag` headers per the A2A v1.0 discovery specification.
pub fn well_known_filter<R>(
    registry: Arc<R>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    R: AgentCardRegistry + 'static,
{
    warp::path!(".well-known" / "agent-card.json")
        .and(warp::get())
        .and(warp::query::<WellKnownQuery>())
        .and(warp::any().map(move || registry.clone()))
        .map(handle_well_known)
}

/// Creates a warp filter for `GET /a2a/agents`.
///
/// Returns a JSON array of all registered agent cards. The `iss` binding
/// is stripped from the public response.
pub fn agent_list_filter<R>(
    registry: Arc<R>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    R: AgentCardRegistry + 'static,
{
    warp::path!("a2a" / "agents")
        .and(warp::get())
        .and(warp::any().map(move || registry.clone()))
        .map(handle_agent_list)
}

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
                        handle_streaming_request(request, executor, task_store).await
                    }
                    _ => {
                        let response = handler::handle_jsonrpc(
                            request, executor, task_store, registry, push_store,
                        )
                        .await;
                        Box::new(warp::reply::json(&response)) as Box<dyn warp::Reply>
                    }
                }
            },
        )
}

/// Creates a warp filter for SSE streaming `POST /a2a` for
/// `SendStreamingMessage` and `SubscribeToTask`.
pub fn streaming_jsonrpc_filter<E, S>(
    executor: Arc<E>,
    task_store: Arc<S>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    E: AgentExecutor + 'static,
    S: TaskStore + 'static,
{
    warp::path("a2a")
        .and(warp::path("stream"))
        .and(warp::post())
        .and(warp::body::json::<JsonRpcRequest>())
        .and(warp::any().map(move || executor.clone()))
        .and(warp::any().map(move || task_store.clone()))
        .then(
            |request: JsonRpcRequest, executor: Arc<E>, task_store: Arc<S>| async move {
                handle_streaming_request(request, executor, task_store).await
            },
        )
}

async fn handle_streaming_request<E, S>(
    request: JsonRpcRequest,
    executor: Arc<E>,
    task_store: Arc<S>,
) -> Box<dyn warp::Reply>
where
    E: AgentExecutor + 'static,
    S: TaskStore + 'static,
{
    match request.method.as_str() {
        "SendStreamingMessage" => {
            handle_send_streaming_message(&request, executor, task_store).await
        }
        "SubscribeToTask" => handle_subscribe_to_task(&request, executor).await,
        _ => {
            let error = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request.id,
                "error": {"code": -32601, "message": format!("method not found: {}", request.method)}
            });
            Box::new(warp::reply::with_status(
                warp::reply::json(&error),
                warp::http::StatusCode::BAD_REQUEST,
            ))
        }
    }
}

async fn handle_send_streaming_message<E, S>(
    request: &JsonRpcRequest,
    executor: Arc<E>,
    task_store: Arc<S>,
) -> Box<dyn warp::Reply>
where
    E: AgentExecutor + 'static,
    S: TaskStore + 'static,
{
    let send_req: SendMessageRequest = match serde_json::from_value(request.params.clone()) {
        Ok(r) => r,
        Err(e) => {
            let error = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request.id,
                "error": {"code": -32602, "message": format!("invalid params: {e}")}
            });
            return Box::new(warp::reply::with_status(
                warp::reply::json(&error),
                warp::http::StatusCode::BAD_REQUEST,
            ));
        }
    };

    let task_id = send_req
        .message
        .task_id
        .clone()
        .unwrap_or_else(|| generate_streaming_id("task"));
    let context_id = send_req
        .message
        .context_id
        .clone()
        .unwrap_or_else(|| generate_streaming_id("ctx"));

    let ctx = ExecutorContext {
        message: send_req.message,
        task_id,
        context_id,
        configuration: send_req.configuration,
    };

    let request_id = request.id.clone();
    match executor.execute_streaming(&ctx).await {
        Ok(stream) => {
            let task_store = task_store.clone();
            let event_stream = stream.map(move |item| {
                // If the item is a completed task, store it best-effort.
                if let StreamResponse::Task(ref task) = item {
                    let _ = task_store.create(task);
                }
                stream_response_to_sse(&request_id, &item)
            });
            Box::new(warp::sse::reply(event_stream))
        }
        Err(e) => {
            let error = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request.id,
                "error": {"code": -32000, "message": e.to_string()}
            });
            Box::new(warp::reply::with_status(
                warp::reply::json(&error),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            ))
        }
    }
}

async fn handle_subscribe_to_task<E>(
    request: &JsonRpcRequest,
    executor: Arc<E>,
) -> Box<dyn warp::Reply>
where
    E: AgentExecutor + 'static,
{
    let req: SubscribeToTaskRequest = match serde_json::from_value(request.params.clone()) {
        Ok(r) => r,
        Err(e) => {
            let error = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request.id,
                "error": {"code": -32602, "message": format!("invalid params: {e}")}
            });
            return Box::new(warp::reply::with_status(
                warp::reply::json(&error),
                warp::http::StatusCode::BAD_REQUEST,
            ));
        }
    };

    let request_id = request.id.clone();
    match executor.subscribe(&req.task_id).await {
        Ok(stream) => {
            let event_stream = stream.map(move |item| stream_response_to_sse(&request_id, &item));
            Box::new(warp::sse::reply(event_stream))
        }
        Err(e) => {
            let error = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request.id,
                "error": {"code": -32000, "message": e.to_string()}
            });
            Box::new(warp::reply::with_status(
                warp::reply::json(&error),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            ))
        }
    }
}

fn stream_response_to_sse(
    request_id: &str,
    item: &StreamResponse,
) -> Result<warp::sse::Event, Infallible> {
    // Wrap each streaming item in a JSON-RPC 2.0 response envelope,
    // matching the A2A v1.0 wire format used by the reference Go impl.
    let result = serde_json::to_value(item).unwrap_or_default();
    let envelope = serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": result
    });
    let data = serde_json::to_string(&envelope).unwrap_or_default();
    Ok(warp::sse::Event::default().data(data))
}

fn generate_streaming_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("{prefix}-s-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Creates REST-style HTTP filters for A2A v1.0 bindings.
///
/// Provides routes like `POST /message:send`, `GET /tasks/{id}`,
/// `POST /tasks/{id}:cancel`, and push notification config CRUD.
pub fn rest_filters<E, S, P>(
    executor: Arc<E>,
    task_store: Arc<S>,
    push_store: Arc<P>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    E: AgentExecutor + 'static,
    S: TaskStore + 'static,
    P: PushNotificationStore + 'static,
{
    let send = rest_send_filter(executor.clone(), task_store.clone());
    let get_task = rest_get_task_filter(task_store.clone());
    let cancel = rest_cancel_filter(executor, task_store);
    let push_create = rest_push_create_filter(push_store.clone());
    let push_get = rest_push_get_filter(push_store.clone());
    let push_list = rest_push_list_filter(push_store.clone());
    let push_delete = rest_push_delete_filter(push_store);

    send.or(get_task)
        .or(cancel)
        .or(push_create)
        .or(push_get)
        .or(push_list)
        .or(push_delete)
}

fn rest_send_filter<E, S>(
    executor: Arc<E>,
    task_store: Arc<S>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    E: AgentExecutor + 'static,
    S: TaskStore + 'static,
{
    warp::path!("message:send")
        .and(warp::post())
        .and(warp::body::json::<SendMessageRequest>())
        .and(warp::any().map(move || executor.clone()))
        .and(warp::any().map(move || task_store.clone()))
        .then(
            |body: SendMessageRequest, executor: Arc<E>, task_store: Arc<S>| async move {
                handler::handle_rest_send_message(body, executor, task_store).await
            },
        )
}

fn rest_get_task_filter<S>(
    task_store: Arc<S>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    S: TaskStore + 'static,
{
    warp::path!("tasks" / String)
        .and(warp::get())
        .and(warp::any().map(move || task_store.clone()))
        .map(|task_id: String, task_store: Arc<S>| {
            handler::handle_rest_get_task(task_id, task_store)
        })
}

fn rest_cancel_filter<E, S>(
    executor: Arc<E>,
    task_store: Arc<S>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    E: AgentExecutor + 'static,
    S: TaskStore + 'static,
{
    // Match "tasks/{id}:cancel" — warp treats the colon as part of the path segment
    warp::path("tasks")
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::any().map(move || executor.clone()))
        .and(warp::any().map(move || task_store.clone()))
        .and_then(
            |task_id_action: String, executor: Arc<E>, task_store: Arc<S>| async move {
                if let Some(task_id) = task_id_action.strip_suffix(":cancel") {
                    Ok(
                        handler::handle_rest_cancel_task(task_id.to_string(), executor, task_store)
                            .await,
                    )
                } else {
                    Err(warp::reject::not_found())
                }
            },
        )
}

fn rest_push_create_filter<P>(
    push_store: Arc<P>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    P: PushNotificationStore + 'static,
{
    warp::path!("tasks" / String / "push-notification-configs")
        .and(warp::post())
        .and(warp::body::json::<TaskPushNotificationConfig>())
        .and(warp::any().map(move || push_store.clone()))
        .map(
            |task_id: String, config: TaskPushNotificationConfig, push_store: Arc<P>| {
                handler::handle_rest_create_push_config(task_id, config, push_store)
            },
        )
}

fn rest_push_get_filter<P>(
    push_store: Arc<P>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    P: PushNotificationStore + 'static,
{
    warp::path!("tasks" / String / "push-notification-configs" / String)
        .and(warp::get())
        .and(warp::any().map(move || push_store.clone()))
        .map(|task_id: String, config_id: String, push_store: Arc<P>| {
            handler::handle_rest_get_push_config(task_id, config_id, push_store)
        })
}

fn rest_push_list_filter<P>(
    push_store: Arc<P>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    P: PushNotificationStore + 'static,
{
    warp::path!("tasks" / String / "push-notification-configs")
        .and(warp::get())
        .and(warp::any().map(move || push_store.clone()))
        .map(|task_id: String, push_store: Arc<P>| {
            handler::handle_rest_list_push_configs(task_id, push_store)
        })
}

fn rest_push_delete_filter<P>(
    push_store: Arc<P>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    P: PushNotificationStore + 'static,
{
    warp::path!("tasks" / String / "push-notification-configs" / String)
        .and(warp::delete())
        .and(warp::any().map(move || push_store.clone()))
        .map(|task_id: String, config_id: String, push_store: Arc<P>| {
            handler::handle_rest_delete_push_config(task_id, config_id, push_store)
        })
}

#[derive(Debug, serde::Deserialize)]
struct WellKnownQuery {
    name: Option<String>,
}

fn handle_well_known<R: AgentCardRegistry>(
    query: WellKnownQuery,
    registry: Arc<R>,
) -> Box<dyn warp::Reply> {
    let result = if let Some(name) = &query.name {
        registry.get(name)
    } else {
        // Return the first agent alphabetically.
        registry.list().map(|mut list| list.pop())
    };

    match result {
        Ok(Some(reg)) => {
            let etag = format!("\"{}\"", reg.card.version);
            let json = warp::reply::json(&reg.card);
            let reply = warp::reply::with_header(json, "Cache-Control", "max-age=3600");
            let reply = warp::reply::with_header(reply, "ETag", etag);
            Box::new(reply)
        }
        Ok(None) => {
            let json = warp::reply::json(&serde_json::json!({
                "error": "no agent cards registered"
            }));
            Box::new(warp::reply::with_status(
                json,
                warp::http::StatusCode::NOT_FOUND,
            ))
        }
        Err(e) => {
            let json = warp::reply::json(&serde_json::json!({
                "error": e.to_string()
            }));
            Box::new(warp::reply::with_status(
                json,
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            ))
        }
    }
}

fn handle_agent_list<R: AgentCardRegistry>(registry: Arc<R>) -> Box<dyn warp::Reply> {
    match registry.list() {
        Ok(registrations) => {
            // Strip iss from public response — only expose the cards.
            let cards: Vec<_> = registrations.into_iter().map(|r| r.card).collect();
            Box::new(warp::reply::json(&serde_json::json!({ "agents": cards })))
        }
        Err(e) => {
            let json = warp::reply::json(&serde_json::json!({
                "error": e.to_string()
            }));
            Box::new(warp::reply::with_status(
                json,
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::RwLock;

    use bitrouter_a2a::card::minimal_card;
    use bitrouter_a2a::error::A2aError;
    use bitrouter_a2a::file_registry::FileAgentCardRegistry;
    use bitrouter_a2a::message::{Message, MessageRole, Part};
    use bitrouter_a2a::registry::AgentRegistration;
    use bitrouter_a2a::server::{ExecuteResult, ExecutorContext, StoredTask};
    use bitrouter_a2a::task::{Task, TaskState, TaskStatus};

    fn setup_registry(dir: &std::path::Path) -> Arc<FileAgentCardRegistry> {
        Arc::new(FileAgentCardRegistry::new(dir).expect("new registry"))
    }

    // ── Mock implementations ───────────────────────────────────

    struct MockExecutor;

    impl AgentExecutor for MockExecutor {
        async fn execute(&self, ctx: &ExecutorContext) -> Result<ExecuteResult, A2aError> {
            // Echo back the input text as the agent response.
            let input_text = ctx
                .message
                .parts
                .iter()
                .filter_map(|p| p.text.as_deref())
                .collect::<Vec<_>>()
                .join(" ");

            let response_msg = Message {
                role: MessageRole::Agent,
                parts: vec![Part::text(&format!("Echo: {input_text}"))],
                message_id: format!("{}-resp", ctx.task_id),
                context_id: Some(ctx.context_id.clone()),
                task_id: Some(ctx.task_id.clone()),
                reference_task_ids: Vec::new(),
                metadata: None,
                extensions: Vec::new(),
            };

            let task = Task {
                id: ctx.task_id.clone(),
                context_id: Some(ctx.context_id.clone()),
                status: TaskStatus {
                    state: TaskState::Completed,
                    timestamp: "2026-03-19T00:00:00Z".to_string(),
                    message: Some(response_msg),
                },
                artifacts: Vec::new(),
                history: Vec::new(),
                metadata: None,
            };
            Ok(ExecuteResult::Task(task))
        }

        async fn cancel(&self, task_id: &str) -> Result<Task, A2aError> {
            Ok(Task {
                id: task_id.to_string(),
                context_id: None,
                status: TaskStatus {
                    state: TaskState::Canceled,
                    timestamp: "2026-03-19T00:00:00Z".to_string(),
                    message: None,
                },
                artifacts: Vec::new(),
                history: Vec::new(),
                metadata: None,
            })
        }
    }

    struct MockTaskStore {
        tasks: RwLock<HashMap<String, StoredTask>>,
    }

    impl MockTaskStore {
        fn new() -> Self {
            Self {
                tasks: RwLock::new(HashMap::new()),
            }
        }
    }

    impl TaskStore for MockTaskStore {
        fn create(&self, task: &Task) -> Result<u64, A2aError> {
            let mut tasks = self
                .tasks
                .write()
                .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;
            tasks.insert(
                task.id.clone(),
                StoredTask {
                    task: task.clone(),
                    version: 1,
                },
            );
            Ok(1)
        }

        fn update(&self, task: &Task, _prev_version: u64) -> Result<u64, A2aError> {
            let mut tasks = self
                .tasks
                .write()
                .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;
            let version = tasks.get(&task.id).map_or(1, |s| s.version + 1);
            tasks.insert(
                task.id.clone(),
                StoredTask {
                    task: task.clone(),
                    version,
                },
            );
            Ok(version)
        }

        fn get(&self, task_id: &str) -> Result<Option<StoredTask>, A2aError> {
            let tasks = self
                .tasks
                .read()
                .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;
            Ok(tasks.get(task_id).cloned())
        }

        fn list(
            &self,
            _query: &bitrouter_a2a::server::TaskQuery,
        ) -> Result<(Vec<StoredTask>, Option<String>), A2aError> {
            let tasks = self
                .tasks
                .read()
                .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;
            let all: Vec<StoredTask> = tasks.values().cloned().collect();
            Ok((all, None))
        }
    }

    struct MockPushStore;

    impl PushNotificationStore for MockPushStore {
        fn create_config(
            &self,
            config: &TaskPushNotificationConfig,
        ) -> Result<TaskPushNotificationConfig, A2aError> {
            let mut result = config.clone();
            if result.id.is_none() {
                result.id = Some("cfg-1".to_string());
            }
            Ok(result)
        }

        fn get_config(
            &self,
            _task_id: &str,
            _id: &str,
        ) -> Result<Option<TaskPushNotificationConfig>, A2aError> {
            Ok(None)
        }

        fn list_configs(
            &self,
            _task_id: &str,
        ) -> Result<Vec<TaskPushNotificationConfig>, A2aError> {
            Ok(Vec::new())
        }

        fn delete_config(&self, _task_id: &str, _id: &str) -> Result<(), A2aError> {
            Ok(())
        }
    }

    fn build_jsonrpc_filter() -> (
        impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = setup_registry(dir.path());
        registry
            .register(AgentRegistration {
                card: minimal_card(
                    "test-agent",
                    "A test agent",
                    "1.0.0",
                    "http://localhost/a2a",
                ),
                iss: None,
            })
            .expect("register");

        let filter = jsonrpc_filter(
            Arc::new(MockExecutor),
            Arc::new(MockTaskStore::new()),
            registry,
            Arc::new(MockPushStore),
        );
        (filter, dir)
    }

    fn jsonrpc_body(method: &str, params: serde_json::Value) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": "test-1",
            "method": method,
            "params": params
        })
        .to_string()
    }

    // ── Discovery tests ─────────────────────────────────────────

    #[tokio::test]
    async fn well_known_returns_404_when_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = setup_registry(dir.path());
        let filter = well_known_filter(registry);

        let resp = warp::test::request()
            .path("/.well-known/agent-card.json")
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn well_known_returns_card() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = setup_registry(dir.path());

        registry
            .register(AgentRegistration {
                card: minimal_card("test-agent", "A test", "1.0.0", "http://localhost:8787"),
                iss: Some("solana:test:key".to_string()),
            })
            .expect("register");

        let filter = well_known_filter(registry);
        let resp = warp::test::request()
            .path("/.well-known/agent-card.json")
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get("Cache-Control").expect("cache-control"),
            "max-age=3600"
        );
        assert_eq!(resp.headers().get("ETag").expect("etag"), "\"1.0.0\"");

        // Verify iss is NOT in the response (card only, not registration).
        let body = String::from_utf8_lossy(resp.body());
        assert!(!body.contains("solana:test:key"));
        assert!(body.contains("test-agent"));
    }

    #[tokio::test]
    async fn well_known_with_name_query() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = setup_registry(dir.path());

        registry
            .register(AgentRegistration {
                card: minimal_card("alpha", "Agent A", "1.0.0", "http://localhost:8787"),
                iss: None,
            })
            .expect("register");
        registry
            .register(AgentRegistration {
                card: minimal_card("beta", "Agent B", "2.0.0", "http://localhost:8787"),
                iss: None,
            })
            .expect("register");

        let filter = well_known_filter(registry);

        let resp = warp::test::request()
            .path("/.well-known/agent-card.json?name=beta")
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 200);
        let body = String::from_utf8_lossy(resp.body());
        assert!(body.contains("beta"));
        assert!(body.contains("Agent B"));
    }

    #[tokio::test]
    async fn agent_list_returns_all_cards() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = setup_registry(dir.path());

        registry
            .register(AgentRegistration {
                card: minimal_card("alpha", "Agent A", "1.0.0", "http://localhost:8787"),
                iss: Some("secret-iss".to_string()),
            })
            .expect("register");
        registry
            .register(AgentRegistration {
                card: minimal_card("beta", "Agent B", "1.0.0", "http://localhost:8787"),
                iss: None,
            })
            .expect("register");

        let filter = agent_list_filter(registry);
        let resp = warp::test::request()
            .path("/a2a/agents")
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 200);
        let body = String::from_utf8_lossy(resp.body());
        assert!(body.contains("alpha"));
        assert!(body.contains("beta"));
        // iss should be stripped.
        assert!(!body.contains("secret-iss"));
    }

    // ── JSON-RPC dispatch tests ─────────────────────────────────

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
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = setup_registry(dir.path());
        registry
            .register(AgentRegistration {
                card: minimal_card("test", "test", "1.0.0", "http://localhost"),
                iss: None,
            })
            .expect("register");
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
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = setup_registry(dir.path());
        registry
            .register(AgentRegistration {
                card: minimal_card("test", "test", "1.0.0", "http://localhost"),
                iss: None,
            })
            .expect("register");

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

    // ── REST endpoint tests ─────────────────────────────────────

    #[tokio::test]
    async fn rest_send_message() {
        let executor = Arc::new(MockExecutor);
        let task_store = Arc::new(MockTaskStore::new());
        let push_store = Arc::new(MockPushStore);

        let filter = rest_filters(executor, task_store, push_store);

        let body = serde_json::json!({
            "message": {
                "role": "ROLE_USER",
                "messageId": "msg-rest-1",
                "parts": [{"text": "rest hello"}]
            }
        })
        .to_string();

        let resp = warp::test::request()
            .method("POST")
            .path("/message:send")
            .header("Content-Type", "application/json")
            .body(body)
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 200);
        let result: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse json");
        assert_eq!(result["status"]["state"], "TASK_STATE_COMPLETED");
    }

    #[tokio::test]
    async fn rest_get_task_not_found() {
        let executor = Arc::new(MockExecutor);
        let task_store = Arc::new(MockTaskStore::new());
        let push_store = Arc::new(MockPushStore);

        let filter = rest_filters(executor, task_store, push_store);

        let resp = warp::test::request()
            .method("GET")
            .path("/tasks/nonexistent")
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 404);
    }
}
