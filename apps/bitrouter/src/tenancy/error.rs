//! Stable redacted service failures, independent of backend diagnostics.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bitrouter_ai::tenancy::{
    identity::OperationId,
    operation::{DispatchState, Error, ErrorCode},
};

pub(crate) struct Failure(pub Error);

impl Failure {
    pub fn new(code: ErrorCode, operation: Option<OperationId>) -> Self {
        let message = match code {
            ErrorCode::InvalidRequest => "invalid tenancy request",
            ErrorCode::Unauthorized => "authentication required",
            ErrorCode::Forbidden => "current authority does not permit this operation",
            ErrorCode::TenantInactive => "tenant is inactive",
            ErrorCode::NotFound => "owned resource not found",
            ErrorCode::RevisionConflict => "observed revision does not match",
            ErrorCode::OperationConflict => "operation ID already identifies another command",
            ErrorCode::OperationExpired => "operation result expired; ID remains reserved",
            ErrorCode::PayloadTooLarge => "request exceeds the byte limit",
            ErrorCode::CapacityExceeded => "bounded authority capacity exhausted",
            ErrorCode::AuthorityUnavailable => "current authority is unavailable",
            ErrorCode::StorageUnavailable => "durable authority storage is unavailable",
            ErrorCode::CapabilityMismatch => "installation or dispatch binding does not match",
        };
        Self(Error {
            code,
            message: message.into(),
            dispatch_state: DispatchState::NotStarted,
            operation_id: operation,
            execution_id: None,
        })
    }
}

impl From<ErrorCode> for Failure {
    fn from(code: ErrorCode) -> Self {
        Self::new(code, None)
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let status = match self.0.code {
            ErrorCode::InvalidRequest => StatusCode::BAD_REQUEST,
            ErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
            ErrorCode::Forbidden | ErrorCode::TenantInactive => StatusCode::FORBIDDEN,
            ErrorCode::NotFound => StatusCode::NOT_FOUND,
            ErrorCode::RevisionConflict
            | ErrorCode::OperationConflict
            | ErrorCode::CapabilityMismatch => StatusCode::CONFLICT,
            ErrorCode::OperationExpired => StatusCode::GONE,
            ErrorCode::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ErrorCode::CapacityExceeded => StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::AuthorityUnavailable | ErrorCode::StorageUnavailable => {
                StatusCode::SERVICE_UNAVAILABLE
            }
        };
        (status, Json(self.0)).into_response()
    }
}
