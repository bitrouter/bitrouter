//! JSON-RPC 2.0 dispatch handler for A2A server methods.
//!
//! Handles all A2A v1.0 methods: `SendMessage`, `GetTask`, `CancelTask`,
//! `ListTasks`, `GetExtendedAgentCard`, `SendStreamingMessage`,
//! `SubscribeToTask`, and push notification config CRUD.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bitrouter_a2a::error::A2aError;
use bitrouter_a2a::jsonrpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use bitrouter_a2a::registry::AgentCardRegistry;
use bitrouter_a2a::request::{
    CancelTaskRequest, DeleteTaskPushNotificationConfigRequest,
    GetTaskPushNotificationConfigRequest, ListTaskPushNotificationConfigsRequest,
    ListTaskPushNotificationConfigsResponse, SendMessageRequest, TaskPushNotificationConfig,
};
use bitrouter_a2a::server::{
    AgentExecutor, ExecuteResult, ExecutorContext, PushNotificationStore, TaskQuery, TaskStore,
};
use bitrouter_a2a::stream::StreamResponse;
use bitrouter_a2a::task::{GetTaskRequest, ListTasksRequest, ListTasksResponse, Task};

/// Dispatch a JSON-RPC request to the appropriate A2A method handler.
pub async fn handle_jsonrpc<E, S, R, P>(
    request: JsonRpcRequest,
    executor: Arc<E>,
    task_store: Arc<S>,
    registry: Arc<R>,
    push_store: Arc<P>,
) -> JsonRpcResponse
where
    E: AgentExecutor,
    S: TaskStore,
    R: AgentCardRegistry,
    P: PushNotificationStore,
{
    match request.method.as_str() {
        "SendMessage" => handle_send_message(&request, executor, task_store).await,
        "GetTask" => handle_get_task(&request, task_store).await,
        "CancelTask" => handle_cancel_task(&request, executor, task_store).await,
        "ListTasks" => handle_list_tasks(&request, task_store).await,
        "GetExtendedAgentCard" => handle_get_extended_agent_card(&request, registry),
        "CreateTaskPushNotificationConfig" => {
            handle_create_push_notification_config(&request, push_store)
        }
        "GetTaskPushNotificationConfig" => {
            handle_get_push_notification_config(&request, push_store)
        }
        "ListTaskPushNotificationConfigs" => {
            handle_list_push_notification_configs(&request, push_store)
        }
        "DeleteTaskPushNotificationConfig" => {
            handle_delete_push_notification_config(&request, push_store)
        }
        _ => error_response(
            &request.id,
            -32601,
            format!("method not found: {}", request.method),
        ),
    }
}

async fn handle_send_message<E, S>(
    request: &JsonRpcRequest,
    executor: Arc<E>,
    task_store: Arc<S>,
) -> JsonRpcResponse
where
    E: AgentExecutor,
    S: TaskStore,
{
    // Deserialize typed request.
    let send_req: SendMessageRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };

    // Derive task and context IDs.
    let task_id = send_req
        .message
        .task_id
        .clone()
        .unwrap_or_else(|| generate_id("task"));
    let context_id = send_req
        .message
        .context_id
        .clone()
        .unwrap_or_else(|| generate_id("ctx"));

    let ctx = ExecutorContext {
        message: send_req.message,
        task_id,
        context_id,
        configuration: send_req.configuration,
    };

    // Execute the request.
    let result = match executor.execute(&ctx).await {
        Ok(r) => r,
        Err(e) => return execution_error_response(&request.id, &e),
    };

    match result {
        ExecuteResult::Task(task) => {
            // Store the completed task (best-effort).
            let _ = task_store.create(&task);
            // Wrap in StreamResponse to match A2A v1.0 wire format.
            success_response(&request.id, &StreamResponse::Task(task))
        }
        ExecuteResult::Message(msg) => success_response(&request.id, &StreamResponse::Message(msg)),
    }
}

async fn handle_get_task<S>(request: &JsonRpcRequest, task_store: Arc<S>) -> JsonRpcResponse
where
    S: TaskStore,
{
    let req: GetTaskRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };

    match task_store.get(&req.id) {
        Ok(Some(stored)) => {
            let mut task = stored.task;
            // Apply history_length truncation.
            if let Some(len) = req.history_length {
                let len = len as usize;
                if task.history.len() > len {
                    let start = task.history.len() - len;
                    task.history = task.history[start..].to_vec();
                }
            }
            success_response(&request.id, &task)
        }
        Ok(None) => error_response(&request.id, -32001, format!("task not found: {}", req.id)),
        Err(e) => error_response(&request.id, -32603, e.to_string()),
    }
}

