//! Separate operator transport, independent of inference readiness.

pub(super) mod auth;
mod media_type;
#[cfg(test)]
mod tests;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Extension, Json, Router,
    body::to_bytes,
    extract::{Query, Request, State, rejection::QueryRejection},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bitrouter_ai::gateway::operator::{ErrorCode, ErrorResponse, Mutation};
use serde::Deserialize;

use super::{
    authority::Authority,
    operator::{Actor, Failure, Operator},
};

struct Admission {
    auth: Arc<auth::Auth>,
    budget: Duration,
}
#[derive(Default)]
struct RequestIdentity(Mutex<Option<String>>);

pub(super) fn router(authority: Authority, auth: Arc<auth::Auth>, budget: Duration) -> Router {
    Router::new()
        .route("/operator/v1/mutations", post(apply))
        .route("/operator/v1/operations", get(operation))
        .route("/operator/v1/revision", get(revision))
        .route("/operator/v1/tenants", get(tenant))
        .route("/operator/v1/resources", get(resource))
        .route("/operator/v1/principals", get(principal))
        .route("/operator/v1/credentials", get(credential))
        .route("/operator/v1/executions", get(execution))
        .fallback(|| async { failure(ErrorCode::NotFound, None) })
        .with_state(Operator::new(authority))
        .layer(middleware::from_fn_with_state(
            Arc::new(Admission { auth, budget }),
            authenticate,
        ))
}

async fn authenticate(
    State(admission): State<Arc<Admission>>,
    mut request: Request,
    next: Next,
) -> Response {
    // Authorization precedes query parsing and body reads. Neither the inference
    // bearer nor legacy control credentials are consulted here.
    let deadline = tokio::time::Instant::now() + admission.budget;
    let mut response = if let Some(actor) = admission.auth.authenticate(request.headers()) {
        let mutation = request.method() == axum::http::Method::POST;
        let identity = Arc::new(RequestIdentity::default());
        request.extensions_mut().insert(actor);
        request.extensions_mut().insert(identity.clone());
        match tokio::time::timeout_at(deadline, next.run(request)).await {
            Ok(response) => response,
            Err(_) => failure(
                if mutation {
                    ErrorCode::Unknown
                } else {
                    ErrorCode::Unavailable
                },
                identity
                    .0
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone(),
            ),
        }
    } else {
        let mut response = failure(ErrorCode::Unauthorized, None);
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            axum::http::HeaderValue::from_static("Bearer realm=\"bitrouter-operator\""),
        );
        response
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

async fn apply(
    State(operator): State<Operator>,
    Extension(actor): Extension<Arc<Actor>>,
    Extension(identity): Extension<Arc<RequestIdentity>>,
    request: Request,
) -> Response {
    let mut types = request.headers().get_all(header::CONTENT_TYPE).iter();
    let is_json = types
        .next()
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| media_type::matches(value, "application/json"));
    if !is_json
        || types.next().is_some()
        || request.headers().contains_key(header::CONTENT_ENCODING)
    {
        return failure(ErrorCode::InvalidInput, None);
    }
    let Ok(bytes) = to_bytes(request.into_body(), 2 * 1024 * 1024).await else {
        return failure(ErrorCode::InvalidInput, None);
    };
    let Ok(mutation) = serde_json::from_slice::<Mutation>(&bytes) else {
        return failure(ErrorCode::InvalidInput, None);
    };
    // Reflect only a validated public handle, never arbitrary JSON or diagnostics.
    let operation_id = super::operator::validate_handle(&mutation.operation_id)
        .is_ok()
        .then_some(mutation.operation_id.clone());
    *identity.0.lock().unwrap_or_else(|error| error.into_inner()) = operation_id.clone();
    match operator.apply(&actor, &mutation).await {
        Ok(receipt) => Json(receipt).into_response(),
        Err(error) => failure(code(&error, ErrorCode::Unknown), operation_id),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationQuery {
    operation_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyQuery {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdQuery {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialQuery {
    provider: String,
    account: String,
}

async fn operation(
    State(operator): State<Operator>,
    Extension(actor): Extension<Arc<Actor>>,
    query: Result<Query<OperationQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return failure(ErrorCode::InvalidInput, None);
    };
    read_result(operator.lookup(&actor, &query.operation_id).await)
}

async fn revision(
    State(operator): State<Operator>,
    Extension(actor): Extension<Arc<Actor>>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
) -> Response {
    if query.is_err() {
        return failure(ErrorCode::InvalidInput, None);
    }
    read_result(operator.revision(&actor).await)
}

async fn tenant(
    State(operator): State<Operator>,
    Extension(actor): Extension<Arc<Actor>>,
    query: Result<Query<IdQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return failure(ErrorCode::InvalidInput, None);
    };
    read_result(operator.tenant(&actor, &query.id).await)
}

async fn resource(
    State(operator): State<Operator>,
    Extension(actor): Extension<Arc<Actor>>,
    query: Result<Query<IdQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return failure(ErrorCode::InvalidInput, None);
    };
    read_result(operator.resource(&actor, &query.id).await)
}

async fn principal(
    State(operator): State<Operator>,
    Extension(actor): Extension<Arc<Actor>>,
    query: Result<Query<IdQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return failure(ErrorCode::InvalidInput, None);
    };
    read_result(operator.principal(&actor, &query.id).await)
}

async fn credential(
    State(operator): State<Operator>,
    Extension(actor): Extension<Arc<Actor>>,
    query: Result<Query<CredentialQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return failure(ErrorCode::InvalidInput, None);
    };
    read_result(
        operator
            .credential(&actor, &query.provider, &query.account)
            .await,
    )
}

async fn execution(
    State(operator): State<Operator>,
    Extension(actor): Extension<Arc<Actor>>,
    query: Result<Query<IdQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return failure(ErrorCode::InvalidInput, None);
    };
    read_result(operator.execution(&actor, &query.id).await)
}

fn read_result<T: serde::Serialize>(result: anyhow::Result<T>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => failure(code(&error, ErrorCode::Unavailable), None),
    }
}

fn code(error: &anyhow::Error, fallback: ErrorCode) -> ErrorCode {
    error
        .downcast_ref::<Failure>()
        .map_or(fallback, |failure| failure.0)
}

fn failure(code: ErrorCode, operation_id: Option<String>) -> Response {
    let status = match code {
        ErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
        ErrorCode::Forbidden => StatusCode::FORBIDDEN,
        ErrorCode::InvalidInput => StatusCode::BAD_REQUEST,
        ErrorCode::Conflict => StatusCode::CONFLICT,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::Unavailable | ErrorCode::Unknown => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, Json(ErrorResponse { code, operation_id })).into_response()
}
