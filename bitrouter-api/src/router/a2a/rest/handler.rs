//! REST-style handler wrappers for A2A v1.0 bindings.

use std::sync::Arc;

use bitrouter_a2a::error::A2aError;
use bitrouter_a2a::request::{
    ListTaskPushNotificationConfigsResponse, SendMessageRequest, TaskPushNotificationConfig,
};
use bitrouter_a2a::server::{
    AgentExecutor, ExecuteResult, ExecutorContext, PushNotificationStore, TaskStore,
};

use crate::router::a2a::jsonrpc::convert::generate_id;

/// Handle REST `POST /message:send` — wraps `SendMessage`.
pub(crate) async fn handle_rest_send_message<E, S>(
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
pub(crate) fn handle_rest_get_task<S>(task_id: String, task_store: Arc<S>) -> Box<dyn warp::Reply>
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
pub(crate) async fn handle_rest_cancel_task<E, S>(
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
pub(crate) fn handle_rest_create_push_config<P>(
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
pub(crate) fn handle_rest_get_push_config<P>(
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
pub(crate) fn handle_rest_list_push_configs<P>(
    task_id: String,
    push_store: Arc<P>,
) -> Box<dyn warp::Reply>
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
pub(crate) fn handle_rest_delete_push_config<P>(
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