async fn handle_cancel_task<E, S>(
    request: &JsonRpcRequest,
    executor: Arc<E>,
    task_store: Arc<S>,
) -> JsonRpcResponse
where
    E: AgentExecutor,
    S: TaskStore,
{
    let req: CancelTaskRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };

    // Check task exists.
    match task_store.get(&req.id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return error_response(&request.id, -32001, format!("task not found: {}", req.id));
        }
        Err(e) => return error_response(&request.id, -32603, e.to_string()),
    }

    match executor.cancel(&req.id).await {
        Ok(task) => {
            // Update the task in store (best-effort).
            let _ = task_store.create(&task);
            success_response(&request.id, &task)
        }
        Err(e) => execution_error_response(&request.id, &e),
    }
}

async fn handle_list_tasks<S>(request: &JsonRpcRequest, task_store: Arc<S>) -> JsonRpcResponse
where
    S: TaskStore,
{
    let req: ListTasksRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };

    let query = TaskQuery {
        context_id: req.context_id,
        status: req.status,
        status_timestamp_after: req.status_timestamp_after,
        page_size: req.page_size,
        page_token: req.page_token,
    };

    match task_store.list(&query) {
        Ok((stored_tasks, next_page_token)) => {
            let total_size = stored_tasks.len() as u32;
            let mut tasks: Vec<Task> = stored_tasks.into_iter().map(|s| s.task).collect();

            // Apply history_length truncation.
            if let Some(len) = req.history_length {
                let len = len as usize;
                for task in &mut tasks {
                    if task.history.len() > len {
                        let start = task.history.len() - len;
                        task.history = task.history[start..].to_vec();
                    }
                }
            }

            // Apply include_artifacts filtering.
            if req.include_artifacts == Some(false) {
                for task in &mut tasks {
                    task.artifacts.clear();
                }
            }

            let response = ListTasksResponse {
                page_size: tasks.len() as u32,
                total_size,
                tasks,
                next_page_token,
            };

            success_response(&request.id, &response)
        }
        Err(e) => error_response(&request.id, -32603, e.to_string()),
    }
}

fn handle_get_extended_agent_card<R>(request: &JsonRpcRequest, registry: Arc<R>) -> JsonRpcResponse
where
    R: AgentCardRegistry,
{
    // For now, return the first registered agent's card (same as public).
    match registry.get_extended_by_first(registry.as_ref()) {
        Ok(Some(reg)) => success_response(&request.id, &reg.card),
        Ok(None) => error_response(&request.id, -32001, "no agent registered".to_string()),
        Err(e) => error_response(&request.id, -32603, e.to_string()),
    }
}

fn handle_create_push_notification_config<P>(
    request: &JsonRpcRequest,
    push_store: Arc<P>,
) -> JsonRpcResponse
where
    P: PushNotificationStore,
{
    let config: TaskPushNotificationConfig = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };

    match push_store.create_config(&config) {
        Ok(stored) => success_response(&request.id, &stored),
        Err(e) => error_response(&request.id, -32603, e.to_string()),
    }
}

fn handle_get_push_notification_config<P>(
    request: &JsonRpcRequest,
    push_store: Arc<P>,
) -> JsonRpcResponse
where
    P: PushNotificationStore,
{
    let req: GetTaskPushNotificationConfigRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };

    match push_store.get_config(&req.task_id, &req.id) {
        Ok(Some(config)) => success_response(&request.id, &config),
        Ok(None) => error_response(
            &request.id,
            -32001,
            format!("push config not found: task={} id={}", req.task_id, req.id),
        ),
        Err(e) => error_response(&request.id, -32603, e.to_string()),
    }
}

fn handle_list_push_notification_configs<P>(
    request: &JsonRpcRequest,
    push_store: Arc<P>,
) -> JsonRpcResponse
where
    P: PushNotificationStore,
{
    let req: ListTaskPushNotificationConfigsRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };

    match push_store.list_configs(&req.task_id) {
        Ok(configs) => {
            let response = ListTaskPushNotificationConfigsResponse { configs };
            success_response(&request.id, &response)
        }
        Err(e) => error_response(&request.id, -32603, e.to_string()),
    }
}

