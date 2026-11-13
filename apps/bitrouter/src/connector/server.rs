//! Authenticated loopback pilot service independent of gateway or Cloud state.

use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::json;
use subtle::ConstantTimeEq;
use tokio::sync::{Semaphore, mpsc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{
    config::ConnectorConfig,
    controller::{Admission, Controller, OperatorAttestation, RequestIdentity},
    executor::Executor,
    observer::Observer,
    request::LocalRequest,
};

#[derive(Clone)]
struct Service {
    controller: Controller,
    executor: Executor,
    auth: Arc<Vec<u8>>,
    preprocessing: Arc<Semaphore>,
}

pub async fn serve(
    config: ConnectorConfig,
    listen: SocketAddr,
    initialize: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        listen.ip().is_loopback(),
        "connector pilot listen address must be loopback"
    );
    let execution = config
        .execution
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("execution_not_configured"))?;
    let auth = bearer(&config).map_err(anyhow::Error::msg)?;
    let executor = Executor::new(&config).map_err(anyhow::Error::msg)?;
    let interval = Duration::from_millis(execution.update_interval_ms);
    let preprocess = execution.max_extra_inflight as usize;
    let controller = Controller::open(config.clone(), initialize).await?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|_| anyhow::anyhow!("could not bind connector loopback listener"))?;
    let service = Service {
        controller: controller.clone(),
        executor,
        auth: Arc::new(auth.into_bytes()),
        preprocessing: Arc::new(Semaphore::new(preprocess)),
    };
    let shutdown = CancellationToken::new();
    let observer_shutdown = shutdown.clone();
    let observing = controller.clone();
    let observer_task = tokio::spawn(async move {
        let mut observer = match Observer::new(config) {
            Ok(observer) => observer,
            Err(_) => return,
        };
        loop {
            tokio::select! {
                _ = observer_shutdown.cancelled() => return,
                snapshot = observer.observe() => {
                    let _ = observing.update_snapshot(snapshot).await;
                }
            }
            tokio::select! {
                _ = observer_shutdown.cancelled() => return,
                _ = tokio::time::sleep(interval) => {},
            }
        }
    });
    let app = Router::new()
        .route("/v1/connector/status", get(status))
        .route("/v1/connector/pause", post(pause))
        .route("/v1/connector/resume", post(resume))
        .route("/v1/connector/requests", post(infer))
        .route("/v1/connector/requests/{id}", get(query))
        .route("/v1/connector/requests/{id}/cancel", post(cancel))
        .route("/v1/connector/requests/{id}/reconcile", post(reconcile))
        .layer(middleware::from_fn_with_state(
            service.clone(),
            authenticate,
        ))
        .with_state(service);
    let stop = shutdown.clone();
    let shutdown_controller = controller.clone();
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_requested().await;
            let _ = shutdown_controller.pause().await;
            stop.cancel();
        })
        .await;
    shutdown.cancel();
    let _ = observer_task.await;
    result.map_err(|_| anyhow::anyhow!("connector local service stopped with a transport error"))
}

async fn shutdown_requested() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = terminate.recv() => {},
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

/// Credentials are resolved locally and never included in reports or debug data.
pub fn bearer(config: &ConnectorConfig) -> Result<String, &'static str> {
    let execution = config
        .execution
        .as_ref()
        .ok_or("execution_not_configured")?;
    let value = std::env::var(&execution.auth_bearer_env)
        .map_err(|_| "local_authentication_unavailable")?;
    if !(32..=4096).contains(&value.len()) || value.bytes().any(|byte| !byte.is_ascii_graphic()) {
        return Err("local_authentication_unavailable");
    }
    Ok(value)
}

async fn authenticate(State(service): State<Service>, request: Request, next: Next) -> Response {
    let valid = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.as_bytes().strip_prefix(b"Bearer "))
        .is_some_and(|value| bool::from(value.ct_eq(service.auth.as_slice())));
    if !valid {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    next.run(request).await
}

async fn status(State(service): State<Service>) -> Response {
    match service.controller.status().await {
        Ok(value) => axum::Json(value).into_response(),
        Err(problem) => error(StatusCode::SERVICE_UNAVAILABLE, problem.code()),
    }
}

async fn pause(State(service): State<Service>) -> Response {
    match service.controller.pause().await {
        Ok(value) => axum::Json(value).into_response(),
        Err(problem) => error(StatusCode::SERVICE_UNAVAILABLE, problem.code()),
    }
}

async fn resume(State(service): State<Service>) -> Response {
    match service.controller.resume().await {
        Ok(value) => axum::Json(value).into_response(),
        Err(problem) => error(StatusCode::SERVICE_UNAVAILABLE, problem.code()),
    }
}

async fn query(State(service): State<Service>, Path(id): Path<String>) -> Response {
    match service.controller.query(&id).await {
        Ok(Some(value)) => axum::Json(value).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "request_not_found"),
        Err(problem) => error(StatusCode::SERVICE_UNAVAILABLE, problem.code()),
    }
}

async fn cancel(State(service): State<Service>, Path(id): Path<String>) -> Response {
    match service.controller.cancel_before_send(&id).await {
        Ok(value) => axum::Json(value).into_response(),
        Err(problem) => error(StatusCode::CONFLICT, problem.code()),
    }
}

