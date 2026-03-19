//! REST-style HTTP bindings for A2A v1.0.
//!
//! Provides routes like `POST /message:send`, `GET /tasks/{id}`,
//! `POST /tasks/{id}:cancel`, and push notification config CRUD.

mod handler;

use std::sync::Arc;

use warp::Filter;

use bitrouter_a2a::request::{SendMessageRequest, TaskPushNotificationConfig};
use bitrouter_a2a::server::{AgentExecutor, PushNotificationStore, TaskStore};

/// Creates REST-style HTTP filters for A2A v1.0 bindings.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::a2a::test_helpers::*;

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