fn handle_delete_push_notification_config<P>(
    request: &JsonRpcRequest,
    push_store: Arc<P>,
) -> JsonRpcResponse
where
    P: PushNotificationStore,
{
    let req: DeleteTaskPushNotificationConfigRequest = match deserialize_params(&request.params) {
        Ok(r) => r,
        Err(resp) => return (*resp).with_id(&request.id),
    };

    match push_store.delete_config(&req.task_id, &req.id) {
        Ok(()) => success_response(&request.id, &serde_json::json!({"success": true})),
        Err(e) => {
            let code = match &e {
                A2aError::PushNotificationNotFound { .. } => -32001,
                _ => -32603,
            };
            error_response(&request.id, code, e.to_string())
        }
    }
}

// ── REST handler wrappers ───────────────────────────────────────

/// Handle REST `POST /message:send` — wraps `SendMessage`.
pub async fn handle_rest_send_message<E, S>(
    body: SendMessageRequest,
    executor: Arc<E>,
    task_store: Arc<S>,
) -> Box<dyn warp::Reply>
where
    E: AgentExecutor,
    S: TaskStore,
{
    let task_id = body
        .message
        .task_id
        .clone()
        .unwrap_or_else(|| generate_id("task"));
    let context_id = body
        .message
        .context_id
        .clone()
        .unwrap_or_else(|| generate_id("ctx"));

    let ctx = ExecutorContext {
        message: body.message,
        task_id,
        context_id,
        configuration: body.configuration,
    };

    match executor.execute(&ctx).await {
        Ok(ExecuteResult::Task(task)) => {
            let _ = task_store.create(&task);
            Box::new(warp::reply::with_status(
                warp::reply::json(&task),
                warp::http::StatusCode::OK,
            ))
        }
        Ok(ExecuteResult::Message(msg)) => Box::new(warp::reply::with_status(
            warp::reply::json(&msg),
            warp::http::StatusCode::OK,
        )),
        Err(e) => Box::new(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({"error": e.to_string()})),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )),
    }
}

/// Handle REST `GET /tasks/{id}`.
pub fn handle_rest_get_task<S>(task_id: String, task_store: Arc<S>) -> Box<dyn warp::Reply>
where
    S: TaskStore,
{
    match task_store.get(&task_id) {
        Ok(Some(stored)) => Box::new(warp::reply::json(&stored.task)),
        Ok(None) => Box::new(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({"error": format!("task not found: {task_id}")})),
            warp::http::StatusCode::NOT_FOUND,
        )),
        Err(e) => Box::new(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({"error": e.to_string()})),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )),
    }
}

/// Handle REST `POST /tasks/{id}:cancel`.
pub async fn handle_rest_cancel_task<E, S>(
    task_id: String,
    executor: Arc<E>,
    task_store: Arc<S>,
) -> Box<dyn warp::Reply>
where
    E: AgentExecutor,
    S: TaskStore,
{
    match task_store.get(&task_id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return Box::new(warp::reply::with_status(
                warp::reply::json(
                    &serde_json::json!({"error": format!("task not found: {task_id}")}),
                ),
                warp::http::StatusCode::NOT_FOUND,
            ));
        }
        Err(e) => {
            return Box::new(warp::reply::with_status(
                warp::reply::json(&serde_json::json!({"error": e.to_string()})),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            ));
        }
    }

    match executor.cancel(&task_id).await {
        Ok(task) => {
            let _ = task_store.create(&task);
            Box::new(warp::reply::json(&task))
        }
        Err(e) => Box::new(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({"error": e.to_string()})),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )),
    }
}

/// Handle REST `POST /tasks/{id}/push-notification-configs`.
pub fn handle_rest_create_push_config<P>(
    _task_id: String,
    config: TaskPushNotificationConfig,
    push_store: Arc<P>,
) -> Box<dyn warp::Reply>
where
    P: PushNotificationStore,
{
    match push_store.create_config(&config) {
        Ok(stored) => Box::new(warp::reply::with_status(
            warp::reply::json(&stored),
            warp::http::StatusCode::CREATED,
        )),
        Err(e) => Box::new(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({"error": e.to_string()})),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )),
    }
}