async fn reconcile(State(service): State<Service>, Path(id): Path<String>, body: Body) -> Response {
    let bounded = tokio::time::timeout(Duration::from_secs(5), to_bytes(body, 2048)).await;
    let attestation: OperatorAttestation = match bounded {
        Ok(Ok(bytes)) => match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => return error(StatusCode::BAD_REQUEST, "invalid_operator_attestation"),
        },
        _ => return error(StatusCode::BAD_REQUEST, "invalid_operator_attestation"),
    };
    match service.controller.reconcile(&id, attestation).await {
        Ok(record) => axum::Json(record).into_response(),
        Err(problem) => error(StatusCode::CONFLICT, problem.code()),
    }
}

async fn infer(State(service): State<Service>, headers: HeaderMap, body: Body) -> Response {
    let started = Instant::now();
    let config = service.controller.config();
    let Some(execution) = config.execution.as_ref() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "execution_not_configured");
    };
    if !headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(';').next() == Some("application/json"))
    {
        return error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "expected_json");
    }
    let Ok(_permit) = service.preprocessing.clone().try_acquire_owned() else {
        return error(StatusCode::TOO_MANY_REQUESTS, "local_preprocessing_limit");
    };
    let body = match tokio::time::timeout(
        Duration::from_millis(execution.max_deadline_ms),
        to_bytes(body, execution.max_request_bytes),
    )
    .await
    {
        Ok(Ok(body)) => body,
        Ok(Err(_)) => return error(StatusCode::PAYLOAD_TOO_LARGE, "request_body_limit"),
        Err(_) => return error(StatusCode::REQUEST_TIMEOUT, "deadline"),
    };
    let request: LocalRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => return error(StatusCode::BAD_REQUEST, "unsupported_request_shape"),
    };
    let digest = match request.digest() {
        Ok(value) => value,
        Err(reason) => return error(StatusCode::BAD_REQUEST, reason),
    };
    match service
        .controller
        .lookup(&request.request_id, &digest)
        .await
    {
        Ok(Some(record)) => return axum::Json(record).into_response(),
        Ok(None) => {}
        Err(problem) => return error(StatusCode::CONFLICT, problem.code()),
    }
    // Retained identity is checked before mutable model/policy bindings. A
    // cold configuration change must never reinterpret an original attempt.
    if let Err(reason) = request.validate(config, service.controller.generation()) {
        return error(StatusCode::BAD_REQUEST, reason);
    }
    let deadline = started + Duration::from_millis(request.deadline_ms);
    if Instant::now() >= deadline {
        return error(StatusCode::REQUEST_TIMEOUT, "deadline");
    }
    // Refuse a known closed policy before even the non-generating tokenizer
    // call. The atomic reservation and send gate still recheck after it.
    match service.controller.status().await {
        Ok(status) if status.budget.max_dispatch_rps <= 0.0 => {
            return error(
                StatusCode::TOO_MANY_REQUESTS,
                status
                    .budget
                    .reason
                    .map_or("admission_closed", |reason| reason.code()),
            );
        }
        Ok(_) => {}
        Err(problem) => return error(StatusCode::SERVICE_UNAVAILABLE, problem.code()),
    }
    // A single member is chosen once. No retry, redirect or target fallback.
    let Some(member) = config.pool.members.first() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "target_configuration");
    };
    let input_tokens = match service
        .executor
        .count_input(config, member, &request, deadline)
        .await
    {
        Ok(value) => value,
        Err(reason) => return error(StatusCode::BAD_REQUEST, reason),
    };
    let wire = match service.executor.build(member, &request) {
        Ok(wire) => wire,
        Err(reason) => return error(StatusCode::SERVICE_UNAVAILABLE, reason),
    };
    let identity = RequestIdentity {
        request_id: request.request_id.clone(),
        digest,
        pool_id: request.pool_id.clone(),
        pool_generation: request.pool_generation.clone(),
        model: request.request.model.clone(),
        input_tokens,
        max_output_tokens: request.request.max_tokens,
        deadline_ms: chrono::Utc::now().timestamp_millis().saturating_add(
            i64::try_from(
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis(),
            )
            .map_or(0, |value| value),
        ),
        member_id: member.id.clone(),
    };
    let record = match service.controller.reserve(identity).await {
        Ok(Admission::Reserved(record)) => record,
        Ok(Admission::Existing(record)) => return axum::Json(record).into_response(),
        Err(problem) => return error(StatusCode::TOO_MANY_REQUESTS, problem.code()),
    };
    let streaming = request.request.stream;
    let (send, mut receive) = mpsc::channel::<Bytes>(8);
    let executor = service.executor.clone();
    tokio::spawn(async move {
        executor
            .execute(service.controller, record, request, wire, deadline, send)
            .await;
    });
    let stream = async_stream::stream! {
        while let Some(bytes) = receive.recv().await { yield Ok::<Bytes, Infallible>(bytes); }
    };
    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static(if streaming {
            "text/event-stream"
        } else {
            "application/json"
        }),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

fn error(status: StatusCode, code: &str) -> Response {
    (status, axum::Json(json!({"error": {"code": code}}))).into_response()
}
