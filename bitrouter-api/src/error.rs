use std::fmt;

use bitrouter_core::errors::BitrouterError;
use warp::reject::Reject;

/// Wraps a [`BitrouterError`] so it can be used as a warp rejection.
#[derive(Debug)]
pub(crate) struct BitrouterRejection(pub BitrouterError);

impl fmt::Display for BitrouterRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Reject for BitrouterRejection {}

/// Wraps a generic message as a warp rejection.
#[derive(Debug)]
pub(crate) struct BadRequest(pub String);

impl fmt::Display for BadRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Reject for BadRequest {}

/// Converts a [`BitrouterRejection`] or [`BadRequest`] warp rejection into a
/// structured JSON error response.
///
/// Returns `Some(response)` if the rejection matches, `None` otherwise —
/// allowing callers to fall through to other rejection handling.
pub fn handle_bitrouter_rejection(err: &warp::Rejection) -> Option<warp::http::Response<String>> {
    use warp::http::StatusCode;

    if let Some(e) = err.find::<BitrouterRejection>() {
        let (status, error_type) = match &e.0 {
            BitrouterError::InvalidRequest { .. } | BitrouterError::UnsupportedFeature { .. } => {
                (StatusCode::BAD_REQUEST, "invalid_request_error")
            }
            BitrouterError::AccessDenied { .. } => (StatusCode::FORBIDDEN, "access_denied"),
            BitrouterError::Cancelled { .. } => (StatusCode::BAD_REQUEST, "cancelled"),
            BitrouterError::Provider { context, .. } => {
                let status = context
                    .status_code
                    .and_then(|code| StatusCode::from_u16(code).ok())
                    .unwrap_or(StatusCode::BAD_GATEWAY);
                (status, "provider_error")
            }
            BitrouterError::Transport { .. }
            | BitrouterError::ResponseDecode { .. }
            | BitrouterError::InvalidResponse { .. }
            | BitrouterError::StreamProtocol { .. } => (StatusCode::BAD_GATEWAY, "upstream_error"),
        };

        let body = serde_json::json!({
            "error": {
                "message": e.0.to_string(),
                "type": error_type,
            }
        })
        .to_string();

        let response = warp::http::Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(body)
            .ok()?;

        return Some(response);
    }

    if let Some(e) = err.find::<BadRequest>() {
        let body = serde_json::json!({
            "error": {
                "message": e.to_string(),
                "type": "invalid_request_error",
            }
        })
        .to_string();

        let response = warp::http::Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header("content-type", "application/json")
            .body(body)
            .ok()?;

        return Some(response);
    }

    None
}
