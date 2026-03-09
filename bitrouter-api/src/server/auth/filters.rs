use std::{convert::Infallible, sync::Arc};

use bitrouter_core::server::{
    auth::{AuthContext, AuthDecision, Authenticator, AuthScope},
    errors::ServerError,
    ids::RequestId,
};
use warp::{Filter, Rejection, Reply, http};

use crate::util::generate_id;

#[derive(Debug)]
struct AuthRejection(ServerError);

impl warp::reject::Reject for AuthRejection {}

pub fn auth_context_filter<T>(
    authenticator: Arc<T>,
    required_scopes: Vec<AuthScope>,
) -> impl Filter<Extract = (AuthDecision,), Error = Rejection> + Clone
where
    T: Authenticator + Send + Sync + 'static,
{
    let required_scopes = Arc::new(required_scopes);

    warp::header::optional::<String>("authorization")
        .and(warp::header::optional::<String>("x-request-id"))
        .and(warp::method())
        .and(warp::path::full())
        .and(warp::any().map(move || authenticator.clone()))
        .and(warp::any().map(move || required_scopes.clone()))
        .and_then(handle_authentication)
}

async fn handle_authentication<T>(
    authorization: Option<String>,
    request_id: Option<String>,
    method: http::Method,
    path: warp::path::FullPath,
    authenticator: Arc<T>,
    required_scopes: Arc<Vec<AuthScope>>,
) -> Result<AuthDecision, Rejection>
where
    T: Authenticator + Send + Sync + 'static,
{
    let decision = authenticator
        .authenticate(AuthContext {
            request_id: RequestId::from(request_id.unwrap_or_else(generate_id)),
            method,
            path: path.as_str().to_owned(),
            authorization,
            remote_addr: None,
            required_scopes: (*required_scopes).clone(),
        })
        .await
        .map_err(|error| warp::reject::custom(AuthRejection(error)))?;

    match decision {
        AuthDecision::Allow { .. } => Ok(decision),
        AuthDecision::Deny(error) => Err(warp::reject::custom(AuthRejection(error))),
    }
}

pub async fn rejection_handler(err: Rejection) -> Result<impl Reply, Infallible> {
    let (code, message, error_type) = if let Some(AuthRejection(error)) = err.find::<AuthRejection>()
    {
        match error {
            ServerError::InvalidInput { message } => {
                (warp::http::StatusCode::BAD_REQUEST, message.clone(), "invalid_input")
            }
            ServerError::Unauthorized { message } => {
                (warp::http::StatusCode::UNAUTHORIZED, message.clone(), "unauthorized")
            }
            ServerError::Forbidden { message } => {
                (warp::http::StatusCode::FORBIDDEN, message.clone(), "forbidden")
            }
            ServerError::NotFound { resource } => (
                warp::http::StatusCode::NOT_FOUND,
                resource.clone(),
                "not_found",
            ),
            ServerError::Conflict { message } => {
                (warp::http::StatusCode::CONFLICT, message.clone(), "conflict")
            }
            ServerError::RateLimited { message, .. } => (
                warp::http::StatusCode::TOO_MANY_REQUESTS,
                message.clone(),
                "rate_limited",
            ),
            ServerError::Unavailable { message } => (
                warp::http::StatusCode::SERVICE_UNAVAILABLE,
                message.clone(),
                "unavailable",
            ),
            ServerError::Internal { message } => (
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
                message.clone(),
                "internal_error",
            ),
        }
    } else if err.is_not_found() {
        (
            warp::http::StatusCode::NOT_FOUND,
            "not found".to_owned(),
            "not_found",
        )
    } else {
        (
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            "internal server error".to_owned(),
            "internal_error",
        )
    };

    let json = warp::reply::json(&serde_json::json!({
        "error": {
            "message": message,
            "type": error_type,
        }
    }));

    Ok(warp::reply::with_status(json, code))
}
