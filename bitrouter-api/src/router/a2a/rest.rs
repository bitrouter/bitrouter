//! REST endpoint bindings for A2A gateway.

use std::sync::Arc;

use warp::Filter;

use super::types::*;

fn error_reply(status: warp::http::StatusCode, message: &str) -> Box<dyn warp::Reply> {
    Box::new(warp::reply::with_status(
        warp::reply::json(&serde_json::json!({"error": message})),
        status,
    ))
}

fn gateway_error_reply(err: &A2aGatewayError) -> Box<dyn warp::Reply> {
    let status = match err {
        A2aGatewayError::AgentNotFound { .. } => warp::http::StatusCode::NOT_FOUND,
        A2aGatewayError::InvalidConfig { .. } => warp::http::StatusCode::BAD_REQUEST,
        _ => warp::http::StatusCode::BAD_GATEWAY,
    };
    error_reply(status, &err.to_string())
}

pub(crate) fn rest_send_filter<T>(
    gateway: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: A2aGateway + 'static,
{
    warp::path!("message:send")
        .and(warp::post())
        .and(warp::body::json::<SendMessageRequest>())
        .and(warp::any().map(move || gateway.clone()))
        .then(
            |body: SendMessageRequest, gateway: Option<Arc<T>>| async move {
                let Some(gw) = gateway else {
                    return error_reply(
                        warp::http::StatusCode::NOT_FOUND,
                        "A2A gateway not configured",
                    );
                };
                match gw.send_message(body).await {
                    Ok(result) => Box::new(warp::reply::json(&result)) as Box<dyn warp::Reply>,
                    Err(e) => gateway_error_reply(&e),
                }
            },
        )
}

pub(crate) fn rest_get_task_filter<T>(
    gateway: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: A2aGateway + 'static,
{
    warp::path!("tasks" / String)
        .and(warp::get())
        .and(warp::any().map(move || gateway.clone()))
        .then(|task_id: String, gateway: Option<Arc<T>>| async move {
            let Some(gw) = gateway else {
                return error_reply(
                    warp::http::StatusCode::NOT_FOUND,
                    "A2A gateway not configured",
                );
            };
            let req = GetTaskRequest {
                id: task_id,
                history_length: None,
            };
            match gw.get_task(req).await {
                Ok(task) => Box::new(warp::reply::json(&task)) as Box<dyn warp::Reply>,
                Err(e) => gateway_error_reply(&e),
            }
        })
}

pub(crate) fn rest_cancel_filter<T>(
    gateway: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: A2aGateway + 'static,
{
    warp::path("tasks")
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::any().map(move || gateway.clone()))
        .and_then(
            |task_id_action: String, gateway: Option<Arc<T>>| async move {
                let Some(task_id) = task_id_action.strip_suffix(":cancel") else {
                    return Err(warp::reject::not_found());
                };
                let Some(gw) = gateway else {
                    return Ok(error_reply(
                        warp::http::StatusCode::NOT_FOUND,
                        "A2A gateway not configured",
                    ));
                };
                let req = CancelTaskRequest {
                    id: task_id.to_string(),
                };
                let reply: Box<dyn warp::Reply> = match gw.cancel_task(req).await {
                    Ok(task) => Box::new(warp::reply::json(&task)),
                    Err(e) => gateway_error_reply(&e),
                };
                Ok(reply)
            },
        )
}

pub(crate) fn rest_push_create_filter<T>(
    gateway: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: A2aGateway + 'static,
{
    warp::path!("tasks" / String / "push-notification-configs")
        .and(warp::post())
        .and(warp::body::json::<TaskPushNotificationConfig>())
        .and(warp::any().map(move || gateway.clone()))
        .then(
            |_task_id: String,
             config: TaskPushNotificationConfig,
             gateway: Option<Arc<T>>| async move {
                let Some(gw) = gateway else {
                    return error_reply(
                        warp::http::StatusCode::NOT_FOUND,
                        "A2A gateway not configured",
                    );
                };
                match gw.set_push_config(config).await {
                    Ok(stored) => Box::new(warp::reply::with_status(
                        warp::reply::json(&stored),
                        warp::http::StatusCode::CREATED,
                    )) as Box<dyn warp::Reply>,
                    Err(e) => gateway_error_reply(&e),
                }
            },
        )
}

pub(crate) fn rest_push_get_filter<T>(
    gateway: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: A2aGateway + 'static,
{
    warp::path!("tasks" / String / "push-notification-configs" / String)
        .and(warp::get())
        .and(warp::any().map(move || gateway.clone()))
        .then(
            |task_id: String, config_id: String, gateway: Option<Arc<T>>| async move {
                let Some(gw) = gateway else {
                    return error_reply(
                        warp::http::StatusCode::NOT_FOUND,
                        "A2A gateway not configured",
                    );
                };
                match gw.get_push_config(&task_id, Some(&config_id)).await {
                    Ok(config) => Box::new(warp::reply::json(&config)) as Box<dyn warp::Reply>,
                    Err(e) => gateway_error_reply(&e),
                }
            },
        )
}

pub(crate) fn rest_push_list_filter<T>(
    gateway: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: A2aGateway + 'static,
{
    warp::path!("tasks" / String / "push-notification-configs")
        .and(warp::get())
        .and(warp::any().map(move || gateway.clone()))
        .then(|task_id: String, gateway: Option<Arc<T>>| async move {
            let Some(gw) = gateway else {
                return error_reply(
                    warp::http::StatusCode::NOT_FOUND,
                    "A2A gateway not configured",
                );
            };
            match gw.list_push_configs(&task_id).await {
                Ok(resp) => Box::new(warp::reply::json(&resp)) as Box<dyn warp::Reply>,
                Err(e) => gateway_error_reply(&e),
            }
        })
}

pub(crate) fn rest_push_delete_filter<T>(
    gateway: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: A2aGateway + 'static,
{
    warp::path!("tasks" / String / "push-notification-configs" / String)
        .and(warp::delete())
        .and(warp::any().map(move || gateway.clone()))
        .then(
            |task_id: String, config_id: String, gateway: Option<Arc<T>>| async move {
                let Some(gw) = gateway else {
                    return error_reply(
                        warp::http::StatusCode::NOT_FOUND,
                        "A2A gateway not configured",
                    );
                };
                match gw.delete_push_config(&task_id, &config_id).await {
                    Ok(()) => Box::new(warp::reply::with_status(
                        warp::reply::json(&serde_json::json!({"success": true})),
                        warp::http::StatusCode::OK,
                    )) as Box<dyn warp::Reply>,
                    Err(e) => gateway_error_reply(&e),
                }
            },
        )
}