/// Handle REST `GET /tasks/{id}/push-notification-configs/{config_id}`.
pub fn handle_rest_get_push_config<P>(
    task_id: String,
    config_id: String,
    push_store: Arc<P>,
) -> Box<dyn warp::Reply>
where
    P: PushNotificationStore,
{
    match push_store.get_config(&task_id, &config_id) {
        Ok(Some(config)) => Box::new(warp::reply::json(&config)),
        Ok(None) => Box::new(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({
                "error": format!("push config not found: task={task_id} id={config_id}")
            })),
            warp::http::StatusCode::NOT_FOUND,
        )),
        Err(e) => Box::new(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({"error": e.to_string()})),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )),
    }
}

/// Handle REST `GET /tasks/{id}/push-notification-configs`.
pub fn handle_rest_list_push_configs<P>(task_id: String, push_store: Arc<P>) -> Box<dyn warp::Reply>
where
    P: PushNotificationStore,
{
    match push_store.list_configs(&task_id) {
        Ok(configs) => {
            let response = ListTaskPushNotificationConfigsResponse { configs };
            Box::new(warp::reply::json(&response))
        }
        Err(e) => Box::new(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({"error": e.to_string()})),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )),
    }
}

/// Handle REST `DELETE /tasks/{id}/push-notification-configs/{config_id}`.
pub fn handle_rest_delete_push_config<P>(
    task_id: String,
    config_id: String,
    push_store: Arc<P>,
) -> Box<dyn warp::Reply>
where
    P: PushNotificationStore,
{
    match push_store.delete_config(&task_id, &config_id) {
        Ok(()) => Box::new(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({"success": true})),
            warp::http::StatusCode::OK,
        )),
        Err(A2aError::PushNotificationNotFound { .. }) => Box::new(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({
                "error": format!("push config not found: task={task_id} id={config_id}")
            })),
            warp::http::StatusCode::NOT_FOUND,
        )),
        Err(e) => Box::new(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({"error": e.to_string()})),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )),
    }
}

// ── Helpers ─────────────────────────────────────────────────────

fn deserialize_params<T: serde::de::DeserializeOwned>(
    params: &serde_json::Value,
) -> Result<T, Box<JsonRpcResponse>> {
    serde_json::from_value::<T>(params.clone())
        .map_err(|e| Box::new(error_response("", -32602, format!("invalid params: {e}"))))
}

fn success_response<T: serde::Serialize>(id: &str, result: &T) -> JsonRpcResponse {
    let value = serde_json::to_value(result).unwrap_or(serde_json::Value::Null);
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: id.to_string(),
        result: Some(value),
        error: None,
    }
}

fn error_response(id: &str, code: i64, message: String) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: id.to_string(),
        result: None,
        error: Some(JsonRpcError {
            code,
            message,
            data: None,
        }),
    }
}

fn execution_error_response(id: &str, err: &A2aError) -> JsonRpcResponse {
    let code = match err {
        A2aError::TaskNotFound { .. } => -32001,
        A2aError::Execution(_) => -32000,
        _ => -32603,
    };
    error_response(id, code, err.to_string())
}

fn generate_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("{prefix}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Helper to set the id on an error response built without one.
trait WithId {
    fn with_id(self, id: &str) -> Self;
}

impl WithId for JsonRpcResponse {
    fn with_id(mut self, id: &str) -> Self {
        self.id = id.to_string();
        self
    }
}

/// Helper trait for `GetExtendedAgentCard` — finds first registered agent.
trait GetExtendedByFirst {
    fn get_extended_by_first(
        &self,
        registry: &dyn AgentCardRegistry,
    ) -> Result<Option<bitrouter_a2a::registry::AgentRegistration>, A2aError>;
}

impl<R: AgentCardRegistry> GetExtendedByFirst for R {
    fn get_extended_by_first(
        &self,
        _registry: &dyn AgentCardRegistry,
    ) -> Result<Option<bitrouter_a2a::registry::AgentRegistration>, A2aError> {
        let list = self.list()?;
        if let Some(first) = list.into_iter().next() {
            self.get_extended(&first.card.name)
        } else {
            Ok(None)
        }
    }
}
