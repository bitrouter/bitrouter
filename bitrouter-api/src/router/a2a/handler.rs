//! JSON-RPC 2.0 dispatch handler for A2A server methods.
//!
//! Handles `SendMessage`, `GetTask`, and `CancelTask` methods per the
//! A2A v1.0 specification.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bitrouter_a2a::error::A2aError;
use bitrouter_a2a::jsonrpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use bitrouter_a2a::message::Message;
use bitrouter_a2a::server::{AgentExecutor, ExecuteResult, ExecutorContext, TaskStore};

/// Dispatch a JSON-RPC request to the appropriate A2A method handler.
pub async fn handle_jsonrpc<E, S>(
    request: JsonRpcRequest,
    executor: Arc<E>,
    task_store: Arc<S>,
) -> JsonRpcResponse
where
    E: AgentExecutor,
    S: TaskStore,
{
    match request.method.as_str() {
        "SendMessage" => handle_send_message(&request, executor, task_store).await,
        "GetTask" => handle_get_task(&request, task_store).await,
        "CancelTask" => handle_cancel_task(&request, executor, task_store).await,
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
    // Extract message from params.
    let message = match extract_message(&request.params) {
        Ok(msg) => msg,
        Err(resp) => return (*resp).with_id(&request.id),
    };

    // Derive task and context IDs.
    let task_id = message
        .task_id
        .clone()
        .unwrap_or_else(|| generate_id("task"));
    let context_id = message
        .context_id
        .clone()
        .unwrap_or_else(|| generate_id("ctx"));

    let ctx = ExecutorContext {
        message,
        task_id,
        context_id,
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
            success_response(&request.id, &task)
        }
        ExecuteResult::Message(msg) => success_response(&request.id, &msg),
    }
}

async fn handle_get_task<S>(request: &JsonRpcRequest, task_store: Arc<S>) -> JsonRpcResponse
where
    S: TaskStore,
{
    let task_id = match extract_task_id(&request.params) {
        Ok(id) => id,
        Err(resp) => return (*resp).with_id(&request.id),
    };

    match task_store.get(&task_id) {
        Ok(Some(stored)) => success_response(&request.id, &stored.task),
        Ok(None) => error_response(&request.id, -32001, format!("task not found: {task_id}")),
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
    let task_id = match extract_task_id(&request.params) {
        Ok(id) => id,
        Err(resp) => return (*resp).with_id(&request.id),
    };

    // Check task exists.
    match task_store.get(&task_id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return error_response(&request.id, -32001, format!("task not found: {task_id}"));
        }
        Err(e) => return error_response(&request.id, -32603, e.to_string()),
    }

    match executor.cancel(&task_id).await {
        Ok(task) => {
            // Update the task in store (best-effort).
            let _ = task_store.create(&task);
            success_response(&request.id, &task)
        }
        Err(e) => execution_error_response(&request.id, &e),
    }
}

// ── Helpers ─────────────────────────────────────────────────────

fn extract_message(params: &serde_json::Value) -> Result<Message, Box<JsonRpcResponse>> {
    let msg_val = params.get("message").ok_or_else(|| {
        Box::new(error_response(
            "",
            -32602,
            "missing 'message' in params".to_string(),
        ))
    })?;

    serde_json::from_value::<Message>(msg_val.clone())
        .map_err(|e| Box::new(error_response("", -32602, format!("invalid message: {e}"))))
}

fn extract_task_id(params: &serde_json::Value) -> Result<String, Box<JsonRpcResponse>> {
    params
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            Box::new(error_response(
                "",
                -32602,
                "missing 'id' in params".to_string(),
            ))
        })
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
