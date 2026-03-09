//! Shared rejection handler for server management endpoints.

use bitrouter_core::server::errors::ServerError;

use crate::error::{BadRequest, BitrouterRejection, ServerRejection};

/// Creates a rejection handler that converts server rejections into appropriate HTTP responses.
///
/// Handles [`ServerRejection`], [`BitrouterRejection`], and [`BadRequest`].
pub async fn rejection_handler(
    err: warp::Rejection,
) -> Result<impl warp::Reply, std::convert::Infallible> {
    let (code, message) = if let Some(e) = err.find::<ServerRejection>() {
        (server_error_status(&e.0), e.to_string())
    } else if let Some(e) = err.find::<BitrouterRejection>() {
        (warp::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    } else if let Some(e) = err.find::<BadRequest>() {
        (warp::http::StatusCode::BAD_REQUEST, e.to_string())
    } else if err.is_not_found() {
        (warp::http::StatusCode::NOT_FOUND, "not found".to_owned())
    } else {
        (
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            "internal server error".to_owned(),
        )
    };

    let json = warp::reply::json(&serde_json::json!({
        "error": {
            "message": message,
            "type": error_type_label(&code),
        }
    }));
    Ok(warp::reply::with_status(json, code))
}

fn server_error_status(err: &ServerError) -> warp::http::StatusCode {
    match err {
        ServerError::NotFound { .. } => warp::http::StatusCode::NOT_FOUND,
        ServerError::AlreadyExists { .. } => warp::http::StatusCode::CONFLICT,
        ServerError::Unauthorized { .. } => warp::http::StatusCode::UNAUTHORIZED,
        ServerError::Forbidden { .. } => warp::http::StatusCode::FORBIDDEN,
        ServerError::RateLimited { .. } => warp::http::StatusCode::TOO_MANY_REQUESTS,
        ServerError::SpendLimitExceeded { .. } => warp::http::StatusCode::PAYMENT_REQUIRED,
        ServerError::InvalidInput { .. } => warp::http::StatusCode::BAD_REQUEST,
        ServerError::Internal { .. } => warp::http::StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn error_type_label(code: &warp::http::StatusCode) -> &'static str {
    match code.as_u16() {
        400 => "invalid_request_error",
        401 => "authentication_error",
        402 => "billing_error",
        403 => "permission_error",
        404 => "not_found_error",
        409 => "conflict_error",
        429 => "rate_limit_error",
        _ => "server_error",
    }
}
