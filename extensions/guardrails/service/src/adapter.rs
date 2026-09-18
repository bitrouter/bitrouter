//! Small request-check v1 HTTP service adapter.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request as HttpRequest, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response as HttpResponse};
use axum::routing::post;
use bitrouter_checker_protocol::v1::{self, ProtocolError};
use serde::Serialize;
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;

/// Maximum number of checker callbacks that can run at once.
pub const MAX_CONCURRENT_CHECKS: usize = 32;
/// Maximum time spent receiving one complete request body.
pub const REQUEST_BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// Business decision returned by a request-check callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckDecision {
    /// The validated entry-request projection may proceed.
    Allow,
    /// The request must stop. The reason must follow the v1 reason-code grammar.
    Deny { reason_code: String },
}

/// Synchronous business callback invoked only with a validated v1 request.
///
/// The callback must be `Send + Sync` because requests execute concurrently.
/// CPU work is bounded by [`MAX_CONCURRENT_CHECKS`], but it is not forcibly
/// cancellable when the HTTP caller stops waiting.
pub type CheckCallback = dyn Fn(&v1::Request) -> CheckDecision + Send + Sync + 'static;

/// Optional bearer credential for the HTTP adapter.
///
/// The token is kept private and this type intentionally does not implement
/// `Debug` or `Display`.
#[derive(Clone)]
pub struct BearerCredential {
    authorization: Arc<[u8]>,
}

/// Sanitized bearer-token configuration error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidBearerCredential;

impl std::fmt::Display for InvalidBearerCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("configured bearer credential is invalid")
    }
}

impl std::error::Error for InvalidBearerCredential {}

impl BearerCredential {
    /// Build an exact `Authorization: Bearer ...` credential.
    pub fn new(token: String) -> Result<Self, InvalidBearerCredential> {
        if token.is_empty() {
            return Err(InvalidBearerCredential);
        }
        let authorization = format!("Bearer {token}");
        if HeaderValue::from_str(&authorization).is_err() {
            return Err(InvalidBearerCredential);
        }
        Ok(Self {
            authorization: Arc::from(authorization.into_bytes()),
        })
    }

    fn authorizes(&self, headers: &HeaderMap) -> bool {
        headers.get(AUTHORIZATION).is_some_and(|provided| {
            bool::from(provided.as_bytes().ct_eq(self.authorization.as_ref()))
        })
    }
}

struct AdapterState {
    callback: Arc<CheckCallback>,
    credential: Option<BearerCredential>,
    permits: Arc<Semaphore>,
    implementation_version: String,
}

/// Build a `POST /check` router over a small synchronous checker callback.
pub fn router(
    callback: Arc<CheckCallback>,
    credential: Option<BearerCredential>,
    implementation_version: String,
) -> Router {
    let state = Arc::new(AdapterState {
        callback,
        credential,
        permits: Arc::new(Semaphore::new(MAX_CONCURRENT_CHECKS)),
        implementation_version,
    });
    Router::new().route("/check", post(check)).with_state(state)
}

async fn check(State(state): State<Arc<AdapterState>>, request: HttpRequest) -> HttpResponse {
    if state
        .credential
        .as_ref()
        .is_some_and(|credential| !credential.authorizes(request.headers()))
    {
        return error_response(StatusCode::UNAUTHORIZED, "unauthorized", true);
    }

    // Admission precedes body buffering so queued clients cannot accumulate
    // unbounded decoded requests. The permit stays with synchronous matching
    // even if the caller stops waiting for the response.
    let permit = match state.permits.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(tokio::sync::TryAcquireError::NoPermits) => {
            return error_response(StatusCode::SERVICE_UNAVAILABLE, "busy", false);
        }
        Err(tokio::sync::TryAcquireError::Closed) => {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", false);
        }
    };

    let body = match tokio::time::timeout(
        REQUEST_BODY_TIMEOUT,
        to_bytes(request.into_body(), v1::MAX_REQUEST_BYTES),
    )
    .await
    {
        Ok(Ok(body)) => body,
        Ok(Err(_)) => {
            return error_response(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", false);
        }
        Err(_) => return error_response(StatusCode::REQUEST_TIMEOUT, "request_timeout", false),
    };
    let invocation = match v1::decode_request(&body) {
        Ok(invocation) => invocation,
        Err(error) => return protocol_error_response(error),
    };
    let invocation_id = invocation.invocation_id.clone();

    let callback = state.callback.clone();
    let decision = match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        callback(&invocation)
    })
    .await
    {
        Ok(decision) => decision,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", false),
    };

    let wire_response = match decision {
        CheckDecision::Allow => {
            v1::Response::allow(invocation_id, Some(state.implementation_version.clone()))
        }
        CheckDecision::Deny { reason_code } => v1::Response::deny(
            invocation_id,
            Some(reason_code),
            Some(state.implementation_version.clone()),
        ),
    };
    let wire_response = match wire_response {
        Ok(response) => response,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", false),
    };
    match v1::encode_response(&wire_response) {
        Ok(body) => (
            StatusCode::OK,
            [(CONTENT_TYPE, HeaderValue::from_static("application/json"))],
            Body::from(body),
        )
            .into_response(),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", false),
    }
}

fn protocol_error_response(error: ProtocolError) -> HttpResponse {
    let status = if error == ProtocolError::RequestTooLarge {
        StatusCode::PAYLOAD_TOO_LARGE
    } else {
        StatusCode::BAD_REQUEST
    };
    error_response(status, error.code(), false)
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

fn error_response(status: StatusCode, code: &'static str, authenticate: bool) -> HttpResponse {
    let mut response = (status, axum::Json(ErrorBody { error: code })).into_response();
    if authenticate {
        response.headers_mut().insert(
            WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"bitrouter-guardrails\""),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_comparison_accepts_only_exact_bearer() -> Result<(), InvalidBearerCredential> {
        assert!(BearerCredential::new(String::new()).is_err());
        assert!(BearerCredential::new("line\nbreak".to_owned()).is_err());
        let credential = BearerCredential::new("private-token".to_owned())?;
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer private-token"),
        );
        assert!(credential.authorizes(&headers));
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer wrong"));
        assert!(!credential.authorizes(&headers));
        Ok(())
    }
}
